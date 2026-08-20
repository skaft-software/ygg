"""API 0.2 tool and command wiring for the official Ygg Browse bundle."""

from __future__ import annotations

from typing import Any, Dict, Mapping, Optional, Set, Tuple

from ygg_extension import (
    CancelledError,
    Extension,
    RpcError,
    current_cancellation,
    image_content,
    text_content,
    tool_result,
)

from .controller import BrowseController
from .presentation import BrowsePresentation, PresentationPublisher
from .safety import (
    BrowseError,
    MAX_TAB_ID_CHARS,
    MAX_TARGET_CHARS,
    MAX_URL_CHARS,
    ResourceOwner,
    bounded_text,
    require_integer,
    require_string,
    valid_tab_id,
)
from .worker import KEY_ALLOWLIST


EMPTY_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {},
    "additionalProperties": False,
}
TAB_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "tab_id": {"type": "string", "minLength": 1, "maxLength": MAX_TAB_ID_CHARS}
    },
    "required": ["tab_id"],
    "additionalProperties": False,
}
OPEN_URL_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "url": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_URL_CHARS,
            "description": "Explicit absolute HTTP(S) URL. Userinfo and every other scheme are rejected.",
        },
        "tab_id": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_TAB_ID_CHARS,
            "description": "Explicit existing tab to navigate. Omit only to create a new tab.",
        },
    },
    "required": ["url"],
    "additionalProperties": False,
}
TARGET_PROPERTIES: Dict[str, Any] = {
    "tab_id": {"type": "string", "minLength": 1, "maxLength": MAX_TAB_ID_CHARS},
    "target": {
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_TARGET_CHARS,
        "description": (
            "One unique ref=eN, role=button[name=\"Exact name\"], text=Exact text, css=selector, "
            "or exact plain semantic text target."
        ),
    },
    "snapshot_generation": {
        "type": "integer",
        "minimum": 0,
        "maximum": (2**53) - 1,
        "description": "Required and exact for ref=eN targets; stale generations fail closed.",
    },
}
CLICK_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": dict(TARGET_PROPERTIES),
    "required": ["tab_id", "target"],
    "additionalProperties": False,
}
TYPE_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        **TARGET_PROPERTIES,
        "text": {
            "type": "string",
            "maxLength": 4096,
            "description": "Value is never logged, echoed, or returned. Credential-like fields are refused.",
        },
    },
    "required": ["tab_id", "target", "text"],
    "additionalProperties": False,
}
PRESS_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        **TARGET_PROPERTIES,
        "key": {
            "type": "string",
            "enum": list(KEY_ALLOWLIST),
            "description": "Navigation-key allowlist only; clipboard and arbitrary modifier shortcuts are unavailable.",
        },
    },
    "required": ["tab_id", "target", "key"],
    "additionalProperties": False,
}
SCROLL_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "tab_id": {"type": "string", "minLength": 1, "maxLength": MAX_TAB_ID_CHARS},
        "delta_x": {"type": "integer", "minimum": -4000, "maximum": 4000, "default": 0},
        "delta_y": {"type": "integer", "minimum": -4000, "maximum": 4000},
    },
    "required": ["tab_id", "delta_y"],
    "additionalProperties": False,
}
WAIT_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "tab_id": {"type": "string", "minLength": 1, "maxLength": MAX_TAB_ID_CHARS},
        "milliseconds": {"type": "integer", "minimum": 0, "maximum": 5000},
    },
    "required": ["tab_id", "milliseconds"],
    "additionalProperties": False,
}


TOOL_DESCRIPTIONS = {
    "browser_status": "Report bounded setup/browser health and the local install-log path without returning log contents, page text, query strings, typed values, credentials, or profile paths.",
    "browser_launch": "Open Playwright's pinned Chromium visibly with the isolated persistent Ygg Browse profile. Headless/background launch and normal user profiles are unavailable.",
    "browser_tabs": "List explicit opaque tab IDs with bounded sanitized titles and URLs. Page-derived fields are marked as untrusted content.",
    "browser_open_url": "Create a tab or navigate one explicit tab to an absolute HTTP(S) URL. Userinfo, relative/non-HTTP schemes, unsafe redirects/popups, and downloads are blocked.",
    "browser_snapshot": "Return a bounded semantic snapshot (about 20000 characters and at most 100 interactive refs) wrapped as untrusted browser content, with a new snapshot generation and no input values.",
    "browser_click": "Click one uniquely resolved semantic locator or generation-matched snapshot ref. No coordinates; ambiguity and stale refs fail closed, and consequential external effects require action-time confirmation.",
    "browser_type": "Fill one uniquely resolved non-credential form field. Password, OTP, payment, authentication, and credential-like fields are manual-only; the typed value is never echoed.",
    "browser_press": "Apply one documented navigation key to a unique target. Clipboard/modifier shortcuts are unavailable and consequential Enter/Space actions require confirmation.",
    "browser_scroll": "Scroll one explicit tab by bounded horizontal/vertical distances without coordinate input or physical-pointer claims.",
    "browser_wait": "Wait in one explicit tab for at most 5000 milliseconds, then refresh bounded tab lifecycle state.",
    "browser_screenshot": "Capture only the current viewport when no tool-typed value or visible form/editable field could leak, retain it under Ygg-owned screenshot storage below 5 MiB, and publish an owner/generation-scoped API 0.2 artifact plus a read-compatible textual path.",
    "browser_tab_close": "Close one explicit opaque tab ID and invalidate its snapshot generation and refs.",
    "browser_close": "Close the visible isolated persistent browser context for the active host-derived owner and invalidate all tab state.",
}


def create_runtime(
    *, controller_factory: Optional[Any] = None
) -> Tuple[Extension, BrowseController, BrowsePresentation, PresentationPublisher]:
    extension = Extension(
        api_version="0.2",
        max_concurrent_requests=8,
        max_pending_requests=32,
        supported_features=("request_cancellation", "content_parts", "artifacts"),
    )
    publisher = PresentationPublisher(extension)
    presentation = BrowsePresentation(publisher)
    controller = (
        controller_factory(presentation)
        if controller_factory is not None
        else BrowseController(presentation)
    )

    def confirmation_callback() -> Any:
        parent_request_id = extension.request_id

        def confirm(prompt: str, detail: Optional[str], destructive: bool) -> bool:
            return extension.confirm(
                prompt,
                detail=detail,
                destructive=destructive,
                default=False,
                parent_request_id=parent_request_id,
            )

        return confirm

    def invoke(name: str, arguments: Any, context: Mapping[str, Any]) -> Dict[str, Any]:
        try:
            values = _arguments(arguments)
            owner = ResourceOwner.from_context(context)
            cancellation = current_cancellation()
            if name == "browser_status":
                _shape(values, set())
                result = controller.browser_status(owner, cancellation=cancellation)
            elif name == "browser_launch":
                _shape(values, set())
                result = controller.browser_launch(owner, cancellation=cancellation)
            elif name == "browser_tabs":
                _shape(values, set())
                result = controller.browser_tabs(owner, cancellation=cancellation)
            elif name == "browser_open_url":
                _shape(values, {"url", "tab_id"}, {"url"})
                url = require_string(values, "url", maximum=MAX_URL_CHARS)
                tab_id = _optional_tab(values)
                result = controller.browser_open_url(
                    owner, url, tab_id, cancellation=cancellation
                )
            elif name == "browser_snapshot":
                _shape(values, {"tab_id"}, {"tab_id"})
                result = controller.browser_snapshot(
                    owner, _tab(values), cancellation=cancellation
                )
            elif name == "browser_click":
                _shape(values, {"tab_id", "target", "snapshot_generation"}, {"tab_id", "target"})
                result = controller.browser_click(
                    owner,
                    _tab(values),
                    _target(values),
                    values.get("snapshot_generation"),
                    confirmation_callback(),
                    cancellation=cancellation,
                )
            elif name == "browser_type":
                _shape(
                    values,
                    {"tab_id", "target", "snapshot_generation", "text"},
                    {"tab_id", "target", "text"},
                )
                typed_value = values.get("text")
                if not isinstance(typed_value, str) or len(typed_value) > 4096:
                    raise BrowseError("invalid_arguments", "text exceeds the bounded input limit.")
                result = controller.browser_type(
                    owner,
                    _tab(values),
                    _target(values),
                    values.get("snapshot_generation"),
                    typed_value,
                    cancellation=cancellation,
                )
            elif name == "browser_press":
                _shape(
                    values,
                    {"tab_id", "target", "snapshot_generation", "key"},
                    {"tab_id", "target", "key"},
                )
                key = require_string(values, "key", maximum=32)
                result = controller.browser_press(
                    owner,
                    _tab(values),
                    _target(values),
                    values.get("snapshot_generation"),
                    key,
                    confirmation_callback(),
                    cancellation=cancellation,
                )
            elif name == "browser_scroll":
                _shape(values, {"tab_id", "delta_x", "delta_y"}, {"tab_id", "delta_y"})
                result = controller.browser_scroll(
                    owner,
                    _tab(values),
                    require_integer(values, "delta_x", minimum=-4000, maximum=4000, default=0),
                    require_integer(values, "delta_y", minimum=-4000, maximum=4000),
                    cancellation=cancellation,
                )
            elif name == "browser_wait":
                _shape(values, {"tab_id", "milliseconds"}, {"tab_id", "milliseconds"})
                result = controller.browser_wait(
                    owner,
                    _tab(values),
                    require_integer(values, "milliseconds", minimum=0, maximum=5000),
                    cancellation=cancellation,
                )
            elif name == "browser_screenshot":
                _shape(values, {"tab_id"}, {"tab_id"})
                if "artifacts" not in extension.negotiated_features:
                    raise BrowseError(
                        "artifacts_unavailable",
                        "The host did not negotiate API 0.2 artifacts; no screenshot was captured.",
                    )
                tab_id = _tab(values)
                record = controller.browser_screenshot(owner, tab_id, cancellation=cancellation)
                artifact_id = controller.artifacts.publish(extension, record)
                controller.screenshot_published(owner, artifact_id, record)
                path = controller.paths.display(record.path)
                return tool_result(
                    text_content(
                        f"Captured viewport-only screenshot for tab {tab_id}.\n"
                        f"Artifact: {artifact_id}\nRead-compatible local reference: {path}\n"
                        f"Size: {record.size} bytes (<5 MiB)."
                    ),
                    image_content(
                        artifact_id,
                        record.mime_type,
                        alt=f"Viewport screenshot for tab {tab_id}",
                    ),
                    metadata={
                        "schema": "ygg.browse.screenshot.v1",
                        "operation": name,
                        "tab_id": tab_id,
                        "artifact_id": artifact_id,
                        "size": record.size,
                    },
                )
            elif name == "browser_tab_close":
                _shape(values, {"tab_id"}, {"tab_id"})
                result = controller.browser_tab_close(
                    owner, _tab(values), cancellation=cancellation
                )
            elif name == "browser_close":
                _shape(values, set())
                result = controller.browser_close(owner, cancellation=cancellation)
            else:  # pragma: no cover - registration is closed and exact.
                raise BrowseError("unknown_operation", "Unknown browser operation.")
            return _tool_success(name, result)
        except CancelledError:
            raise
        except BrowseError as error:
            return _tool_error(name, error)
        except RpcError:
            return _tool_error(
                name,
                BrowseError(
                    "host_service_error",
                    "A required host confirmation or artifact service failed or was unavailable.",
                ),
            )
        except Exception as error:
            extension.log.error(
                "browse operation failed",
                operation=name,
                error_type=type(error).__name__,
            )
            return _tool_error(
                name,
                BrowseError("internal_error", "The bounded browser operation failed safely."),
            )

    @extension.tool(name="browser_status", description=TOOL_DESCRIPTIONS["browser_status"], parameters=EMPTY_SCHEMA)
    def browser_status(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_status", arguments, context)

    @extension.tool(name="browser_launch", description=TOOL_DESCRIPTIONS["browser_launch"], parameters=EMPTY_SCHEMA)
    def browser_launch(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_launch", arguments, context)

    @extension.tool(name="browser_tabs", description=TOOL_DESCRIPTIONS["browser_tabs"], parameters=EMPTY_SCHEMA)
    def browser_tabs(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_tabs", arguments, context)

    @extension.tool(name="browser_open_url", description=TOOL_DESCRIPTIONS["browser_open_url"], parameters=OPEN_URL_SCHEMA)
    def browser_open_url(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_open_url", arguments, context)

    @extension.tool(name="browser_snapshot", description=TOOL_DESCRIPTIONS["browser_snapshot"], parameters=TAB_SCHEMA)
    def browser_snapshot(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_snapshot", arguments, context)

    @extension.tool(name="browser_click", description=TOOL_DESCRIPTIONS["browser_click"], parameters=CLICK_SCHEMA)
    def browser_click(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_click", arguments, context)

    @extension.tool(name="browser_type", description=TOOL_DESCRIPTIONS["browser_type"], parameters=TYPE_SCHEMA)
    def browser_type(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_type", arguments, context)

    @extension.tool(name="browser_press", description=TOOL_DESCRIPTIONS["browser_press"], parameters=PRESS_SCHEMA)
    def browser_press(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_press", arguments, context)

    @extension.tool(name="browser_scroll", description=TOOL_DESCRIPTIONS["browser_scroll"], parameters=SCROLL_SCHEMA)
    def browser_scroll(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_scroll", arguments, context)

    @extension.tool(name="browser_wait", description=TOOL_DESCRIPTIONS["browser_wait"], parameters=WAIT_SCHEMA)
    def browser_wait(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_wait", arguments, context)

    @extension.tool(name="browser_screenshot", description=TOOL_DESCRIPTIONS["browser_screenshot"], parameters=TAB_SCHEMA)
    def browser_screenshot(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_screenshot", arguments, context)

    @extension.tool(name="browser_tab_close", description=TOOL_DESCRIPTIONS["browser_tab_close"], parameters=TAB_SCHEMA)
    def browser_tab_close(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_tab_close", arguments, context)

    @extension.tool(name="browser_close", description=TOOL_DESCRIPTIONS["browser_close"], parameters=EMPTY_SCHEMA)
    def browser_close(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Dict[str, Any]:
        return invoke("browser_close", arguments, context)

    @extension.command(
        name="browse",
        description="Set up, inspect, open, close, or safely reset the visible isolated Ygg Browse browser.",
        usage="/browse [setup|status|open|close|reset-profile]",
    )
    def browse_command(arguments: Any, context: Mapping[str, Any]) -> Dict[str, Any]:
        try:
            text = controller.command(
                arguments,
                context,
                confirmation_callback(),
                cancellation=current_cancellation(),
            )
            return {"text": bounded_text(text, 16_384, collapse_whitespace=False)}
        except CancelledError:
            raise
        except BrowseError as error:
            return {"text": f"Browse command failed [{error.code}]: {error.message}"}
        except RpcError:
            return {
                "text": "Browse command failed [host_service_error]: required host confirmation service failed or was unavailable."
            }
        except Exception as error:
            extension.log.error("browse command failed", error_type=type(error).__name__)
            return {"text": "Browse command failed [internal_error]: operation failed safely."}

    @extension.status("status")
    def browse_status_surface(_params: Mapping[str, Any]) -> Dict[str, Any]:
        # Status collection is process-scoped. It reports the already bounded
        # presentation cache and never allocates or claims owner-scoped state.
        presentation.publish()
        return {
            "surface": "status",
            "text": presentation.process_status(),
            "style_role": "extension.ygg_browse.status",
            "priority": 20,
        }

    @extension.on_shutdown
    def shutdown(_params: Mapping[str, Any]) -> None:
        controller.shutdown()
        publisher.close()

    return extension, controller, presentation, publisher


def _tool_success(name: str, result: Mapping[str, Any]) -> Dict[str, Any]:
    text = result.get("text")
    if not isinstance(text, str) or not text:
        text = f"{name} completed."
    metadata: Dict[str, Any] = {
        "schema": "ygg.browse.result.v1",
        "operation": name,
    }
    for key in (
        "affected_tab_id",
        "snapshot_generation",
        "element_count",
        "truncated",
        "tab_count",
        "selected_tab_id",
        "created_tab_ids",
        "closed_tab_ids",
        "download_blocked",
        "value_echoed",
        "setup_state",
        "degraded",
    ):
        if key in result:
            metadata[key] = result[key]
    return tool_result(text_content(bounded_text(text, 24_000, collapse_whitespace=False)), metadata=metadata)


def _tool_error(name: str, error: BrowseError) -> Dict[str, Any]:
    text = f"{name} failed [{error.code}]: {error.message}"
    if error.untrusted_detail:
        text += (
            "\nBEGIN UNTRUSTED BROWSER CONTENT\n"
            + error.untrusted_detail
            + "\nEND UNTRUSTED BROWSER CONTENT"
        )
    return tool_result(
        text_content(bounded_text(text, 8192, collapse_whitespace=False)),
        is_error=True,
        metadata={
            "schema": "ygg.browse.error.v1",
            "operation": name,
            "code": error.code,
        },
    )


def _arguments(value: Any) -> Dict[str, Any]:
    if not isinstance(value, Mapping):
        raise BrowseError("invalid_arguments", "Tool arguments must be an object.")
    return dict(value)


def _shape(
    values: Mapping[str, Any], allowed: Set[str], required: Optional[Set[str]] = None
) -> None:
    unknown = set(values) - allowed
    missing = (required or set()) - set(values)
    if unknown or missing:
        raise BrowseError("invalid_arguments", "Tool arguments do not match the declared schema.")


def _tab(values: Mapping[str, Any]) -> str:
    value = require_string(values, "tab_id", maximum=MAX_TAB_ID_CHARS)
    if not valid_tab_id(value):
        raise BrowseError("invalid_tab", "tab_id must be an opaque ID returned by Ygg Browse.")
    return value


def _optional_tab(values: Mapping[str, Any]) -> Optional[str]:
    return _tab(values) if "tab_id" in values else None


def _target(values: Mapping[str, Any]) -> str:
    return require_string(values, "target", maximum=MAX_TARGET_CHARS)


def main() -> None:
    extension, _controller, _presentation, _publisher = create_runtime()
    extension.run()
