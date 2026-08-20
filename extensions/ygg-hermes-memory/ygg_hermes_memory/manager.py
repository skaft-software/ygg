"""Resident, owner-fenced Hermes MemoryProvider compatibility manager."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
import hashlib
import inspect
import json
import os
import queue
try:
    import resource
except ImportError:  # pragma: no cover - Windows has no resource module
    resource = None
import sys
import threading
import time
from typing import Any, Callable, Deque, Dict, List, Mapping, Optional, Sequence, Tuple

from ygg_extension import (
    CancelledError,
    current_cancellation,
    current_request_id,
    text_content,
    tool_result,
)

from .config import BridgeConfig
from .constants import (
    HERMES_CONTRACT_ID,
    MAX_ACTIVITIES,
    MAX_SESSION_MESSAGE_BYTES,
    MAX_SESSION_MESSAGES,
    MAX_SYSTEM_CONTEXT_BYTES,
    MAX_TOOL_ARGUMENT_BYTES,
    MAX_TURN_TEXT_BYTES,
)
from .credentials import ProviderEnvironmentError, read_provider_environment
from .discovery import DiscoverySnapshot, ProviderCandidate, discover_providers
from .loader import LoadedProvider, load_selected_provider
from .presentation import (
    build_presentation,
    compact_status,
    format_detail,
    format_picker,
    snapshot_json,
)
from .safety import (
    SafetyError,
    fence_memory,
    normalize_tool_schema,
    parse_tool_result,
    provider_reported_write_state,
    redact_secrets,
    safe_error_code,
    safe_error_summary,
    safe_detail,
    safe_identifier,
    safe_label,
    truncate_utf8,
)


@dataclass
class Activity:
    id: str
    kind: str
    state: str
    summary: str
    provenance: str
    started_at_ms: int
    completed_at_ms: Optional[int] = None


@dataclass
class FrozenContext:
    key: Tuple[int, int, str]
    contributions: Tuple[Mapping[str, Any], ...]


@dataclass
class OwnerState:
    key: str
    session_id: str = field(repr=False)
    owner_reference: str
    extension_instance_id: Optional[str] = field(default=None, repr=False)
    process_generation: Optional[int] = None
    selected_id: Optional[str] = None
    inspected_id: str = "off"
    provider_label: str = "Off"
    provider: Optional[LoadedProvider] = field(default=None, repr=False)
    activation: int = 0
    state: str = "off"
    last_error_code: Optional[str] = None
    setup_hint: Optional[str] = None
    tools: Tuple[Mapping[str, Any], ...] = ()
    published_tool_names: Tuple[str, ...] = ()
    static_context: Optional[Mapping[str, Any]] = None
    frozen_context: Optional[FrozenContext] = None
    turn_number: int = 0
    turn_open: bool = False
    turn_id: Optional[str] = None
    user_text: str = field(default="", repr=False)
    assistant_text: Optional[str] = field(default=None, repr=False)
    turn_synced: bool = False
    messages: Deque[Mapping[str, str]] = field(
        default_factory=lambda: deque(maxlen=MAX_SESSION_MESSAGES), repr=False
    )
    queue_depth: int = 0
    last_prefetch: Mapping[str, Any] = field(default_factory=dict)
    last_sync: Mapping[str, Any] = field(default_factory=dict)
    optional_hooks: Tuple[str, ...] = ()
    unsupported_hooks: Tuple[str, ...] = ()
    activities: Deque[Activity] = field(
        default_factory=lambda: deque(maxlen=MAX_ACTIVITIES)
    )
    activity_sequence: int = 0
    last_seen: float = field(default_factory=time.monotonic)


@dataclass(frozen=True)
class BackgroundTask:
    owner_key: str
    activation: int
    kind: str
    payload: Mapping[str, Any]
    activity_id: Optional[str] = None


@dataclass(frozen=True)
class CallOutcome:
    state: str
    value: Any
    duration_ms: int
    error_code: Optional[str] = None


class MemoryBridge:
    """One process, at most one imported provider instance per Ygg owner."""

    def __init__(
        self,
        extension: Any,
        config: BridgeConfig,
        *,
        clock: Optional[Callable[[], int]] = None,
        discovery: Optional[DiscoverySnapshot] = None,
        loader: Callable[..., LoadedProvider] = load_selected_provider,
        config_error_code: Optional[str] = None,
    ) -> None:
        self.extension = extension
        self.config = config
        self._clock = clock or (lambda: int(time.time() * 1000))
        self._loader = loader
        if config_error_code is not None and discovery is None:
            self._discovery = DiscoverySnapshot(
                (),
                "invalid-config",
                None,
                "unavailable",
                safe_identifier(config_error_code, fallback="invalid_config"),
            )
        else:
            self._discovery = discovery or discover_providers(config)
        self._owners: Dict[str, OwnerState] = {}
        self._session_index: Dict[str, str] = {}
        self._runtime_trust: Dict[str, str] = {}
        self._current_owner_key: Optional[str] = None
        self._revision = 0
        self._started = False
        self._accepting = True
        self._shutdown_event = threading.Event()
        self._lock = threading.RLock()
        self._call_lock = threading.Lock()
        self._active_call_threads: Dict[threading.Thread, Tuple[str, str]] = {}
        self._catalog_lock = threading.Lock()
        self._presentation_publish_lock = threading.Lock()
        self._presentation_timer: Optional[threading.Timer] = None
        self._presentation_pending_keys: Deque[Optional[str]] = deque()
        self._presentation_pending_key_set = set()
        self._last_presentation_at = 0.0
        self._catalog_names: Tuple[str, ...] = ()
        self._catalog_owner_key: Optional[str] = None
        self._catalog_candidate_id: Optional[str] = None
        self._provider_environment_loaded = False
        self._provider_environment_keys: Tuple[str, ...] = ()
        self._provider_environment_previous: Dict[str, Optional[str]] = {}
        self._measurement_snapshot = self._measurements()
        queue_capacity = min(256, config.limits.max_queue_depth * config.limits.max_owners)
        self._background_queue = queue.Queue(maxsize=max(1, queue_capacity))
        self._background_thread = threading.Thread(
            target=self._background_loop,
            name="ygg-hermes-memory-background",
            daemon=True,
        )

    # -- Process lifecycle -------------------------------------------------

    def start(self, initialization: Optional[Mapping[str, Any]] = None) -> None:
        """Start the bounded worker after the API 0.2 handshake is flushed."""

        with self._lock:
            if self._started:
                return
            self._started = True
        self._background_thread.start()
        context = {}
        if isinstance(initialization, Mapping):
            host = initialization.get("host")
            if isinstance(host, Mapping):
                context = {"host": dict(host)}
        owner = self.owner_for_context(context)
        # Initialization carries display/session state but not necessarily the
        # complete API 0.2 owner fence. Defer provider import until the first
        # owner-scoped command/hook/context request rather than initialize a
        # backend under a provisional identifier.
        if isinstance(context.get("resource_owner"), Mapping) and self.config.default_provider is not None:
            candidate = self._discovery.by_id(self.config.default_provider)
            if candidate is not None and candidate.trusted_by(self.config, self._runtime_trust):
                self._activate(owner, candidate, cancellation=None, selection_kind="selected")
        self._changed()

    def shutdown(self) -> None:
        """Bounded drain, cancellation, provider shutdown, and state fencing."""

        with self._lock:
            if not self._accepting:
                return
            self._accepting = False
            self._started = False
            owners = list(self._owners.values())
            for owner in owners:
                owner.activation += 1
                owner.state = "stopping"
            self._revision += 1
        self._stop_presentation_publisher()
        self._shutdown_event.set()
        timeout = self.config.limits.shutdown_timeout_ms / 1000.0
        deadline = time.monotonic() + timeout
        drain_deadline = time.monotonic() + timeout / 2.0
        while self._background_queue.unfinished_tasks and time.monotonic() < drain_deadline:
            time.sleep(0.01)
        self._cancel_queued_background()
        if self._background_thread.is_alive():
            self._background_thread.join(timeout=max(0.0, drain_deadline - time.monotonic()))

        shutdown_events = []
        for owner in owners:
            loaded = owner.provider
            if loaded is None:
                continue
            done = threading.Event()

            def stop_provider(provider: Any = loaded.provider, settled: threading.Event = done) -> None:
                try:
                    provider.shutdown()
                except BaseException:
                    pass
                finally:
                    settled.set()

            thread = threading.Thread(
                target=stop_provider,
                name=f"hermes-shutdown-{owner.owner_reference[-8:]}",
                daemon=True,
            )
            shutdown_events.append(done)
            thread.start()
        for done in shutdown_events:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            done.wait(remaining)

        for owner in owners:
            with self._lock:
                owner.provider = None
                owner.tools = ()
                owner.published_tool_names = ()
                owner.static_context = None
                owner.frozen_context = None
                owner.messages.clear()
                owner.user_text = ""
                owner.assistant_text = None
                owner.queue_depth = 0
                owner.state = "stopped"
        self._forget_catalog()
        self._clear_provider_environment()
        self._changed()

    # -- Owner fencing -----------------------------------------------------

    def owner_for_context(
        self,
        context: Optional[Mapping[str, Any]],
        *,
        session_id: Optional[str] = None,
    ) -> OwnerState:
        value = context if isinstance(context, Mapping) else {}
        resource = value.get("resource_owner")
        host = value.get("host")
        resource_session = None
        host_session = None
        instance_id = None
        generation = None
        if isinstance(resource, Mapping):
            candidate_session = resource.get("session_id")
            if isinstance(candidate_session, str) and candidate_session:
                resource_session = candidate_session
            candidate_instance = resource.get("extension_instance_id")
            if isinstance(candidate_instance, str) and candidate_instance:
                instance_id = candidate_instance
            candidate_generation = resource.get("process_generation")
            if (
                isinstance(candidate_generation, int)
                and not isinstance(candidate_generation, bool)
                and candidate_generation >= 0
            ):
                generation = candidate_generation
        if isinstance(host, Mapping):
            candidate_session = host.get("session_id")
            if isinstance(candidate_session, str) and candidate_session:
                host_session = candidate_session
        raw_session = resource_session or session_id or host_session or "headless-unsaved-session"
        raw_session = truncate_utf8(str(raw_session), 1024)[0]
        aliases = []
        for item in (resource_session, session_id, host_session, raw_session):
            if not item:
                continue
            alias = truncate_utf8(str(item), 1024)[0]
            if alias not in aliases:
                aliases.append(alias)
        alias_hashes = [
            hashlib.sha256(item.encode("utf-8")).hexdigest() for item in aliases
        ]
        session_hash = hashlib.sha256(raw_session.encode("utf-8")).hexdigest()

        stale_provider = None
        stale_catalog = False
        with self._lock:
            key = next(
                (
                    self._session_index[alias]
                    for alias in alias_hashes
                    if alias in self._session_index
                ),
                None,
            )
            owner = self._owners.get(key) if key else None
            fences_changed = (
                owner is not None
                and instance_id is not None
                and owner.extension_instance_id is not None
                and (
                    instance_id != owner.extension_instance_id
                    or generation != owner.process_generation
                )
            )
            if fences_changed:
                stale_provider = owner.provider
                stale_catalog = self._catalog_owner_key == owner.key
                owner.activation += 1
                owner.provider = None
                owner.tools = ()
                owner.published_tool_names = ()
                owner.static_context = None
                owner.frozen_context = None
                owner.messages.clear()
                owner.user_text = ""
                owner.assistant_text = None
                owner.state = "stopped"
                owner.last_error_code = "stale_generation_retired"
                self._remove_owner_aliases_locked(owner.key)
                self._owners.pop(owner.key, None)
                key = None
                owner = None
            if owner is None:
                if len(self._owners) >= self.config.limits.max_owners:
                    owner = self._evict_oldest_owner_locked()
                    stale_provider = stale_provider or owner.provider
                    stale_catalog = stale_catalog or self._catalog_owner_key == owner.key
                    self._owners.pop(owner.key, None)
                owner_key_material = (
                    f"{session_hash}\0{instance_id or 'provisional'}\0"
                    f"{generation if generation is not None else 'provisional'}"
                )
                key = hashlib.sha256(owner_key_material.encode("utf-8")).hexdigest()
                reference = f"memory-owner:{key[:32]}"
                owner = OwnerState(
                    key=key,
                    session_id=raw_session,
                    owner_reference=reference,
                    extension_instance_id=instance_id,
                    process_generation=generation,
                )
                self._owners[key] = owner
            elif instance_id is not None and owner.extension_instance_id is None:
                owner.extension_instance_id = instance_id
                owner.process_generation = generation
                if resource_session and owner.provider is None:
                    owner.session_id = truncate_utf8(resource_session, 1024)[0]
            for alias in alias_hashes:
                self._session_index[alias] = owner.key
            owner.last_seen = time.monotonic()
            self._current_owner_key = owner.key
        if stale_catalog:
            self._clear_catalog()
        if stale_provider is not None:
            self._shutdown_loaded_async(stale_provider)
        return owner

    def _evict_oldest_owner_locked(self) -> OwnerState:
        owner = min(self._owners.values(), key=lambda item: item.last_seen)
        self._remove_owner_aliases_locked(owner.key)
        owner.activation += 1
        owner.state = "stopped"
        return owner

    def _remove_owner_aliases_locked(self, owner_key: str) -> None:
        for alias, mapped in list(self._session_index.items()):
            if mapped == owner_key:
                self._session_index.pop(alias, None)

    # -- Discovery, trust, commands ---------------------------------------

    def execute_command(
        self,
        arguments: Sequence[Any],
        context: Optional[Mapping[str, Any]],
    ) -> Mapping[str, Any]:
        owner = self.owner_for_context(context)
        args = [str(item) for item in arguments[:4]]
        if any(len(item.encode("utf-8", errors="replace")) > 4096 for item in args):
            return {"text": "Memory command arguments exceed the bounded input limit."}
        command = args[0].lower() if args else "picker"

        if command == "picker":
            # Re-publish the complete semantic collection so interactive hosts
            # can focus their generic inspector; the returned text is the
            # narrow/headless fallback. No provider operation is repeated.
            self._changed(owner)
            return {"text": format_picker(self._discovery_view(), self._owner_view(owner))}
        if command in {"list", "status"}:
            return {"text": format_picker(self._discovery_view(), self._owner_view(owner))}
        if command == "snapshot":
            return {"text": snapshot_json(self.presentation_snapshot(owner))}
        if command == "show" and len(args) == 2:
            candidate = self._discovery.by_id(args[1])
            with self._lock:
                owner.inspected_id = candidate.id if candidate else "off"
            self._changed(owner)
            return {
                "text": format_detail(
                    self._candidate_view(candidate) if candidate else None,
                    self._owner_view(owner),
                    self._discovery_view(),
                )
            }
        if command == "trust" and len(args) == 3:
            return {"text": self._trust_candidate(owner, args[1], args[2])}
        if command == "select" and len(args) == 2:
            candidate = self._discovery.by_id(args[1])
            if candidate is None:
                return {"text": "Unknown memory provider id."}
            if not candidate.trusted_by(self.config, self._runtime_trust):
                return {"text": "Provider is not trusted. Review its fingerprint and use /memory trust first."}
            if owner.turn_open:
                return {"text": "Provider selection is allowed only at an idle turn boundary."}
            self._activate(owner, candidate, cancellation=current_cancellation(), selection_kind="selected")
            return {"text": compact_status(self._owner_view(owner))}
        if command == "off":
            if owner.turn_open:
                return {"text": "Memory can be disabled only at an idle turn boundary."}
            self._disable(owner, activity_kind="disabled")
            return {"text": "memory off"}
        if command in {"reload", "retry"}:
            if owner.turn_open:
                return {"text": "Provider reload is allowed only at an idle turn boundary."}
            candidate = self._discovery.by_id(owner.selected_id or "")
            if candidate is None:
                return {"text": "No selected provider to reload."}
            if not candidate.trusted_by(self.config, self._runtime_trust):
                return {"text": "The selected provider fingerprint is no longer trusted."}
            self._activate(owner, candidate, cancellation=current_cancellation(), selection_kind="reloaded")
            return {"text": compact_status(self._owner_view(owner))}
        if command == "discover":
            self.refresh_discovery(owner)
            return {"text": format_picker(self._discovery_view(), self._owner_view(owner))}
        if command == "lifecycle":
            return {"text": self._lifecycle_report(owner)}
        return {
            "text": (
                "Usage: /memory [status|list|snapshot|show ID|trust ID FINGERPRINT|"
                "select ID|off|retry|reload|discover|lifecycle]"
            )
        }

    def refresh_discovery(self, owner: Optional[OwnerState] = None) -> None:
        snapshot = discover_providers(self.config)
        with self._lock:
            self._discovery = snapshot
            affected = list(self._owners.values())
            for item in affected:
                if item.selected_id:
                    candidate = snapshot.by_id(item.selected_id)
                    if candidate is None or candidate.fingerprint is None:
                        item.state = "degraded"
                        item.last_error_code = "selected_provider_not_discovered"
                    elif item.provider is not None and not candidate.trusted_by(
                        self.config, self._runtime_trust
                    ):
                        item.state = "degraded"
                        item.last_error_code = "selected_provider_fingerprint_changed"
        if owner is not None and all(item is not owner for item in affected):
            affected.append(owner)
        for item in affected:
            self._changed(item)

    def _trust_candidate(self, owner: OwnerState, candidate_id: str, fingerprint: str) -> str:
        candidate = self._discovery.by_id(candidate_id)
        if candidate is None or candidate.fingerprint is None:
            return "Unknown or unavailable memory provider id."
        if fingerprint != candidate.fingerprint:
            return "Provider fingerprint did not match current metadata; nothing was trusted."
        with self._lock:
            self._runtime_trust[candidate.id] = fingerprint
            owner.inspected_id = candidate.id
        self._add_activity(
            owner,
            "memory_provider",
            "active",
            "Memory provider trusted for this process",
            f"{candidate.label} · metadata fingerprint verified · code not initialized",
            terminal=True,
        )
        with self._lock:
            affected = list(self._owners.values())
        for item in affected:
            self._changed(item)
        return "Provider trusted for this extension process only; it has not been imported or initialized."

    # -- Selection and dynamic catalog ------------------------------------

    def _activate(
        self,
        owner: OwnerState,
        candidate: ProviderCandidate,
        *,
        cancellation: Any,
        selection_kind: str,
        preserve_turn: bool = False,
    ) -> None:
        with self._lock:
            if not self._accepting:
                return
            old_provider = owner.provider
            old_messages = list(owner.messages)
            old_id = owner.selected_id
            owner.activation += 1
            activation = owner.activation
            owner.selected_id = candidate.id
            owner.inspected_id = candidate.id
            owner.provider_label = candidate.label
            owner.provider = None
            owner.tools = ()
            owner.published_tool_names = ()
            owner.static_context = None
            owner.frozen_context = None
            if not preserve_turn:
                owner.messages.clear()
                owner.user_text = ""
                owner.assistant_text = None
                owner.turn_synced = False
            owner.optional_hooks = ()
            owner.unsupported_hooks = ()
            owner.state = "loading"
            owner.last_error_code = None
            owner.setup_hint = None
        self._changed(owner)
        if old_provider is not None:
            if "on_session_end" in old_provider.optional_hooks and old_messages:
                self._call_bounded(
                    owner,
                    "on_session_end",
                    lambda: old_provider.provider.on_session_end(old_messages),
                    self.config.limits.sync_timeout_ms,
                    cancellation=None,
                )
            self._call_bounded(
                owner,
                "shutdown",
                old_provider.provider.shutdown,
                self.config.limits.shutdown_timeout_ms,
                cancellation=None,
            )
        if self._catalog_owner_key == owner.key:
            # A failed replacement must never leave handlers for a provider
            # that has already been shut down.
            self._clear_catalog()

        expected = self._runtime_trust.get(candidate.id) or self.config.trusted_fingerprint(candidate.id)
        if expected != candidate.fingerprint or expected is None:
            self._activation_failed(owner, activation, "provider_not_trusted")
            return
        try:
            self._ensure_provider_environment()
        except ProviderEnvironmentError as error:
            self._activation_failed(owner, activation, str(error))
            return
        outcome = self._call_bounded(
            owner,
            "load",
            lambda: self._loader(candidate, self.config, expected_fingerprint=expected),
            self.config.limits.initialize_timeout_ms,
            cancellation=cancellation,
        )
        if not self._accept_call_outcome(owner, activation, outcome, "provider_load"):
            if isinstance(outcome.value, LoadedProvider):
                self._shutdown_loaded_async(outcome.value)
            return
        loaded = outcome.value

        available = self._call_bounded(
            owner,
            "is_available",
            loaded.provider.is_available,
            self.config.limits.availability_timeout_ms,
            cancellation=cancellation,
        )
        if not self._accept_call_outcome(owner, activation, available, "provider_availability"):
            self._shutdown_loaded_async(loaded)
            return
        if available.value is not True:
            reason_method = getattr(loaded.provider, "unavailable_reason", None)
            if callable(reason_method):
                reason = self._call_bounded(
                    owner,
                    "unavailable_reason",
                    reason_method,
                    min(500, self.config.limits.availability_timeout_ms),
                    cancellation=cancellation,
                )
                if reason.state == "succeeded" and isinstance(reason.value, str) and reason.value.strip():
                    with self._lock:
                        owner.setup_hint = safe_detail(reason.value, maximum=512)
            self._activation_failed(owner, activation, "provider_reported_unavailable", state="unavailable")
            self._shutdown_loaded_async(loaded)
            return

        environment = self.config.environment
        if environment is None or environment.hermes_home is None:
            self._activation_failed(owner, activation, "hermes_home_not_configured")
            self._shutdown_loaded_async(loaded)
            return
        workspace = os.environ.get("YGG_WORKSPACE", "workspace")
        workspace_name = safe_label(os.path.basename(workspace) or "workspace", maximum=128)
        initialized = self._call_bounded(
            owner,
            "initialize",
            lambda: loaded.provider.initialize(
                owner.session_id,
                hermes_home=str(environment.hermes_home),
                platform="ygg",
                agent_context="primary",
                agent_identity=environment.id,
                agent_workspace=workspace_name,
            ),
            self.config.limits.initialize_timeout_ms,
            cancellation=cancellation,
        )
        if not self._accept_call_outcome(owner, activation, initialized, "provider_initialize"):
            self._shutdown_loaded_async(loaded)
            return

        static_context = None
        degraded_codes = []
        if "system_prompt_block" in loaded.optional_hooks:
            static = self._call_bounded(
                owner,
                "system_prompt_block",
                loaded.provider.system_prompt_block,
                self.config.limits.prefetch_timeout_ms,
                cancellation=cancellation,
            )
            if static.state == "succeeded" and static.value:
                try:
                    fenced, original_bytes, truncated = fence_memory(
                        static.value,
                        provider=candidate.label,
                        source="system_prompt_block",
                        maximum=min(self.config.limits.max_context_bytes, MAX_SYSTEM_CONTEXT_BYTES),
                    )
                    if fenced:
                        static_context = {
                            "label": f"hermes-memory.{safe_identifier(candidate.name)}.system",
                            "content": fenced,
                            "placement": "system_suffix",
                            "bytes": len(fenced.encode("utf-8")),
                            "originalBytes": original_bytes,
                            "truncated": truncated,
                        }
                except SafetyError:
                    degraded_codes.append("system_prompt_block_malformed")
            elif static.state != "succeeded":
                degraded_codes.append(f"system_prompt_block_{static.state}")

        schemas = self._call_bounded(
            owner,
            "get_tool_schemas",
            loaded.provider.get_tool_schemas,
            self.config.limits.initialize_timeout_ms,
            cancellation=cancellation,
        )
        tools = []
        if schemas.state != "succeeded" or not isinstance(schemas.value, list):
            degraded_codes.append("tool_schema_collection_failed")
        else:
            if len(schemas.value) > self.config.limits.max_tools:
                degraded_codes.append("tool_schema_limit_exceeded")
            for raw in schemas.value[: self.config.limits.max_tools]:
                try:
                    normalized = normalize_tool_schema(raw)
                except Exception:
                    degraded_codes.append("malformed_tool_schema")
                    continue
                if any(item["name"] == normalized["name"] for item in tools):
                    degraded_codes.append("duplicate_tool_schema")
                    continue
                tools.append(normalized)

        with self._lock:
            if owner.activation != activation or owner.selected_id != candidate.id:
                self._shutdown_loaded_async(loaded)
                return
            owner.provider = loaded
            owner.tools = tuple(tools)
            owner.static_context = static_context
            owner.optional_hooks = loaded.optional_hooks
            unsupported = list(loaded.unsupported_hooks)
            unsupported.extend(
                [
                    "on_pre_compress:no_api_boundary",
                    "on_delegation:no_api_boundary",
                    "backup_paths:not_exposed",
                    "provider_skills:not_exposed",
                ]
            )
            if loaded.ignored_registrations:
                unsupported.append("secondary_registrations_ignored")
            owner.unsupported_hooks = tuple(unsupported)
            owner.state = "degraded" if degraded_codes else "active"
            owner.last_error_code = degraded_codes[0] if degraded_codes else None
        catalog_ok = self._publish_catalog(owner, candidate)
        if not catalog_ok:
            with self._lock:
                owner.state = "degraded"
                owner.last_error_code = "dynamic_tool_publication_failed"
        activity_word = "switched" if old_id and old_id != candidate.id else selection_kind
        self._add_activity(
            owner,
            "memory_provider",
            "succeeded" if owner.state == "active" else "degraded",
            f"Memory provider {activity_word}",
            (
                f"{candidate.label} · {candidate.version} · contract {HERMES_CONTRACT_ID} · "
                f"{len(owner.published_tool_names)}/{len(tools)} tools published · environment {candidate.environment_id}"
            ),
            terminal=True,
        )
        self._changed(owner)

    def _accept_call_outcome(
        self,
        owner: OwnerState,
        activation: int,
        outcome: CallOutcome,
        prefix: str,
    ) -> bool:
        if outcome.state == "succeeded":
            with self._lock:
                return self._accepting and owner.activation == activation
        code = outcome.error_code or f"{prefix}_{outcome.state}"
        self._activation_failed(owner, activation, code)
        if outcome.state == "cancelled":
            raise CancelledError("memory provider activation cancelled")
        return False

    def _activation_failed(
        self,
        owner: OwnerState,
        activation: int,
        code: str,
        *,
        state: str = "unavailable",
    ) -> None:
        with self._lock:
            if owner.activation != activation:
                return
            owner.provider = None
            owner.tools = ()
            owner.published_tool_names = ()
            owner.static_context = None
            owner.frozen_context = None
            owner.state = state
            owner.last_error_code = safe_identifier(code, fallback="provider_error")
        self._add_activity(
            owner,
            "memory_provider",
            "unavailable" if state == "unavailable" else "failed",
            "Memory provider unavailable",
            f"{owner.provider_label} · {safe_error_summary(code)}",
            terminal=True,
        )
        self._changed(owner)

    def _disable(self, owner: OwnerState, *, activity_kind: str) -> None:
        with self._lock:
            loaded = owner.provider
            closing_messages = list(owner.messages)
            owner.activation += 1
            owner.provider = None
            owner.selected_id = None
            owner.inspected_id = "off"
            owner.provider_label = "Off"
            owner.tools = ()
            owner.published_tool_names = ()
            owner.static_context = None
            owner.frozen_context = None
            owner.messages.clear()
            owner.user_text = ""
            owner.assistant_text = None
            owner.state = "off"
            owner.last_error_code = None
            owner.setup_hint = None
        if loaded is not None:
            if "on_session_end" in loaded.optional_hooks and closing_messages:
                self._call_bounded(
                    owner,
                    "on_session_end",
                    lambda: loaded.provider.on_session_end(closing_messages),
                    self.config.limits.sync_timeout_ms,
                    cancellation=None,
                )
            self._call_bounded(
                owner,
                "shutdown",
                loaded.provider.shutdown,
                self.config.limits.shutdown_timeout_ms,
                cancellation=None,
            )
        if self._catalog_owner_key == owner.key:
            self._clear_catalog()
        self._add_activity(
            owner,
            "memory_provider",
            "stopped",
            "Memory provider disabled",
            f"owner fenced · {activity_kind}",
            terminal=True,
        )
        self._changed(owner)

    def _publish_catalog(self, owner: OwnerState, candidate: ProviderCandidate) -> bool:
        definitions = []
        for tool in owner.tools:
            name = tool["name"]

            def handler(
                arguments: Mapping[str, Any],
                context: Mapping[str, Any],
                *,
                expected_candidate: str = candidate.id,
                expected_tool: str = name,
            ) -> Mapping[str, Any]:
                return self.call_provider_tool(
                    expected_candidate,
                    expected_tool,
                    arguments,
                    context,
                )

            definitions.append(
                {
                    "name": name,
                    "description": tool["description"],
                    "parameters": tool["parameters"],
                    "handler": handler,
                }
            )
        new_names = tuple(item["name"] for item in definitions)
        with self._catalog_lock:
            previous = self._catalog_names
            host_names = set(previous)
            try:
                if definitions:
                    response = self.extension.register_tools(definitions)
                    host_names = set(response.get("tools", []))
                removed = tuple(name for name in previous if name not in new_names)
                if removed:
                    response = self.extension.unregister_tools(*removed)
                    host_names = set(response.get("tools", []))
            except Exception:
                return False
            accepted = tuple(name for name in new_names if name in host_names)
            owner.published_tool_names = accepted
            self._catalog_names = tuple(sorted(host_names))
            self._catalog_owner_key = owner.key
            self._catalog_candidate_id = candidate.id
            return True

    def _focus_catalog(self, owner: OwnerState) -> None:
        if owner.provider is None or owner.selected_id is None:
            return
        if (
            self._catalog_owner_key == owner.key
            and self._catalog_candidate_id == owner.selected_id
            and self._catalog_names == tuple(sorted(owner.published_tool_names))
        ):
            return
        candidate = self._discovery.by_id(owner.selected_id)
        if candidate is not None and not self._publish_catalog(owner, candidate):
            with self._lock:
                owner.state = "degraded"
                owner.last_error_code = "dynamic_tool_publication_failed"
            self._changed(owner)

    def _clear_catalog(self) -> None:
        with self._catalog_lock:
            names = self._catalog_names
            try:
                if names:
                    self.extension.unregister_tools(*names)
            except Exception:
                pass
            self._catalog_names = ()
            self._catalog_owner_key = None
            self._catalog_candidate_id = None

    def _forget_catalog(self) -> None:
        """Drop local catalog state when process shutdown makes host cleanup authoritative."""

        with self._catalog_lock:
            self._catalog_names = ()
            self._catalog_owner_key = None
            self._catalog_candidate_id = None

    # -- Prompt context and tools -----------------------------------------

    def collect_context(
        self,
        params: Mapping[str, Any],
        context: Optional[Mapping[str, Any]],
    ) -> List[Mapping[str, Any]]:
        owner = self.owner_for_context(context)
        self._ensure_active_default(owner)
        if owner.provider is None or owner.selected_id is None:
            return []
        self._focus_catalog(owner)
        prompt = params.get("prompt") if isinstance(params, Mapping) else None
        if not isinstance(prompt, str):
            prompt = owner.user_text
        prompt = truncate_utf8(redact_secrets(prompt), self.config.limits.max_query_bytes)[0]
        prompt_digest = hashlib.sha256(prompt.encode("utf-8")).hexdigest()
        key = (owner.activation, owner.turn_number, prompt_digest)
        with self._lock:
            frozen = owner.frozen_context
            if frozen is not None and frozen.key == key:
                owner.last_prefetch = dict(owner.last_prefetch, cache="hit")
                return [dict(item) for item in frozen.contributions]
            loaded = owner.provider
            candidate = self._discovery.by_id(owner.selected_id)
        if loaded is None or candidate is None:
            return []

        started = self._clock()
        contributions: List[Mapping[str, Any]] = []
        byte_count = 0
        item_count = 0
        truncated = False
        sources = []
        if owner.static_context:
            static = {key: value for key, value in owner.static_context.items() if key in {"label", "content", "placement"}}
            contributions.append(static)
            byte_count += len(str(static["content"]).encode("utf-8"))
            item_count += 1
            truncated = bool(owner.static_context.get("truncated"))
            sources.append("system_prompt_block")

        prefetch_outcome = "empty"
        if prompt and "prefetch" in loaded.optional_hooks:
            outcome = self._call_bounded(
                owner,
                "prefetch",
                lambda: loaded.provider.prefetch(prompt, session_id=owner.session_id),
                self.config.limits.prefetch_timeout_ms,
                cancellation=current_cancellation(),
            )
            if outcome.state == "cancelled":
                self._finish_read_activity(
                    owner,
                    candidate,
                    started,
                    "cancelled",
                    byte_count,
                    item_count,
                    truncated,
                    sources or ["prefetch"],
                    "miss",
                )
                raise CancelledError("memory prefetch cancelled")
            if outcome.state == "succeeded" and outcome.value:
                try:
                    remaining_context = max(0, self.config.limits.max_context_bytes - byte_count)
                    if remaining_context <= 256:
                        raise SafetyError("aggregate memory context limit exhausted")
                    fenced, _, was_truncated = fence_memory(
                        outcome.value,
                        provider=candidate.label,
                        source="prefetch",
                        maximum=remaining_context,
                    )
                    if fenced:
                        contributions.append(
                            {
                                "label": f"hermes-memory.{safe_identifier(candidate.name)}.prefetch",
                                "content": fenced,
                                "placement": "prompt_prefix",
                            }
                        )
                        bytes_added = len(fenced.encode("utf-8"))
                        byte_count += bytes_added
                        item_count += self._recall_count(owner, loaded, default=1)
                        truncated = truncated or was_truncated
                        sources.append("prefetch")
                        prefetch_outcome = "success"
                except SafetyError:
                    prefetch_outcome = "malformed"
                    sources.append("prefetch")
                    with self._lock:
                        owner.state = "degraded"
                        owner.last_error_code = "prefetch_malformed"
            elif outcome.state == "succeeded":
                prefetch_outcome = "empty"
            else:
                prefetch_outcome = outcome.state
                sources.append("prefetch")
                with self._lock:
                    owner.state = "degraded"
                    owner.last_error_code = outcome.error_code or f"prefetch_{outcome.state}"
        prefetch_failed = prefetch_outcome not in {"empty", "success"}
        outcome_name = "degraded" if contributions and prefetch_failed else "success" if contributions else prefetch_outcome
        with self._lock:
            owner.frozen_context = FrozenContext(key, tuple(dict(item) for item in contributions))
            owner.last_prefetch = {
                "outcome": outcome_name,
                "cache": "miss",
                "bytes": byte_count,
                "items": item_count,
                "truncated": truncated,
                "latencyMs": max(0, self._clock() - started),
            }
        if contributions or prefetch_outcome not in {"empty", "success"}:
            self._finish_read_activity(
                owner,
                candidate,
                started,
                "degraded" if contributions and prefetch_failed else "succeeded" if contributions else "failed",
                byte_count,
                item_count,
                truncated,
                sources or ["prefetch"],
                "miss",
            )
        self._changed(owner)
        return [dict(item) for item in contributions]

    def call_provider_tool(
        self,
        expected_candidate: str,
        tool_name: str,
        arguments: Mapping[str, Any],
        context: Optional[Mapping[str, Any]],
    ) -> Mapping[str, Any]:
        owner = self.owner_for_context(context)
        with self._lock:
            loaded = owner.provider
            selected_id = owner.selected_id
            candidate = self._discovery.by_id(selected_id or "")
        if loaded is None or candidate is None or selected_id != expected_candidate:
            return tool_result(
                text_content("Memory provider selection changed; the stale call was not replayed."),
                is_error=True,
                metadata={"outcome": "stale_provider_fence"},
            )
        if not isinstance(arguments, Mapping):
            return tool_result(text_content("Memory tool arguments must be an object."), is_error=True)
        try:
            encoded_arguments = json.dumps(arguments, separators=(",", ":"), allow_nan=False).encode("utf-8")
        except (TypeError, ValueError, RecursionError):
            return tool_result(text_content("Memory tool arguments are not strict JSON."), is_error=True)
        if len(encoded_arguments) > MAX_TOOL_ARGUMENT_BYTES:
            return tool_result(text_content("Memory tool arguments exceed the bridge limit."), is_error=True)

        operation = self._tool_operation(candidate, tool_name)
        activity_kind = "memory_write" if operation == "write" else "memory_read" if operation == "read" else "memory_tool"
        activity_id = self._add_activity(
            owner,
            activity_kind,
            "running",
            "Memory write" if operation == "write" else "Memory read" if operation == "read" else "Memory provider tool",
            f"{candidate.label} · tool {tool_name} · {len(encoded_arguments)} argument bytes · running",
            terminal=False,
        )
        started = self._clock()
        outcome = self._call_bounded(
            owner,
            f"tool_{safe_identifier(tool_name)}",
            lambda: loaded.provider.handle_tool_call(
                tool_name,
                dict(arguments),
                session_id=owner.session_id,
                platform="ygg",
            ),
            self.config.limits.tool_timeout_ms,
            cancellation=current_cancellation(),
        )
        latency = max(0, self._clock() - started)
        if outcome.state == "cancelled":
            self._update_activity(
                owner,
                activity_id,
                "cancelled",
                "Memory write cancelled" if operation == "write" else "Memory read cancelled",
                f"{candidate.label} · cancellation requested · durability ambiguous · {latency} ms",
            )
            self._changed(owner)
            raise CancelledError("memory provider tool cancelled")
        if outcome.state != "succeeded":
            self._update_activity(
                owner,
                activity_id,
                "failed",
                "Memory write failed" if operation == "write" else "Memory read failed",
                f"{candidate.label} · {outcome.state} · no replay · {latency} ms",
            )
            with self._lock:
                owner.state = "degraded"
                owner.last_error_code = outcome.error_code or f"tool_{outcome.state}"
            self._changed(owner)
            return tool_result(
                text_content("Hermes memory provider tool failed; direct coding remains available."),
                is_error=True,
                metadata={"provider": candidate.id, "outcome": outcome.state},
            )
        try:
            visible, parsed, result_bytes, was_truncated = parse_tool_result(
                outcome.value, self.config.limits.max_tool_result_bytes
            )
            fenced, _, fence_truncated = fence_memory(
                visible,
                provider=candidate.label,
                source=f"tool:{tool_name}",
                maximum=self.config.limits.max_tool_result_bytes,
            )
        except SafetyError:
            self._update_activity(
                owner,
                activity_id,
                "failed",
                "Memory provider result rejected",
                f"{candidate.label} · malformed or oversized result · {latency} ms",
            )
            self._changed(owner)
            return tool_result(
                text_content("Hermes memory provider returned a malformed or oversized result."),
                is_error=True,
                metadata={"provider": candidate.id, "outcome": "malformed_result"},
            )

        durability = provider_reported_write_state(parsed) if operation == "write" else "not_applicable"
        if operation == "write":
            state_map = {
                "committed": ("succeeded", "Memory write committed"),
                "queued": ("pending", "Memory write queued"),
                "failed": ("failed", "Memory write failed"),
                "cancelled": ("cancelled", "Memory write cancelled"),
                "unreported": ("degraded", "Memory write durability unreported"),
            }
            presentation_state, summary = state_map[durability]
        else:
            presentation_state, summary = "succeeded", "Memory read"
        self._update_activity(
            owner,
            activity_id,
            presentation_state,
            summary,
            (
                f"{candidate.label} · tool {tool_name} · 1 item · {result_bytes} bytes · "
                f"{durability if operation == 'write' else 'success'} · {latency} ms · "
                f"{'truncated' if was_truncated or fence_truncated else 'complete'}"
            ),
        )
        self._changed(owner)
        return tool_result(
            text_content(fenced),
            metadata={
                "provider": candidate.id,
                "operation": operation,
                "durability": durability,
                "bytes": result_bytes,
                "latency_ms": latency,
                "truncated": bool(was_truncated or fence_truncated),
            },
        )

    # -- Hooks and lifecycle ----------------------------------------------

    def before_prompt(
        self,
        payload: Mapping[str, Any],
        context: Optional[Mapping[str, Any]],
    ) -> Mapping[str, Any]:
        owner = self.owner_for_context(context)
        self._ensure_active_default(owner)
        prompt = payload.get("prompt") if isinstance(payload, Mapping) else ""
        if not isinstance(prompt, str):
            prompt = ""
        prompt = truncate_utf8(redact_secrets(prompt), MAX_TURN_TEXT_BYTES)[0]
        with self._lock:
            same_epoch = owner.turn_open and owner.user_text == prompt
            if not same_epoch:
                owner.turn_number += 1
                owner.turn_open = True
                owner.turn_id = None
                owner.user_text = prompt
                owner.assistant_text = None
                owner.turn_synced = False
                owner.frozen_context = None
            loaded = owner.provider
            turn_number = owner.turn_number
        if not same_epoch and loaded is not None and "on_turn_start" in loaded.optional_hooks:
            outcome = self._call_bounded(
                owner,
                "on_turn_start",
                lambda: loaded.provider.on_turn_start(
                    turn_number,
                    prompt,
                    platform="ygg",
                ),
                min(1000, self.config.limits.prefetch_timeout_ms),
                cancellation=current_cancellation(),
            )
            if outcome.state not in {"succeeded", "cancelled"}:
                with self._lock:
                    owner.state = "degraded"
                    owner.last_error_code = outcome.error_code or "on_turn_start_failed"
        return {"disposition": {"action": "continue"}, "context": [], "notifications": []}

    def after_response(
        self,
        payload: Mapping[str, Any],
        context: Optional[Mapping[str, Any]],
    ) -> Mapping[str, Any]:
        """Capture the successful assistant response for ``sync_turn``.

        API 0.2 delivers this success-only hook in addition to terminal
        lifecycle observations. Failure/cancellation cleanup remains driven by
        ``turn/settled`` so no response is invented on an unsuccessful turn.
        """

        owner = self.owner_for_context(context)
        response = payload.get("response") if isinstance(payload, Mapping) else ""
        if not isinstance(response, str):
            response = ""
        response = truncate_utf8(redact_secrets(response), MAX_TURN_TEXT_BYTES)[0]
        with self._lock:
            owner.assistant_text = response
        self._queue_turn_sync(owner)
        return {"disposition": {"action": "continue"}, "context": [], "notifications": []}

    def after_tool_call(
        self,
        payload: Mapping[str, Any],
        context: Optional[Mapping[str, Any]],
    ) -> Mapping[str, Any]:
        owner = self.owner_for_context(context)
        if not isinstance(payload, Mapping) or payload.get("name") != "memory":
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        with self._lock:
            loaded = owner.provider
        if loaded is None or "on_memory_write" not in loaded.optional_hooks:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        arguments = payload.get("arguments")
        if not isinstance(arguments, Mapping) or payload.get("is_error") is True:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        output = payload.get("output")
        try:
            parsed_output = json.loads(output) if isinstance(output, str) else output
        except (ValueError, TypeError, RecursionError):
            parsed_output = None
        if not isinstance(parsed_output, Mapping) or parsed_output.get("success") is not True or parsed_output.get("staged") is True:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        action = arguments.get("action")
        if action not in {"add", "replace", "remove"}:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        target = arguments.get("target", "memory")
        if target not in {"memory", "user"}:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        content = arguments.get("content", "")
        if not isinstance(content, str):
            content = ""
        content = truncate_utf8(redact_secrets(content), MAX_TURN_TEXT_BYTES)[0]
        activity_id = self._add_activity(
            owner,
            "memory_write",
            "pending",
            "Memory write queued",
            f"{owner.provider_label} · built-in memory hook · {action} · durability unreported",
            terminal=False,
        )
        self._enqueue_background(
            owner,
            "memory_write",
            {"action": action, "target": target, "content": content},
            activity_id=activity_id,
        )
        return {"disposition": {"action": "continue"}, "context": [], "notifications": []}

    def lifecycle(self, method: str, event: Mapping[str, Any]) -> None:
        session_id = event.get("session_id") if isinstance(event, Mapping) else None
        owner = self.owner_for_context({}, session_id=session_id if isinstance(session_id, str) else None)
        if method == "turn/started":
            turn_id = event.get("turn_id")
            with self._lock:
                if not owner.turn_open:
                    owner.turn_number += 1
                    owner.turn_open = True
                    owner.user_text = ""
                    owner.assistant_text = None
                    owner.turn_synced = False
                    owner.frozen_context = None
                owner.turn_id = str(turn_id)[:256] if turn_id is not None else None
            return
        if method == "turn/settled":
            outcome = event.get("outcome")
            if outcome == "completed":
                if owner.assistant_text is not None:
                    self._queue_turn_sync(owner)
                elif owner.provider is not None and "sync_turn" in owner.optional_hooks:
                    with self._lock:
                        owner.last_sync = {
                            "outcome": "unsupported_boundary",
                            "reason": "assistant_text_unavailable",
                        }
                        owner.state = "degraded"
                        owner.last_error_code = "sync_turn_assistant_text_unavailable"
                    self._add_activity(
                        owner,
                        "memory_sync",
                        "degraded",
                        "Memory sync skipped",
                        f"{owner.provider_label} · API 0.2 lifecycle omitted assistant text · no transcript guessed",
                        terminal=True,
                    )
                if owner.provider is not None and "queue_prefetch" in owner.optional_hooks and owner.user_text:
                    self._enqueue_background(owner, "queue_prefetch", {"query": owner.user_text})
            else:
                self._add_activity(
                    owner,
                    "memory_sync",
                    "cancelled" if outcome in {"cancelled", "interrupted", "shutdown"} else "failed",
                    "Memory sync not attempted",
                    f"{owner.provider_label} · turn {safe_identifier(outcome, fallback='failed')} · Ygg lifecycle authoritative",
                    terminal=True,
                )
            with self._lock:
                owner.turn_open = False
                owner.frozen_context = None
            self._changed(owner)
            return
        if method == "session/settled":
            self._settle_owner(owner)
            return

    def _settle_owner(self, owner: OwnerState) -> None:
        """Fence immediately and run terminal hooks outside the ordinary queue."""

        with self._lock:
            loaded = owner.provider
            if loaded is None:
                return
            provider = loaded.provider
            call_session_end = "on_session_end" in owner.optional_hooks
            messages = list(owner.messages)
            owner.activation += 1
            activation = owner.activation
            owner.provider = None
            owner.tools = ()
            owner.published_tool_names = ()
            owner.static_context = None
            owner.frozen_context = None
            owner.messages.clear()
            owner.user_text = ""
            owner.assistant_text = None
            owner.state = "stopping"
            clear_catalog = self._catalog_owner_key == owner.key
        if clear_catalog:
            self._clear_catalog()
        self._changed(owner)

        def settle() -> None:
            session_end_outcome = None
            if call_session_end:
                session_end_outcome = self._call_bounded(
                    owner,
                    "session_end",
                    lambda: provider.on_session_end(messages),
                    self.config.limits.sync_timeout_ms,
                    cancellation=None,
                    ignore_shutdown=True,
                    terminal=True,
                )
            shutdown_outcome = self._call_bounded(
                owner,
                "settle_owner",
                provider.shutdown,
                self.config.limits.shutdown_timeout_ms,
                cancellation=None,
                ignore_shutdown=True,
                terminal=True,
            )
            with self._lock:
                if owner.activation != activation:
                    return
                owner.state = "stopped"
                failed = shutdown_outcome.state != "succeeded" or (
                    session_end_outcome is not None
                    and session_end_outcome.state != "succeeded"
                )
                if failed:
                    owner.last_error_code = (
                        shutdown_outcome.error_code
                        or (
                            session_end_outcome.error_code
                            if session_end_outcome is not None
                            else None
                        )
                        or "session_settlement_failed"
                    )
            self._changed(owner)

        threading.Thread(
            target=settle,
            name=f"hermes-settle-{owner.owner_reference[-8:]}",
            daemon=True,
        ).start()

    # -- Background lifecycle mappings -----------------------------------

    def _queue_turn_sync(self, owner: OwnerState) -> None:
        with self._lock:
            if owner.turn_synced or owner.provider is None or owner.assistant_text is None:
                return
            if "sync_turn" not in owner.optional_hooks:
                owner.turn_synced = True
                return
            user = owner.user_text
            assistant = owner.assistant_text
            owner.turn_synced = True
            if user:
                owner.messages.append({"role": "user", "content": truncate_utf8(user, MAX_SESSION_MESSAGE_BYTES)[0]})
            if assistant:
                owner.messages.append({"role": "assistant", "content": truncate_utf8(assistant, MAX_SESSION_MESSAGE_BYTES)[0]})
            messages = list(owner.messages)
        activity_id = self._add_activity(
            owner,
            "memory_sync",
            "pending",
            "Memory sync queued",
            f"{owner.provider_label} · completed turn · {len(user.encode('utf-8')) + len(assistant.encode('utf-8'))} bytes · queue {owner.queue_depth + 1}",
            terminal=False,
        )
        self._enqueue_background(
            owner,
            "sync_turn",
            {"user": user, "assistant": assistant, "messages": messages},
            activity_id=activity_id,
        )

    def _enqueue_background(
        self,
        owner: OwnerState,
        kind: str,
        payload: Mapping[str, Any],
        *,
        activity_id: Optional[str] = None,
    ) -> bool:
        with self._lock:
            if not self._accepting:
                if activity_id:
                    self._update_activity_locked(
                        owner,
                        activity_id,
                        "cancelled",
                        "Memory background work cancelled",
                        "extension shutdown fence",
                    )
                return False
            if owner.queue_depth >= self.config.limits.max_queue_depth:
                owner.state = "degraded"
                owner.last_error_code = "background_queue_full"
                if activity_id:
                    self._update_activity_locked(
                        owner,
                        activity_id,
                        "failed",
                        "Memory background work rejected",
                        f"{owner.provider_label} · bounded queue full",
                    )
                self._changed(owner)
                return False
            task = BackgroundTask(owner.key, owner.activation, kind, dict(payload), activity_id)
            owner.queue_depth += 1
        try:
            self._background_queue.put_nowait(task)
        except queue.Full:
            with self._lock:
                owner.queue_depth = max(0, owner.queue_depth - 1)
                owner.state = "degraded"
                owner.last_error_code = "global_background_queue_full"
                if activity_id:
                    self._update_activity_locked(
                        owner,
                        activity_id,
                        "failed",
                        "Memory background work rejected",
                        f"{owner.provider_label} · global queue full",
                    )
            self._changed(owner)
            return False
        self._changed(owner)
        return True

    def _background_loop(self) -> None:
        while True:
            if self._shutdown_event.is_set() and self._background_queue.empty():
                return
            try:
                task = self._background_queue.get(timeout=0.05)
            except queue.Empty:
                continue
            try:
                self._execute_background(task)
            finally:
                with self._lock:
                    owner = self._owners.get(task.owner_key)
                    if owner is not None:
                        owner.queue_depth = max(0, owner.queue_depth - 1)
                self._background_queue.task_done()
                if owner is not None:
                    self._changed(owner)

    def _execute_background(self, task: BackgroundTask) -> None:
        with self._lock:
            owner = self._owners.get(task.owner_key)
            if owner is None or owner.activation != task.activation or owner.provider is None:
                if owner is not None and task.activity_id:
                    self._update_activity_locked(
                        owner,
                        task.activity_id,
                        "cancelled",
                        "Memory background work cancelled",
                        "owner or provider generation changed",
                    )
                return
            loaded = owner.provider
            provider = loaded.provider
        timeout = self.config.limits.sync_timeout_ms
        if task.kind == "sync_turn":
            messages = list(task.payload.get("messages", []))
            if _accepts_keyword(provider.sync_turn, "messages"):
                call = lambda: provider.sync_turn(
                    task.payload.get("user", ""),
                    task.payload.get("assistant", ""),
                    session_id=owner.session_id,
                    messages=messages,
                )
            else:
                call = lambda: provider.sync_turn(
                    task.payload.get("user", ""),
                    task.payload.get("assistant", ""),
                    session_id=owner.session_id,
                )
        elif task.kind == "queue_prefetch":
            call = lambda: provider.queue_prefetch(
                task.payload.get("query", ""), session_id=owner.session_id
            )
        elif task.kind == "session_end":
            call = lambda: provider.on_session_end(list(task.payload.get("messages", [])))
        elif task.kind == "memory_write":
            metadata = {
                "write_origin": "ygg_builtin_memory_hook",
                "execution_context": "primary",
                "session_id": owner.session_id,
                "platform": "ygg",
                "tool_name": "memory",
            }
            if _accepts_keyword(provider.on_memory_write, "metadata"):
                call = lambda: provider.on_memory_write(
                    task.payload.get("action", ""),
                    task.payload.get("target", "memory"),
                    task.payload.get("content", ""),
                    metadata=metadata,
                )
            else:
                call = lambda: provider.on_memory_write(
                    task.payload.get("action", ""),
                    task.payload.get("target", "memory"),
                    task.payload.get("content", ""),
                )
        elif task.kind == "settle_owner":
            call = provider.shutdown
            timeout = self.config.limits.shutdown_timeout_ms
        else:
            return
        outcome = self._call_bounded(
            owner,
            task.kind,
            call,
            timeout,
            cancellation=None,
            ignore_shutdown=True,
        )
        with self._lock:
            if owner.activation != task.activation:
                return
            if task.kind == "sync_turn":
                owner.last_sync = {
                    "outcome": "accepted" if outcome.state == "succeeded" else outcome.state,
                    "latencyMs": outcome.duration_ms,
                }
            elif task.kind == "queue_prefetch":
                owner.last_prefetch = dict(
                    owner.last_prefetch,
                    queuedOutcome="accepted" if outcome.state == "succeeded" else outcome.state,
                    queueLatencyMs=outcome.duration_ms,
                )
            if task.activity_id:
                if outcome.state == "succeeded" and task.kind == "memory_write":
                    self._update_activity_locked(
                        owner,
                        task.activity_id,
                        "degraded",
                        "Memory write forwarded; durability unreported",
                        f"{owner.provider_label} · hook accepted · {outcome.duration_ms} ms · provider gave no durability acknowledgement",
                    )
                elif outcome.state == "succeeded":
                    self._update_activity_locked(
                        owner,
                        task.activity_id,
                        "succeeded",
                        "Memory sync accepted" if task.kind == "sync_turn" else "Memory background work accepted",
                        f"{owner.provider_label} · provider accepted call · {outcome.duration_ms} ms",
                    )
                else:
                    self._update_activity_locked(
                        owner,
                        task.activity_id,
                        "cancelled" if outcome.state == "cancelled" else "failed",
                        "Memory background work failed",
                        f"{owner.provider_label} · {outcome.state} · {outcome.duration_ms} ms",
                    )
                    owner.state = "degraded"
                    owner.last_error_code = outcome.error_code or f"{task.kind}_{outcome.state}"
            if task.kind == "settle_owner":
                owner.provider = None
                owner.tools = ()
                owner.published_tool_names = ()
                owner.static_context = None
                owner.frozen_context = None
                owner.messages.clear()
                owner.user_text = ""
                owner.assistant_text = None
                owner.state = "stopped"
                owner.activation += 1
                if self._catalog_owner_key == owner.key:
                    self._clear_catalog()

    def _cancel_queued_background(self) -> None:
        while True:
            try:
                task = self._background_queue.get_nowait()
            except queue.Empty:
                return
            with self._lock:
                owner = self._owners.get(task.owner_key)
                if owner is not None:
                    owner.queue_depth = max(0, owner.queue_depth - 1)
                    if task.activity_id:
                        self._update_activity_locked(
                            owner,
                            task.activity_id,
                            "cancelled",
                            "Memory background work cancelled",
                            "extension shutdown fence",
                        )
            self._background_queue.task_done()

    # -- Bounded provider calls -------------------------------------------

    def _call_bounded(
        self,
        owner: OwnerState,
        kind: str,
        function: Callable[[], Any],
        timeout_ms: int,
        *,
        cancellation: Any,
        ignore_shutdown: bool = False,
        terminal: bool = False,
    ) -> CallOutcome:
        box: Dict[str, Any] = {}
        done = threading.Event()

        def run() -> None:
            try:
                box["value"] = function()
            except BaseException as error:  # provider code is an isolation boundary
                box["error_code"] = safe_error_code(error)
            finally:
                with self._call_lock:
                    self._active_call_threads.pop(threading.current_thread(), None)
                done.set()

        with self._call_lock:
            dead = [thread for thread in self._active_call_threads if not thread.is_alive()]
            for thread in dead:
                self._active_call_threads.pop(thread, None)
            maximum = max(4, self.config.limits.max_owners * 2)
            hard_maximum = maximum + self.config.limits.max_owners if terminal else maximum
            if len(self._active_call_threads) >= hard_maximum:
                return CallOutcome("overloaded", None, 0, "provider_call_capacity_exhausted")
            thread = threading.Thread(
                target=run,
                name=f"hermes-{safe_identifier(kind, fallback='call', maximum=32)}-{owner.owner_reference[-8:]}",
                daemon=True,
            )
            self._active_call_threads[thread] = (owner.key, kind)
            started = time.monotonic()
            thread.start()
        deadline = started + timeout_ms / 1000.0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return CallOutcome("timeout", None, int((time.monotonic() - started) * 1000), f"{safe_identifier(kind)}_timeout")
            if done.wait(min(0.02, remaining)):
                duration = int((time.monotonic() - started) * 1000)
                if "error_code" in box:
                    return CallOutcome("failed", None, duration, box["error_code"])
                return CallOutcome("succeeded", box.get("value"), duration)
            if cancellation is not None and getattr(cancellation, "cancelled", False):
                return CallOutcome("cancelled", None, int((time.monotonic() - started) * 1000), f"{safe_identifier(kind)}_cancelled")
            if self._shutdown_event.is_set() and not ignore_shutdown:
                return CallOutcome("cancelled", None, int((time.monotonic() - started) * 1000), f"{safe_identifier(kind)}_shutdown")

    # -- Activities, health, presentation ---------------------------------

    def _finish_read_activity(
        self,
        owner: OwnerState,
        candidate: ProviderCandidate,
        started_ms: int,
        state: str,
        byte_count: int,
        item_count: int,
        truncated: bool,
        sources: Sequence[str],
        cache: str,
    ) -> None:
        latency = max(0, self._clock() - started_ms)
        self._add_activity(
            owner,
            "memory_read",
            state,
            "Memory read" if state == "succeeded" else f"Memory read {state}",
            (
                f"{candidate.label} · {'+'.join(safe_identifier(item) for item in sources)} · "
                f"{item_count} items · {byte_count} bytes · cache {cache} · {latency} ms · "
                f"{'truncated' if truncated else state}"
            ),
            terminal=True,
        )

    def _add_activity(
        self,
        owner: OwnerState,
        kind: str,
        state: str,
        summary: str,
        provenance: str,
        *,
        terminal: bool,
    ) -> str:
        with self._lock:
            owner.activity_sequence += 1
            identifier = f"memory:{owner.owner_reference[-8:]}:{owner.activity_sequence}"
            now = self._clock()
            owner.activities.append(
                Activity(
                    id=identifier,
                    kind=safe_identifier(kind, fallback="memory_activity"),
                    state=state,
                    summary=safe_label(summary, maximum=512),
                    provenance=safe_label(provenance, maximum=1024),
                    started_at_ms=now,
                    completed_at_ms=now if terminal else None,
                )
            )
            return identifier

    def _update_activity(
        self,
        owner: OwnerState,
        activity_id: str,
        state: str,
        summary: str,
        provenance: str,
    ) -> None:
        with self._lock:
            self._update_activity_locked(owner, activity_id, state, summary, provenance)

    def _update_activity_locked(
        self,
        owner: OwnerState,
        activity_id: str,
        state: str,
        summary: str,
        provenance: str,
    ) -> None:
        replacement: Deque[Activity] = deque(maxlen=MAX_ACTIVITIES)
        for activity in owner.activities:
            if activity.id == activity_id:
                activity = Activity(
                    id=activity.id,
                    kind=activity.kind,
                    state=state,
                    summary=safe_label(summary, maximum=512),
                    provenance=safe_label(provenance, maximum=1024),
                    started_at_ms=activity.started_at_ms,
                    completed_at_ms=self._clock(),
                )
            replacement.append(activity)
        owner.activities = replacement

    def _recall_count(self, owner: OwnerState, loaded: LoadedProvider, *, default: int) -> int:
        if "recall_status" not in loaded.optional_hooks:
            return default
        outcome = self._call_bounded(
            owner,
            "recall_status",
            loaded.provider.recall_status,
            min(250, self.config.limits.prefetch_timeout_ms),
            cancellation=current_cancellation(),
        )
        if outcome.state != "succeeded" or outcome.value is None:
            return default
        try:
            count = getattr(outcome.value, "count", default)
        except Exception:
            return default
        if isinstance(count, int) and not isinstance(count, bool) and 0 <= count <= 10000:
            return count or default
        return default

    def status_contribution(self, context: Optional[Mapping[str, Any]], surface: str) -> Mapping[str, Any]:
        owner = self.owner_for_context(context)
        owner_view = self._owner_view(owner)
        text = compact_status(owner_view)
        state = owner_view.get("state")
        if owner.selected_id is None and self._discovery.environment_state == "unavailable":
            text = "memory unavailable"
            state = "unavailable"
        role = "extension.ygg_hermes_memory.active"
        if state in {"degraded", "unavailable"}:
            role = "extension.ygg_hermes_memory.degraded"
        elif state in {"off", "stopped"}:
            role = "extension.ygg_hermes_memory.off"
        elif state in {"loading", "syncing"}:
            role = "extension.ygg_hermes_memory.running"
        return {"surface": surface, "text": text, "style_role": role, "priority": 16}

    def presentation_snapshot(self, owner: Optional[OwnerState] = None) -> Mapping[str, Any]:
        with self._lock:
            if owner is None:
                owner = self._owners.get(self._current_owner_key or "")
                if owner is None:
                    owner = self.owner_for_context({})
            revision = self._revision
        return build_presentation(
            revision=revision,
            discovery=self._discovery_view(),
            owner=self._owner_view(owner),
        )

    def _changed(self, owner: Optional[OwnerState] = None) -> None:
        with self._lock:
            self._measurement_snapshot = self._measurements()
            self._revision += 1
            if not self._started:
                return
            owner_key = owner.key if owner is not None else None
        self._schedule_presentation(owner_key)

    def _schedule_presentation(self, owner_key: Optional[str]) -> None:
        publish_key = None
        publish_now = False
        now = time.monotonic()
        minimum_interval = 1.0 / 30.0  # stay below the host's 32 updates/second ceiling
        with self._presentation_publish_lock:
            due_in = self._last_presentation_at + minimum_interval - now
            if due_in <= 0 and self._presentation_timer is None and not self._presentation_pending_keys:
                self._last_presentation_at = now
                publish_key = owner_key
                publish_now = True
            else:
                if owner_key not in self._presentation_pending_key_set:
                    self._presentation_pending_keys.append(owner_key)
                    self._presentation_pending_key_set.add(owner_key)
                if self._presentation_timer is None:
                    timer = threading.Timer(max(0.001, due_in), self._flush_presentation)
                    timer.name = "hermes-presentation-coalesce"
                    timer.daemon = True
                    self._presentation_timer = timer
                    timer.start()
        if publish_now:
            self._publish_presentation_snapshot(publish_key)

    def _flush_presentation(self) -> None:
        publish_key = None
        has_snapshot = False
        with self._presentation_publish_lock:
            self._presentation_timer = None
            if not self._started or not self._presentation_pending_keys:
                self._presentation_pending_keys.clear()
                self._presentation_pending_key_set.clear()
                return
            publish_key = self._presentation_pending_keys.popleft()
            self._presentation_pending_key_set.discard(publish_key)
            has_snapshot = True
            self._last_presentation_at = time.monotonic()
            if self._presentation_pending_keys:
                timer = threading.Timer(1.0 / 30.0, self._flush_presentation)
                timer.name = "hermes-presentation-coalesce"
                timer.daemon = True
                self._presentation_timer = timer
                timer.start()
        if has_snapshot:
            self._publish_presentation_snapshot(publish_key)

    def _publish_presentation_snapshot(self, owner_key: Optional[str]) -> None:
        with self._lock:
            if owner_key is None:
                owner = self._owners.get(self._current_owner_key or "")
            else:
                owner = self._owners.get(owner_key)
            if owner is None:
                return
            self._revision += 1
            owner_payload = None
            if (
                owner.extension_instance_id
                and owner.process_generation is not None
                and owner.process_generation >= 1
            ):
                owner_payload = {
                    "session_id": owner.session_id,
                    "extension_instance_id": owner.extension_instance_id,
                    "process_generation": owner.process_generation,
                }
        snapshot = self.presentation_snapshot(owner)
        try:
            if current_request_id() is not None:
                self.extension.publish_presentation(snapshot)
            elif owner_payload is not None:
                self.extension.publish_presentation(
                    snapshot,
                    resource_owner=owner_payload,
                )
            elif owner_key is None:
                self.extension.publish_presentation(snapshot)
            else:
                # Never downgrade owner-specific state to a process-global
                # snapshot when a complete host fence is unavailable.
                return
        except Exception:
            # Older/incomplete hosts may not expose the generic primitive. The
            # `/memory` fallback stays authoritative and provider work continues.
            return

    def _stop_presentation_publisher(self) -> None:
        with self._presentation_publish_lock:
            timer = self._presentation_timer
            self._presentation_timer = None
            self._presentation_pending_keys.clear()
            self._presentation_pending_key_set.clear()
        if timer is not None:
            timer.cancel()

    def _discovery_view(self) -> Mapping[str, Any]:
        with self._lock:
            providers = [self._candidate_view(candidate) for candidate in self._discovery.candidates]
            snapshot = self._discovery
        return {
            "environment": snapshot.environment_id,
            "environmentVersion": snapshot.environment_version,
            "environmentState": snapshot.environment_state,
            "reasonCode": snapshot.reason_code,
            "contractVersion": HERMES_CONTRACT_ID,
            "providers": providers,
        }

    def _candidate_view(self, candidate: Optional[ProviderCandidate]) -> Optional[Mapping[str, Any]]:
        if candidate is None:
            return None
        return candidate.safe_metadata(
            trusted=candidate.trusted_by(self.config, self._runtime_trust)
        )

    def _owner_view(self, owner: OwnerState) -> Mapping[str, Any]:
        with self._lock:
            state = "syncing" if owner.queue_depth and owner.state == "active" else owner.state
            activities = [
                {
                    "id": item.id,
                    "kind": item.kind,
                    "state": item.state,
                    "summary": item.summary,
                    "provenance": item.provenance,
                    "startedAtMs": item.started_at_ms,
                    "completedAtMs": item.completed_at_ms,
                    "ownerReference": owner.owner_reference,
                }
                for item in owner.activities
            ]
            return {
                "ownerReference": owner.owner_reference,
                "selectedId": owner.selected_id,
                "inspectedId": owner.inspected_id,
                "providerLabel": owner.provider_label,
                "state": state,
                "lastErrorCode": owner.last_error_code,
                "setupHint": owner.setup_hint,
                "toolCount": len(owner.published_tool_names),
                "tools": [tool["name"] for tool in owner.tools],
                "publishedTools": list(owner.published_tool_names),
                "contextByteLimit": self.config.limits.max_context_bytes,
                "queueDepth": owner.queue_depth,
                "lastPrefetch": dict(owner.last_prefetch),
                "lastSync": dict(owner.last_sync),
                "optionalHooks": list(owner.optional_hooks),
                "unsupportedHooks": list(owner.unsupported_hooks),
                "activities": activities,
                "measurements": dict(self._measurement_snapshot),
            }

    def _measurements(self) -> Mapping[str, Any]:
        try:
            usage = resource.getrusage(resource.RUSAGE_SELF)
            rss = int(usage.ru_maxrss)
            if sys.platform == "darwin":
                rss //= 1024
        except Exception:
            rss = 0
        return {"cpuSeconds": max(0.0, time.process_time()), "rssKiB": max(0, rss)}

    def _lifecycle_report(self, owner: OwnerState) -> str:
        mapped = [
            "system_prompt_block -> context/collect (frozen per activation)",
            "prefetch -> bounded context/collect (frozen per prompt epoch)",
            "on_turn_start -> before_prompt",
            "sync_turn -> captured before_prompt user + successful after_response assistant",
            "queue_prefetch -> completed turn/settled",
            "on_memory_write -> committed built-in memory after_tool_call",
            "on_session_end -> session/settled with bounded in-process snippets",
            "shutdown -> bounded extension/session shutdown",
        ]
        unsupported = list(owner.unsupported_hooks) or ["none"]
        return "Mapped lifecycle:\n- " + "\n- ".join(mapped) + "\nUnsupported/no equivalent:\n- " + "\n- ".join(unsupported)

    def _ensure_active_default(self, owner: OwnerState) -> None:
        with self._lock:
            if owner.provider is not None or owner.state == "loading":
                return
            selected_id = owner.selected_id or self.config.default_provider
        if selected_id is None:
            return
        candidate = self._discovery.by_id(selected_id)
        if candidate is None or not candidate.trusted_by(self.config, self._runtime_trust):
            return
        with self._lock:
            if owner.provider is not None or owner.state == "loading":
                return
            owner.selected_id = candidate.id
            owner.inspected_id = candidate.id
            owner.provider_label = candidate.label
            owner.state = "loading"
        self._changed(owner)

        def activate_default() -> None:
            self._activate(
                owner,
                candidate,
                cancellation=None,
                selection_kind="selected",
                preserve_turn=True,
            )

        threading.Thread(
            target=activate_default,
            name=f"hermes-default-{owner.owner_reference[-8:]}",
            daemon=True,
        ).start()

    def _tool_operation(self, candidate: ProviderCandidate, tool_name: str) -> str:
        if tool_name in candidate.write_tools:
            return "write"
        if tool_name in candidate.read_tools:
            return "read"
        lowered = tool_name.lower()
        if any(token in lowered for token in ("remember", "store", "write", "save", "forget", "delete", "update")):
            return "write"
        if any(token in lowered for token in ("recall", "search", "read", "find", "query", "retrieve")):
            return "read"
        return "unknown"

    def _ensure_provider_environment(self) -> None:
        with self._lock:
            if self._provider_environment_loaded:
                return
            environment = self.config.environment
            path = environment.provider_env_file if environment is not None else None
        values = {} if path is None else read_provider_environment(path)
        with self._lock:
            if self._provider_environment_loaded:
                return
            previous: Dict[str, Optional[str]] = {}
            for name, value in values.items():
                previous[name] = os.environ.get(name)
                os.environ[name] = value
            self._provider_environment_previous = previous
            self._provider_environment_keys = tuple(sorted(values))
            self._provider_environment_loaded = True

    def _clear_provider_environment(self) -> None:
        with self._lock:
            previous = self._provider_environment_previous
            self._provider_environment_previous = {}
            self._provider_environment_keys = ()
            self._provider_environment_loaded = False
        for name, value in previous.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value

    def _shutdown_loaded_async(self, loaded: LoadedProvider) -> None:
        def stop() -> None:
            try:
                loaded.provider.shutdown()
            except BaseException:
                pass

        threading.Thread(target=stop, name="hermes-provider-retire", daemon=True).start()


def _accepts_keyword(function: Callable[..., Any], keyword: str) -> bool:
    try:
        signature = inspect.signature(function)
    except Exception:
        return True
    if keyword in signature.parameters:
        return True
    return any(
        parameter.kind == inspect.Parameter.VAR_KEYWORD
        for parameter in signature.parameters.values()
    )
