"""Selected-only loading of the pinned Hermes MemoryProvider contract."""

from __future__ import annotations

from dataclasses import dataclass
import importlib
import importlib.metadata as importlib_metadata
import importlib.util
import inspect
import os
from pathlib import Path
import re
import sys
import tempfile
import threading
from types import ModuleType
from typing import Any, Tuple

from .config import BridgeConfig, current_environment_matches
from .constants import HERMES_CONTRACT_VERSION
from .discovery import (
    ProviderCandidate,
    directory_snapshot,
    entry_point_snapshot,
)
from .safety import safe_identifier, safe_label


class ProviderLoadError(RuntimeError):
    """Stable provider load failure that never retains provider exception text."""

    def __init__(self, code: str) -> None:
        self.code = safe_identifier(code, fallback="provider_load_failed", maximum=64)
        super().__init__(self.code)


_MODULE_PATH_RE = re.compile(r"^[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*$", re.ASCII)
_SNAPSHOT_LOCK = threading.Lock()
_ENTRY_POINT_SNAPSHOTS = {}
_CLAIMED_PROVIDER_INSTANCES = {}
_MAX_CLAIMED_PROVIDER_INSTANCES = 256


def _materialize_snapshot(files: Any, prefix: str) -> Tuple[Any, Path]:
    temporary = tempfile.TemporaryDirectory(prefix=prefix)
    root = Path(temporary.name)
    try:
        for relative, data in sorted(files.items()):
            relative_path = Path(relative)
            if relative_path.is_absolute() or not relative_path.parts or any(
                part in {"", ".", ".."} for part in relative_path.parts
            ):
                raise ProviderLoadError("provider_snapshot_path_invalid")
            path = root.joinpath(*relative_path.parts)
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            with path.open("xb") as handle:
                handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
            path.chmod(0o600)
        return temporary, root
    except Exception:
        temporary.cleanup()
        raise


def _claim_provider_instance(provider: Any) -> None:
    with _SNAPSHOT_LOCK:
        identity = id(provider)
        if identity in _CLAIMED_PROVIDER_INSTANCES:
            raise ProviderLoadError("provider_instance_reused")
        if len(_CLAIMED_PROVIDER_INSTANCES) >= _MAX_CLAIMED_PROVIDER_INSTANCES:
            raise ProviderLoadError("provider_instance_registry_full")
        _CLAIMED_PROVIDER_INSTANCES[identity] = provider


@dataclass(frozen=True)
class LoadedProvider:
    provider: Any
    memory_provider_class: type
    optional_hooks: Tuple[str, ...]
    unsupported_hooks: Tuple[str, ...]
    ignored_registrations: Tuple[str, ...]


class _ProviderCollector:
    """Narrow Hermes registration context with all non-memory effects disabled."""

    def __init__(self) -> None:
        self.providers = []
        self.ignored = []

    @property
    def provider(self) -> Any:
        return self.providers[0] if self.providers else None

    def register_memory_provider(self, provider: Any) -> None:
        if len(self.providers) >= 1:
            raise ProviderLoadError("multiple_providers_registered")
        self.providers.append(provider)

    def register_skill(self, *args: Any, **kwargs: Any) -> None:
        del args, kwargs
        self.ignored.append("register_skill")

    def register_cli_command(self, *args: Any, **kwargs: Any) -> None:
        del args, kwargs
        self.ignored.append("register_cli_command")

    def __getattr__(self, name: str) -> Any:
        if not name.startswith("register_"):
            raise AttributeError(name)

        def ignore(*args: Any, **kwargs: Any) -> None:
            del args, kwargs
            self.ignored.append(name)

        return ignore


def load_selected_provider(
    candidate: ProviderCandidate,
    config: BridgeConfig,
    *,
    expected_fingerprint: str,
    metadata_module: Any = importlib_metadata,
) -> LoadedProvider:
    """Import exactly one already-trusted provider and validate its ABC identity."""

    if candidate.fingerprint is None or expected_fingerprint != candidate.fingerprint:
        raise ProviderLoadError("provider_not_trusted")
    if candidate.availability == "unavailable":
        raise ProviderLoadError(candidate.reason_code or "provider_unavailable")
    environment = config.environment
    if environment is None or not current_environment_matches(environment):
        raise ProviderLoadError("python_environment_mismatch")
    try:
        installed_version = str(metadata_module.version("hermes-agent"))
    except Exception as error:
        raise ProviderLoadError("hermes_contract_not_installed") from error
    if installed_version != HERMES_CONTRACT_VERSION:
        raise ProviderLoadError("hermes_contract_version_mismatch")

    # Importing the Hermes ABC is intentionally delayed until after selection
    # and exact trust. It may import the `agent` package in the configured
    # provider environment, so discovery must never do this.
    try:
        contract_module = importlib.import_module("agent.memory_provider")
        memory_provider_class = getattr(contract_module, "MemoryProvider")
    except Exception as error:
        raise ProviderLoadError("hermes_contract_import_failed") from error
    _verify_contract_origin(contract_module, metadata_module)
    if not isinstance(memory_provider_class, type):
        raise ProviderLoadError("hermes_contract_class_invalid")

    collector = _ProviderCollector()
    if candidate.source == "directory":
        loaded = _load_directory(candidate, expected_fingerprint)
    elif candidate.source == "entrypoint":
        loaded = _load_entry_point(candidate, expected_fingerprint)
    else:
        raise ProviderLoadError("provider_source_unsupported")

    provider = _extract_provider(loaded, memory_provider_class, collector)
    if provider is None or not isinstance(provider, memory_provider_class):
        raise ProviderLoadError("memory_provider_instance_missing")
    _validate_required_contract(provider)
    try:
        actual_name = provider.name
    except Exception as error:
        raise ProviderLoadError("provider_name_failed") from error
    if not isinstance(actual_name, str) or not actual_name.strip():
        raise ProviderLoadError("provider_name_invalid")
    if safe_label(actual_name, fallback="provider", maximum=128) != candidate.name:
        raise ProviderLoadError("provider_name_mismatch")

    optional = []
    unsupported = []
    for hook in (
        "system_prompt_block",
        "prefetch",
        "queue_prefetch",
        "recall_status",
        "sync_turn",
        "on_turn_start",
        "on_session_end",
        "on_session_switch",
        "on_pre_compress",
        "on_memory_write",
        "on_delegation",
        "shutdown",
    ):
        provider_impl = getattr(type(provider), hook, None)
        base_impl = getattr(memory_provider_class, hook, None)
        if callable(provider_impl) and provider_impl is not base_impl:
            optional.append(hook)
        else:
            unsupported.append(hook)
    _claim_provider_instance(provider)
    return LoadedProvider(
        provider=provider,
        memory_provider_class=memory_provider_class,
        optional_hooks=tuple(optional),
        unsupported_hooks=tuple(unsupported),
        ignored_registrations=tuple(sorted(set(collector.ignored))),
    )


def _verify_contract_origin(contract_module: Any, metadata_module: Any) -> None:
    actual_file = getattr(contract_module, "__file__", None)
    if not isinstance(actual_file, str):
        raise ProviderLoadError("hermes_contract_origin_missing")
    try:
        distribution = metadata_module.distribution("hermes-agent")
        files = list(getattr(distribution, "files", None) or [])
    except Exception as error:
        raise ProviderLoadError("hermes_contract_distribution_missing") from error
    expected = None
    for relative in files:
        normalized = str(relative).replace("\\", "/")
        if normalized == "agent/memory_provider.py":
            expected = Path(distribution.locate_file(relative)).resolve(strict=True)
            break
    if expected is None:
        raise ProviderLoadError("hermes_contract_origin_unlisted")
    try:
        actual = Path(actual_file).resolve(strict=True)
    except OSError as error:
        raise ProviderLoadError("hermes_contract_origin_missing") from error
    if actual != expected:
        raise ProviderLoadError("hermes_contract_origin_mismatch")


def _load_directory(candidate: ProviderCandidate, expected_fingerprint: str) -> ModuleType:
    path = candidate.path
    if path is None:
        raise ProviderLoadError("directory_provider_path_missing")
    try:
        actual, files = directory_snapshot(candidate.id, path, candidate.environment_id)
    except Exception as error:
        raise ProviderLoadError("directory_provider_changed") from error
    if actual != expected_fingerprint:
        raise ProviderLoadError("directory_provider_changed")
    module_name = f"_ygg_hermes_provider_{expected_fingerprint[:24]}"
    previous = sys.modules.get(module_name)
    if previous is not None and getattr(previous, "_ygg_snapshot_fingerprint", None) == actual:
        return previous
    temporary = None
    try:
        temporary, root = _materialize_snapshot(files, "ygg-hermes-directory-")
        init_file = root / "__init__.py"
        spec = importlib.util.spec_from_file_location(
            module_name,
            str(init_file),
            submodule_search_locations=[str(root)],
        )
        if spec is None or spec.loader is None:
            temporary.cleanup()
            raise ProviderLoadError("directory_provider_spec_failed")
        module = importlib.util.module_from_spec(spec)
        module._ygg_snapshot_owner = temporary
        module._ygg_snapshot_fingerprint = actual
        sys.modules[module_name] = module
        previous_bytecode = sys.dont_write_bytecode
        sys.dont_write_bytecode = True
        try:
            spec.loader.exec_module(module)
        finally:
            sys.dont_write_bytecode = previous_bytecode
        return module
    except ProviderLoadError:
        sys.modules.pop(module_name, None)
        if temporary is not None:
            temporary.cleanup()
        raise
    except Exception as error:
        sys.modules.pop(module_name, None)
        if temporary is not None:
            temporary.cleanup()
        raise ProviderLoadError("directory_provider_import_failed") from error


def _entry_point_target(value: str) -> Tuple[str, Tuple[str, ...]]:
    if "[" in value or "]" in value:
        raise ProviderLoadError("entry_point_value_invalid")
    module_name, separator, attribute = value.partition(":")
    if not _MODULE_PATH_RE.fullmatch(module_name):
        raise ProviderLoadError("entry_point_value_invalid")
    attributes = tuple(attribute.split(".")) if separator else ()
    if any(not re.fullmatch(r"[A-Za-z_]\w*", item, re.ASCII) for item in attributes):
        raise ProviderLoadError("entry_point_value_invalid")
    return module_name, attributes


def _load_entry_point(candidate: ProviderCandidate, expected_fingerprint: str) -> Any:
    entry_point = candidate.entry_point
    if entry_point is None:
        raise ProviderLoadError("entry_point_missing")
    if str(getattr(entry_point, "value", "")) != (candidate.entry_point_value or ""):
        raise ProviderLoadError("entry_point_changed")
    try:
        actual, files = entry_point_snapshot(entry_point, candidate.environment_id)
    except Exception as error:
        raise ProviderLoadError("entry_point_changed") from error
    if actual != expected_fingerprint:
        raise ProviderLoadError("entry_point_changed")
    module_name, attributes = _entry_point_target(candidate.entry_point_value or "")
    cache_key = (candidate.entry_point_value, actual)
    with _SNAPSHOT_LOCK:
        cached = _ENTRY_POINT_SNAPSHOTS.get(cache_key)
    if cached is not None:
        return cached[0]

    temporary = None
    try:
        temporary, root = _materialize_snapshot(files, "ygg-hermes-entrypoint-")
        for loaded_name in list(sys.modules):
            if loaded_name == module_name or loaded_name.startswith(module_name + "."):
                sys.modules.pop(loaded_name, None)
        sys.path.insert(0, str(root))
        previous_bytecode = sys.dont_write_bytecode
        sys.dont_write_bytecode = True
        try:
            loaded = importlib.import_module(module_name)
        finally:
            sys.dont_write_bytecode = previous_bytecode
            try:
                sys.path.remove(str(root))
            except ValueError:
                pass
        actual_file = getattr(loaded, "__file__", None)
        if not isinstance(actual_file, str):
            raise ProviderLoadError("entry_point_import_origin_missing")
        try:
            Path(actual_file).resolve(strict=True).relative_to(root.resolve(strict=True))
        except (OSError, ValueError) as error:
            raise ProviderLoadError("entry_point_import_origin_mismatch") from error
        target = loaded
        for attribute in attributes:
            target = getattr(target, attribute)
        with _SNAPSHOT_LOCK:
            if len(_ENTRY_POINT_SNAPSHOTS) >= 128:
                raise ProviderLoadError("entry_point_snapshot_registry_full")
            _ENTRY_POINT_SNAPSHOTS[cache_key] = (target, temporary)
        return target
    except ProviderLoadError:
        if temporary is not None:
            temporary.cleanup()
        raise
    except Exception as error:
        if temporary is not None:
            temporary.cleanup()
        raise ProviderLoadError("entry_point_import_failed") from error


def _extract_provider(loaded: Any, base: type, collector: _ProviderCollector) -> Any:
    if isinstance(loaded, base):
        raise ProviderLoadError("provider_instance_entry_point_unsupported")
    if isinstance(loaded, type) and issubclass(loaded, base):
        try:
            return loaded()
        except Exception as error:
            raise ProviderLoadError("provider_constructor_failed") from error

    register = getattr(loaded, "register", None)
    if callable(register):
        try:
            result = register(collector)
        except ProviderLoadError:
            raise
        except Exception as error:
            if collector.provider is None:
                raise ProviderLoadError("provider_register_failed") from error
            result = None
        if isinstance(result, base):
            if collector.provider is not None and result is not collector.provider:
                raise ProviderLoadError("multiple_providers_registered")
            return result
        if collector.provider is not None:
            return collector.provider

    # An entry point commonly targets register(ctx) directly rather than its
    # module. Call only a function that demonstrably accepts at least one
    # argument; never guess by catching arbitrary TypeError from provider code.
    if callable(loaded) and not isinstance(loaded, ModuleType):
        try:
            signature = inspect.signature(loaded)
            required_or_positional = [
                parameter
                for parameter in signature.parameters.values()
                if parameter.kind
                in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD)
            ]
        except (TypeError, ValueError):
            required_or_positional = [object()]
        if required_or_positional:
            try:
                result = loaded(collector)
            except ProviderLoadError:
                raise
            except Exception as error:
                if collector.provider is None:
                    raise ProviderLoadError("provider_register_failed") from error
                result = None
            if isinstance(result, base):
                if collector.provider is not None and result is not collector.provider:
                    raise ProviderLoadError("multiple_providers_registered")
                return result
            if collector.provider is not None:
                return collector.provider
        else:
            try:
                result = loaded()
            except Exception as error:
                raise ProviderLoadError("provider_constructor_failed") from error
            if isinstance(result, base):
                return result

    if isinstance(loaded, ModuleType):
        candidates = []
        for value in vars(loaded).values():
            if isinstance(value, type) and value is not base and issubclass(value, base):
                candidates.append(value)
        if len(candidates) == 1:
            try:
                return candidates[0]()
            except Exception as error:
                raise ProviderLoadError("provider_constructor_failed") from error
        if len(candidates) > 1:
            raise ProviderLoadError("multiple_provider_classes")
    return None


def _validate_required_contract(provider: Any) -> None:
    for name in (
        "is_available",
        "initialize",
        "get_tool_schemas",
        "handle_tool_call",
    ):
        if not callable(getattr(provider, name, None)):
            raise ProviderLoadError(f"provider_missing_{name}")
