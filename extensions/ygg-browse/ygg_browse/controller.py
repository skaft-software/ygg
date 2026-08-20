"""Domain controller joining setup, profile, serialized browser, and UI state."""

from __future__ import annotations

import threading
from typing import Any, Callable, Dict, Mapping, Optional

from .artifacts import ArtifactStore, ScreenshotRecord
from .paths import BrowsePaths, PLAYWRIGHT_VERSION
from .presentation import BrowsePresentation
from .profile import ProfileManager
from .safety import BrowseError, ResourceOwner, bounded_text
from .setup import SetupManager
from .snapshot import UNTRUSTED_BEGIN, UNTRUSTED_END
from .worker import (
    CONFIRMATION_OPERATION_TIMEOUT,
    DEFAULT_OPERATION_TIMEOUT,
    NAVIGATION_TIMEOUT,
    BrowserEngine,
    PlaywrightWorker,
)


Confirmation = Callable[[str, Optional[str], bool], bool]


class BrowseController:
    def __init__(
        self,
        presentation: BrowsePresentation,
        *,
        paths: Optional[BrowsePaths] = None,
        setup: Optional[SetupManager] = None,
        profile: Optional[ProfileManager] = None,
        worker: Optional[PlaywrightWorker] = None,
        artifacts: Optional[ArtifactStore] = None,
    ) -> None:
        self.paths = paths or BrowsePaths.for_home()
        self.presentation = presentation
        self.setup = setup or SetupManager(self.paths, on_state=self._on_setup_state)
        self.profile = profile or ProfileManager(self.paths)
        self.artifacts = artifacts or ArtifactStore(self.paths)
        self.worker = worker or PlaywrightWorker(
            lambda: BrowserEngine(self.paths, self.setup, self.profile)
        )
        self._shutdown_lock = threading.Lock()
        self._closed = False

    def _on_setup_state(self, status: Mapping[str, Any]) -> None:
        self.presentation.update_setup(status)
        state = str(status.get("state", "degraded"))
        activity_state = {
            "installing": "running",
            "ready": "succeeded",
            "degraded": "failed",
            "not_set_up": "empty",
        }.get(state, "degraded")
        summary = {
            "installing": "Installing pinned browser runtime",
            "ready": "Pinned browser runtime ready",
            "degraded": "Pinned browser setup failed or was interrupted",
            "not_set_up": "Pinned browser runtime is not set up",
        }.get(state, "Browser setup state changed")
        self.presentation.activity(
            "browse:setup",
            kind="setup",
            state=activity_state,
            summary=summary,
        )

    def command(
        self,
        arguments: Any,
        context: Mapping[str, Any],
        confirmation: Confirmation,
        *,
        cancellation: Any = None,
    ) -> str:
        if not isinstance(arguments, list) or len(arguments) > 1:
            raise BrowseError(
                "invalid_command",
                "Usage: /browse [setup|status|open|close|reset-profile]",
            )
        action = arguments[0] if arguments else "status"
        command_owner = self._optional_owner(context)
        command_owner_payload = (
            command_owner.as_dict() if command_owner is not None else None
        )
        if action == "setup":
            self.presentation.activity(
                "browse:setup",
                kind="setup",
                state="pending",
                summary="Waiting for browser setup confirmation",
                resource_owner=command_owner_payload,
            )
            confirmed = confirmation(
                "Download pinned Playwright 1.57.0 and its Chromium browser?",
                "Dependencies will be installed only under ~/.ygg/browse/. Setup continues in the background.",
                False,
            )
            if not confirmed:
                self.presentation.activity(
                    "browse:setup",
                    kind="setup",
                    state="cancelled",
                    summary="Browser setup was not authorized",
                    resource_owner=command_owner_payload,
                )
                return "Browser setup was denied or no interactive confirmation was available."
            status = self.setup.start()
            self._on_setup_state(status.as_dict())
            return (
                f"{status.detail}\nStatus: {status.state}\nInstall log: {status.log_path}\n"
                "Use /browse status; log contents are never returned to the model."
            )
        if action == "status":
            return self._status_text(command_owner, cancellation=cancellation)
        if action == "open":
            owner = ResourceOwner.from_context(context)
            result = self._worker_call(
                "launch",
                owner,
                cancellation=cancellation,
                timeout=CONFIRMATION_OPERATION_TIMEOUT,
                activity_id="browse:launch",
                kind="browser",
                running="Opening visible isolated browser",
                succeeded="Visible isolated browser is open",
            )
            return result["text"] + "\n" + self._tab_text(result)
        if action == "close":
            owner = ResourceOwner.from_context(context)
            result = self._worker_call(
                "close",
                owner,
                cancellation=cancellation,
                activity_id="browse:close",
                kind="browser",
                running="Closing isolated browser",
                succeeded="Isolated browser closed",
            )
            return result["text"]
        if action == "reset-profile":
            reset_owner = ResourceOwner.from_context(context)
            reset_owner_payload = reset_owner.as_dict()
            self.presentation.activity(
                "browse:reset",
                kind="browser",
                state="pending",
                summary="Waiting for destructive profile-reset confirmation",
                resource_owner=reset_owner_payload,
            )
            confirmed = confirmation(
                "Close Ygg Browse and permanently reset its isolated profile?",
                "Only a sentinel-verified ~/.ygg/browse/profile/ directory will be removed. Normal browser profiles are never inspected.",
                True,
            )
            if not confirmed:
                self.presentation.activity(
                    "browse:reset",
                    kind="browser",
                    state="cancelled",
                    summary="Profile reset was not authorized",
                    resource_owner=reset_owner_payload,
                )
                return "Profile reset was denied or no interactive confirmation was available."
            self._worker_call(
                "force_close",
                cancellation=cancellation,
                activity_id="browse:reset",
                kind="browser",
                running="Closing browser before profile reset",
                succeeded="Browser closed for profile reset",
            )
            removed = self.profile.reset()
            self.presentation.activity(
                "browse:reset",
                kind="browser",
                state="succeeded",
                summary="Isolated profile reset" if removed else "Isolated profile was already absent",
                resource_owner=reset_owner_payload,
            )
            return (
                "Closed Ygg Browse and removed its sentinel-verified isolated profile."
                if removed
                else "Closed Ygg Browse; its isolated profile was already absent."
            )
        raise BrowseError(
            "invalid_command",
            "Usage: /browse [setup|status|open|close|reset-profile]",
        )

    def browser_status(
        self, owner: ResourceOwner, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        setup_status = self.setup.status()
        self.presentation.update_setup(setup_status.as_dict())
        browser = self.worker.call(
            "status",
            owner,
            timeout=DEFAULT_OPERATION_TIMEOUT,
            cancellation=cancellation,
        )
        self.presentation.update_browser(browser, resource_owner=owner.as_dict())
        browser["text"] = self._format_status(setup_status.as_dict(), browser)
        browser["setup_state"] = setup_status.state
        browser["install_log"] = setup_status.log_path
        return browser

    def browser_launch(
        self, owner: ResourceOwner, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        return self._worker_call(
            "launch",
            owner,
            timeout=CONFIRMATION_OPERATION_TIMEOUT,
            cancellation=cancellation,
            activity_id="browse:launch",
            kind="browser",
            running="Opening visible isolated browser",
            succeeded="Visible isolated browser is open",
        )

    def browser_tabs(
        self, owner: ResourceOwner, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        result = self._worker_call(
            "tabs",
            owner,
            cancellation=cancellation,
            activity_id="browse:tabs",
            kind="browser",
            running="Reading bounded tab state",
            succeeded="Bounded tab state ready",
        )
        result["text"] = self._tab_text(result)
        return result

    def browser_open_url(
        self,
        owner: ResourceOwner,
        url: str,
        tab_id: Optional[str],
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        return self._worker_call(
            "open_url",
            owner,
            url,
            tab_id,
            timeout=NAVIGATION_TIMEOUT + 2,
            cancellation=cancellation,
            activity_id="browse:navigation",
            kind="navigation",
            running="Navigating an explicit HTTP(S) target",
            succeeded="Navigation settled",
        )

    def browser_snapshot(
        self, owner: ResourceOwner, tab_id: str, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        return self._worker_call(
            "snapshot",
            owner,
            tab_id,
            cancellation=cancellation,
            activity_id="browse:snapshot",
            kind="observation",
            running="Building bounded semantic snapshot",
            succeeded="Bounded semantic snapshot ready",
        )

    def browser_click(
        self,
        owner: ResourceOwner,
        tab_id: str,
        target: str,
        generation: Any,
        confirmation: Confirmation,
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        callback = self._consequential_callback(confirmation, owner)
        return self._worker_call(
            "click",
            owner,
            tab_id,
            target,
            generation,
            callback,
            timeout=CONFIRMATION_OPERATION_TIMEOUT,
            cancellation=cancellation,
            activity_id="browse:click",
            kind="action",
            running="Resolving one unique click target",
            succeeded="Click action settled",
        )

    def browser_type(
        self,
        owner: ResourceOwner,
        tab_id: str,
        target: str,
        generation: Any,
        value: str,
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        return self._worker_call(
            "type_text",
            owner,
            tab_id,
            target,
            generation,
            value,
            cancellation=cancellation,
            activity_id="browse:type",
            kind="action",
            running="Typing a withheld value into a non-credential field",
            succeeded="Typing settled; value withheld",
        )

    def browser_press(
        self,
        owner: ResourceOwner,
        tab_id: str,
        target: str,
        generation: Any,
        key: str,
        confirmation: Confirmation,
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        return self._worker_call(
            "press",
            owner,
            tab_id,
            target,
            generation,
            key,
            self._consequential_callback(confirmation, owner),
            timeout=CONFIRMATION_OPERATION_TIMEOUT,
            cancellation=cancellation,
            activity_id="browse:press",
            kind="action",
            running="Applying one allowlisted key to a unique target",
            succeeded="Allowlisted key action settled",
        )

    def browser_scroll(
        self,
        owner: ResourceOwner,
        tab_id: str,
        delta_x: int,
        delta_y: int,
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        return self._worker_call(
            "scroll",
            owner,
            tab_id,
            delta_x,
            delta_y,
            cancellation=cancellation,
            activity_id="browse:scroll",
            kind="action",
            running="Applying bounded page scroll",
            succeeded="Bounded page scroll settled",
        )

    def browser_wait(
        self,
        owner: ResourceOwner,
        tab_id: str,
        milliseconds: int,
        *,
        cancellation: Any = None,
    ) -> Dict[str, Any]:
        return self._worker_call(
            "wait",
            owner,
            tab_id,
            milliseconds,
            timeout=max(DEFAULT_OPERATION_TIMEOUT, milliseconds / 1000.0 + 2),
            cancellation=cancellation,
            activity_id="browse:wait",
            kind="wait",
            running="Waiting for a bounded interval",
            succeeded="Bounded wait settled",
        )

    def browser_screenshot(
        self, owner: ResourceOwner, tab_id: str, *, cancellation: Any = None
    ) -> ScreenshotRecord:
        result = self._worker_call(
            "screenshot",
            owner,
            tab_id,
            cancellation=cancellation,
            activity_id="browse:screenshot",
            kind="artifact",
            running="Capturing viewport-only screenshot",
            succeeded="Viewport screenshot captured",
        )
        return self.artifacts.save_png(result["data"])

    def screenshot_published(
        self,
        owner: ResourceOwner,
        artifact_id: str,
        record: ScreenshotRecord,
    ) -> None:
        self.presentation.activity(
            "browse:screenshot",
            kind="artifact",
            state="succeeded",
            summary="Viewport screenshot published",
            artifact_id=artifact_id,
            resource_owner=owner.as_dict(),
        )

    def browser_tab_close(
        self, owner: ResourceOwner, tab_id: str, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        return self._worker_call(
            "close_tab",
            owner,
            tab_id,
            cancellation=cancellation,
            activity_id="browse:tab-close",
            kind="browser",
            running="Closing explicit tab",
            succeeded="Explicit tab closed",
        )

    def browser_close(
        self, owner: ResourceOwner, *, cancellation: Any = None
    ) -> Dict[str, Any]:
        return self._worker_call(
            "close",
            owner,
            cancellation=cancellation,
            activity_id="browse:close",
            kind="browser",
            running="Closing isolated browser",
            succeeded="Isolated browser closed",
        )

    def shutdown(self) -> None:
        with self._shutdown_lock:
            if self._closed:
                return
            self._closed = True
            self.setup.shutdown(timeout=0.8)
            self.worker.shutdown(timeout=1.0)

    def _worker_call(
        self,
        method: str,
        *arguments: Any,
        timeout: float = DEFAULT_OPERATION_TIMEOUT,
        cancellation: Any = None,
        activity_id: str,
        kind: str,
        running: str,
        succeeded: str,
    ) -> Dict[str, Any]:
        presentation_owner = next(
            (argument.as_dict() for argument in arguments if isinstance(argument, ResourceOwner)),
            None,
        )
        self.presentation.activity(
            activity_id,
            kind=kind,
            state="running",
            summary=running,
            resource_owner=presentation_owner,
        )
        try:
            result = self.worker.call(
                method,
                *arguments,
                timeout=timeout,
                cancellation=cancellation,
            )
        except BaseException as error:
            degraded = method == "launch" or (
                isinstance(error, BrowseError)
                and error.code in {"browser_degraded", "browser_stopped"}
            )
            if degraded:
                self.presentation.mark_degraded(
                    "Visible isolated browser runtime is degraded; inspect /browse status and the local install log.",
                    resource_owner=presentation_owner,
                )
            self.presentation.activity(
                activity_id,
                kind=kind,
                state="failed",
                summary=bounded_text(succeeded.replace("settled", "failed"), 256),
                resource_owner=presentation_owner,
            )
            raise
        if isinstance(result, Mapping):
            self.presentation.update_browser(
                result, resource_owner=presentation_owner
            )
        self.presentation.activity(
            activity_id,
            kind=kind,
            state="succeeded",
            summary=succeeded,
            resource_owner=presentation_owner,
        )
        return dict(result)

    def _consequential_callback(
        self,
        confirmation: Confirmation,
        owner: ResourceOwner,
    ) -> Callable[[str, bool, str], bool]:
        owner_payload = owner.as_dict()

        def callback(category: str, destructive: bool, origin: str) -> bool:
            self.presentation.activity(
                "browse:confirmation",
                kind="confirmation",
                state="pending",
                summary="Waiting for consequential-action confirmation",
                resource_owner=owner_payload,
            )
            confirmed = confirmation(
                f"Allow the visible browser to {category}?",
                f"Origin: {origin}. Page labels are untrusted and cannot authorize this action.",
                destructive,
            )
            self.presentation.activity(
                "browse:confirmation",
                kind="confirmation",
                state="succeeded" if confirmed else "cancelled",
                summary="Consequential action confirmed"
                if confirmed
                else "Consequential action denied",
                resource_owner=owner_payload,
            )
            return confirmed

        return callback

    def _status_text(self, owner: Optional[ResourceOwner], *, cancellation: Any) -> str:
        setup_status = self.setup.status()
        self.presentation.update_setup(setup_status.as_dict())
        browser = self.worker.call(
            "status",
            owner,
            timeout=DEFAULT_OPERATION_TIMEOUT,
            cancellation=cancellation,
        )
        self.presentation.update_browser(
            browser,
            resource_owner=owner.as_dict() if owner is not None else None,
        )
        return self._format_status(setup_status.as_dict(), browser)

    def _format_status(self, setup: Mapping[str, Any], browser: Mapping[str, Any]) -> str:
        profile_health = self.profile.inspect()
        lines = [
            f"Browse setup: {setup.get('state', 'degraded')} · Playwright {PLAYWRIGHT_VERSION}",
            f"Browser: {'open' if browser.get('open') else 'closed'}",
            f"Browser health: {'degraded' if browser.get('degraded') else 'healthy'}",
            f"Tabs: {browser.get('tab_count', 0)}",
            f"Selected origin: {browser.get('selected_origin', 'unavailable')}",
            f"Profile health: {profile_health}",
            f"Install log: {setup.get('log_path', self.paths.display(self.paths.install_log))}",
            "Install log contents, profile paths, query strings, page text, and typed values are withheld.",
        ]
        if browser.get("open") and not browser.get("owner_matches", True):
            lines.append("The open browser belongs to a different host-derived resource owner.")
        return "\n".join(lines)

    @staticmethod
    def _tab_text(result: Mapping[str, Any]) -> str:
        tabs = result.get("tabs", [])
        lines = [UNTRUSTED_BEGIN]
        if isinstance(tabs, list):
            for tab in tabs[:64]:
                if not isinstance(tab, Mapping):
                    continue
                marker = " selected" if tab.get("selected") else ""
                lines.append(
                    "- %s%s · %s · %s · snapshot_generation=%s"
                    % (
                        bounded_text(tab.get("tab_id", "unknown"), 64),
                        marker,
                        bounded_text(tab.get("title", "Untitled"), 160),
                        bounded_text(tab.get("url", "unavailable"), 512),
                        tab.get("snapshot_generation")
                        if isinstance(tab.get("snapshot_generation"), int)
                        else "none",
                    )
                )
        if len(lines) == 1:
            lines.append("No tabs are open.")
        lines.append(UNTRUSTED_END)
        return "\n".join(lines)

    @staticmethod
    def _optional_owner(context: Mapping[str, Any]) -> Optional[ResourceOwner]:
        if not isinstance(context.get("resource_owner"), Mapping):
            return None
        return ResourceOwner.from_context(context)
