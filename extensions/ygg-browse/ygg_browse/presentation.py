"""Frontend-neutral Browse status, activity, collection, and actions."""

from __future__ import annotations

import threading
import time
from collections import OrderedDict
from typing import Any, Callable, Dict, List, Mapping, Optional

from ygg_extension.extension import MAX_PRESENTATION_REVISION

from .safety import bounded_text


class PresentationPublisher:
    # Stay below the host's 32 update/second abuse bound even when several
    # protocol handlers complete together. State updates are complete snapshots,
    # so brief serialization does not require replay.
    _MIN_INTERVAL_SECONDS = 1.0 / 24.0

    def __init__(self, extension: Any) -> None:
        self.extension = extension
        self._lock = threading.Lock()
        self._revision = 0
        self._closed = False
        self._last_sent = 0.0

    def __call__(
        self,
        snapshot: Mapping[str, Any],
        resource_owner: Optional[Mapping[str, Any]] = None,
    ) -> None:
        if not self.extension.initialized:
            return
        with self._lock:
            if self._closed or self._revision > MAX_PRESENTATION_REVISION:
                return
            remaining = self._MIN_INTERVAL_SECONDS - (time.monotonic() - self._last_sent)
            if remaining > 0:
                time.sleep(remaining)
            value = dict(snapshot)
            value["revision"] = self._revision
            # Owner-specific state always carries the complete host-issued
            # triple, including from process-scoped status handlers. When no
            # session state exists, an ambient tool/command parent supplies the
            # owner automatically; a background setup snapshot is process-wide.
            if resource_owner is not None:
                self.extension.publish_presentation(value, resource_owner=resource_owner)
            elif self.extension.request_id is not None:
                self.extension.publish_presentation(value)
            else:
                self.extension.publish_presentation(value)
            self._revision += 1
            self._last_sent = time.monotonic()

    def close(self) -> None:
        with self._lock:
            self._closed = True


class BrowsePresentation:
    """Maintain only bounded display state; never retain page bodies or values."""

    def __init__(
        self,
        publish: Callable[[Mapping[str, Any], Optional[Mapping[str, Any]]], None],
    ) -> None:
        self._publish = publish
        self._lock = threading.RLock()
        self._resource_owner: Optional[Dict[str, Any]] = None
        self._setup: Dict[str, Any] = {
            "state": "not_set_up",
            "detail": "Pinned browser dependencies are not installed.",
            "log_path": "~/.ygg/browse/install.log",
        }
        self._browser: Dict[str, Any] = {
            "open": False,
            "tab_count": 0,
            "tabs": [],
            "selected_tab_id": None,
        }
        self._activities: "OrderedDict[str, Dict[str, Any]]" = OrderedDict()
        self._runtime_degraded: Optional[str] = None

    def update_setup(self, status: Mapping[str, Any]) -> None:
        with self._lock:
            self._setup = {
                "state": status.get("state", "degraded"),
                "detail": bounded_text(status.get("detail", "Setup status unavailable."), 512),
                "log_path": bounded_text(status.get("log_path", "unavailable"), 1024),
            }
            if self._setup["state"] == "degraded":
                self._runtime_degraded = self._setup["detail"]
            elif self._setup["state"] in {"installing", "not_set_up"}:
                self._runtime_degraded = None
            self._publish_locked()

    def update_browser(
        self,
        result: Mapping[str, Any],
        *,
        resource_owner: Optional[Mapping[str, Any]] = None,
    ) -> None:
        tabs = []
        raw_tabs = result.get("tabs")
        if isinstance(raw_tabs, list):
            for item in raw_tabs[:64]:
                if not isinstance(item, Mapping):
                    continue
                tab_id = bounded_text(item.get("tab_id", "unknown"), 64)
                tabs.append(
                    {
                        "tab_id": tab_id,
                        # Title is kept only in selected detail, never a compact
                        # label or reconnect status line.
                        "title": bounded_text(item.get("title", "Untitled"), 160),
                        "url": bounded_text(item.get("url", "unavailable"), 512),
                        "origin": bounded_text(item.get("origin", "unavailable"), 256),
                        "snapshot_generation": item.get("snapshot_generation"),
                        "selected": bool(item.get("selected", False)),
                    }
                )
        with self._lock:
            if resource_owner is not None:
                self._set_resource_owner_locked(resource_owner)
            self._browser = {
                "open": bool(result.get("open", False)),
                "tab_count": min(len(tabs), 64),
                "tabs": tabs,
                "selected_tab_id": result.get("selected_tab_id")
                if isinstance(result.get("selected_tab_id"), str)
                else None,
            }
            if self._browser["open"]:
                self._runtime_degraded = None
            elif result.get("degraded"):
                self._runtime_degraded = "Browser runtime or profile health is degraded."
            self._publish_locked()

    def activity(
        self,
        activity_id: str,
        *,
        kind: str,
        state: str,
        summary: str,
        artifact_id: Optional[str] = None,
        resource_owner: Optional[Mapping[str, Any]] = None,
    ) -> None:
        item: Dict[str, Any] = {
            "id": bounded_text(activity_id, 128),
            "kind": bounded_text(kind, 64),
            "state": state,
            "summary": bounded_text(summary, 256),
            "provenance": "visible isolated browser",
            "started_at_ms": min(int(time.time() * 1000), MAX_PRESENTATION_REVISION),
            "references": [],
        }
        if artifact_id:
            item["references"] = [
                {"kind": "artifact", "id": bounded_text(artifact_id, 1024), "label": "Screenshot"}
            ]
        with self._lock:
            if resource_owner is not None:
                self._set_resource_owner_locked(resource_owner)
            self._activities.pop(activity_id, None)
            self._activities[activity_id] = item
            while len(self._activities) > 8:
                self._activities.popitem(last=False)
            self._publish_locked()

    def mark_degraded(
        self,
        detail: str,
        *,
        resource_owner: Optional[Mapping[str, Any]] = None,
    ) -> None:
        with self._lock:
            if resource_owner is not None:
                self._set_resource_owner_locked(resource_owner)
            self._runtime_degraded = bounded_text(detail, 512)
            self._publish_locked()

    def snapshot(self) -> Dict[str, Any]:
        with self._lock:
            return self._snapshot_locked()

    def compact_status(self) -> str:
        """Owner-scoped presentation status, used only inside snapshots."""
        with self._lock:
            status = self._status_locked()
            return bounded_text(status["label"], 256)

    def process_status(self) -> str:
        """Process-safe status contribution with no tab or origin state."""
        with self._lock:
            setup_state = (
                "degraded"
                if self._runtime_degraded
                else str(self._setup.get("state", "degraded")).replace("_", " ")
            )
            return bounded_text(f"Browse · {setup_state}", 256)

    def publish(self) -> None:
        with self._lock:
            self._publish_locked()

    def _set_resource_owner_locked(self, owner: Mapping[str, Any]) -> None:
        value = dict(owner)
        if self._resource_owner is not None and self._resource_owner != value:
            # Never carry tabs, titles, generations, or artifact references into
            # a different owner's complete snapshot.
            self._browser = {
                "open": False,
                "tab_count": 0,
                "tabs": [],
                "selected_tab_id": None,
            }
            self._activities.clear()
        self._resource_owner = value

    def _publish_locked(self) -> None:
        try:
            owner = self._resource_owner
            self._publish(self._snapshot_locked(), owner)
        except Exception:
            # Presentation is best effort and must never change browser results.
            pass

    def _snapshot_locked(self) -> Dict[str, Any]:
        nodes: List[Dict[str, Any]] = []
        selected = self._browser.get("selected_tab_id")
        detail: Optional[Dict[str, Any]] = None
        if self._browser.get("open"):
            for tab in self._browser.get("tabs", []):
                tab_id = tab["tab_id"]
                node: Dict[str, Any] = {
                    "id": tab_id,
                    "state": "active" if tab_id == selected else "pending",
                    "label": f"Tab {tab_id}",
                    "secondary": tab["origin"],
                    "action_ids": [],
                    "references": [],
                }
                nodes.append(node)
                if tab_id == selected:
                    generation = tab.get("snapshot_generation")
                    generation_text = str(generation) if isinstance(generation, int) else "none"
                    detail = {
                        "node_id": tab_id,
                        "title": f"Tab {tab_id}",
                        "body": bounded_text(
                            "Sanitized untrusted title: %s\nSanitized URL: %s\nSnapshot generation: %s"
                            % (tab["title"], tab["url"], generation_text),
                            4096,
                            collapse_whitespace=False,
                        ),
                        "references": [],
                    }
        else:
            setup_state = str(self._setup.get("state", "degraded"))
            if self._runtime_degraded:
                setup_state = "degraded"
            node_state = {
                "not_set_up": "empty",
                "installing": "loading",
                "ready": "succeeded",
                "degraded": "degraded",
            }.get(setup_state, "degraded")
            nodes.append(
                {
                    "id": "browse-runtime",
                    "state": node_state,
                    "label": "Browser setup",
                    "secondary": setup_state.replace("_", " "),
                    "action_ids": ["setup"] if setup_state != "ready" else ["open"],
                    "references": [],
                }
            )
            selected = "browse-runtime"
            detail_text = self._runtime_degraded or self._setup.get("detail", "")
            detail = {
                "node_id": "browse-runtime",
                "title": "Ygg Browse",
                "body": bounded_text(
                    "%s\nInstall log: %s\nProfile: isolated Ygg-owned profile (path withheld from labels)."
                    % (detail_text, self._setup.get("log_path", "unavailable")),
                    4096,
                    collapse_whitespace=False,
                ),
                "references": [],
            }
        collection: Dict[str, Any] = {
            "kind": "list",
            "title": "Browse tabs" if self._browser.get("open") else "Browse",
            "nodes": nodes,
            "selected_node_id": selected,
        }
        if detail is not None:
            collection["detail"] = detail
        return {
            "status": self._status_locked(),
            "activities": list(self._activities.values()),
            "collection": collection,
            "actions": [
                {
                    "id": "setup",
                    "label": "Set up browser",
                    "command": "browse",
                    "arguments": ["setup"],
                    "destructive": False,
                },
                {
                    "id": "open",
                    "label": "Open visible browser",
                    "command": "browse",
                    "arguments": ["open"],
                    "destructive": False,
                },
                {
                    "id": "close",
                    "label": "Close browser",
                    "command": "browse",
                    "arguments": ["close"],
                    "destructive": False,
                },
                {
                    "id": "reset-profile",
                    "label": "Reset isolated profile",
                    "command": "browse",
                    "arguments": ["reset-profile"],
                    "destructive": True,
                },
            ],
        }

    def _status_locked(self) -> Dict[str, str]:
        if self._browser.get("open"):
            count = int(self._browser.get("tab_count", 0))
            origin = "unavailable"
            selected = self._browser.get("selected_tab_id")
            for tab in self._browser.get("tabs", []):
                if tab.get("tab_id") == selected:
                    origin = tab.get("origin", "unavailable")
                    break
            return {
                "state": "active",
                "label": f"Browse · open · {count} tab{'s' if count != 1 else ''} · {origin}",
                "detail": "Always-headful isolated persistent browser.",
            }
        setup_state = str(self._setup.get("state", "degraded"))
        if self._runtime_degraded:
            return {
                "state": "degraded",
                "label": "Browse · degraded",
                "detail": self._runtime_degraded,
            }
        state = {
            "not_set_up": "empty",
            "installing": "loading",
            "ready": "active",
            "degraded": "degraded",
        }.get(setup_state, "degraded")
        return {
            "state": state,
            "label": "Browse · " + setup_state.replace("_", " "),
            "detail": bounded_text(self._setup.get("detail", ""), 512),
        }
