#!/usr/bin/env node

/**
 * Ygg's deliberately small Pi compatibility host.
 *
 * This process is a compatibility boundary, not a second agent kernel. It
 * loads Pi extension factories with Pi's public loader and translates the
 * portable tool, command, lifecycle, notification, input, and confirmation
 * surfaces onto Ygg's API 0.3 JSON-RPC bus.
 */

import { AsyncLocalStorage } from "node:async_hooks";
import { Console } from "node:console";
import { createHash } from "node:crypto";
import {
  closeSync,
  constants as fsConstants,
  existsSync,
  fstatSync,
  lstatSync,
  openSync,
  opendirSync,
  readSync,
  realpathSync,
} from "node:fs";
import { homedir } from "node:os";
import {
  delimiter,
  dirname,
  isAbsolute,
  join,
  relative as relativePath,
  resolve,
  sep,
} from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";

const API_VERSION = "0.3";
const BRIDGE_VERSION = "0.3.0";
const LOCK_SCHEMA_VERSION = 2;
const PROFILE_ID = "pi-0.84.4";
const PROFILE_REPOSITORY = "https://github.com/earendil-works/pi.git";
const PROFILE_REVISION = "b79e4cc834970cca69daebffab7df1da7d1e52c4";
const PROFILE_TAG = "v0.84.4";
const SUPPORTED_PI_PACKAGE = "@earendil-works/pi-coding-agent";
const SUPPORTED_PI_VERSION = "0.84.4";
const SUPPORTED_PI_INTEGRITY = "sha512-jmOlrqUmvhh/siNWFRXjYLJzhKFIHNsAQaysRwzQPQFnPAaV/vhqHsLH/MBsIISA1Rjj7WTUFR3nJrpXoLx39w==";
const SUPPORTED_TUI_PACKAGE = "@earendil-works/pi-tui";
const SUPPORTED_TUI_INTEGRITY = "sha512-nPUnwDkLtupPXnZQYrCwPFcuTydCDqTY6ZbFqhsL4S4kVq0AT418kPa/6uXwtaCD+MjBNBltb7ScTYX65yeE1w==";
const MINIMUM_NODE_VERSION = [22, 19, 0];
const MAX_PI_PACKAGE_MANIFEST_BYTES = 256 * 1024;
const MAX_PI_LOCK_BYTES = 256 * 1024;
const MAX_GENERATED_FILE_BYTES = 4 * 1024 * 1024;
const MAX_AGGREGATE_SOURCES = 256;
const AGGREGATE_DIGEST_ENV = "YGG_PI_AGGREGATE_DIGEST";
const SOURCE_FINGERPRINT_FORMAT = 1;
const MAX_SOURCE_FILES = 4096;
const MAX_SOURCE_ENTRIES = 8192;
const MAX_SOURCE_DEPTH = 64;
const MAX_SOURCE_PATH_BYTES = 4096;
const MAX_SOURCE_BYTES = 64 * 1024 * 1024;
const SKIPPED_SOURCE_DIRECTORIES = new Set([
  ".git",
  ".pytest_cache",
  "__pycache__",
  "node_modules",
  "target",
]);
const REQUIRED_FEATURES = [
  "request_cancellation",
  "content_parts",
  "owner_context",
  "ordered_events",
  "catalog_transactions",
  "effect_transactions",
  "document_streams",
];
const OPTIONAL_FEATURES = [
  "request_progress",
  "artifacts",
  "policy_intents",
];
const PI_EVENT_NAMES = [
  "project_trust",
  "resources_discover",
  "session_start",
  "session_info_changed",
  "session_before_switch",
  "session_before_fork",
  "session_before_compact",
  "session_compact",
  "session_compact_failed",
  "session_shutdown",
  "session_before_tree",
  "session_tree",
  "context",
  "before_provider_request",
  "before_provider_headers",
  "after_provider_response",
  "before_agent_start",
  "agent_start",
  "agent_end",
  "agent_settled",
  "ui_prompt_start",
  "ui_prompt_end",
  "turn_start",
  "turn_end",
  "message_start",
  "message_update",
  "message_end",
  "tool_execution_start",
  "tool_execution_update",
  "tool_execution_end",
  "model_select",
  "thinking_level_select",
  "user_bash",
  "input",
  "tool_call",
  "tool_result",
];
const LIFECYCLE_EVENTS = [
  "session/started",
  "session/settled",
  "turn/started",
  "turn/settled",
  "tool/started",
  "tool/settled",
];

const args = parseArgs(process.argv.slice(2));
const protocolWrite = process.stdout.write.bind(process.stdout);
const scopes = new AsyncLocalStorage();
const pendingHostRequests = new Map();
const inflight = new Map();
let nextHostRequestId = 1;
let outputChain = Promise.resolve();
let orderedInputChain = Promise.resolve();
let bridge;

class CancellationError extends Error {
  constructor(message = "Pi compatibility request cancelled") {
    super(message);
    this.name = "CancellationError";
    this.code = -32800;
  }
}

function installProtocolSafeConsole() {
  // Reserve fd 1 for JSON-RPC even when unchanged Pi extensions write to the
  // Node stdout stream directly instead of using console.log.
  process.stdout.write = process.stderr.write.bind(process.stderr);
  globalThis.console = new Console({
    stdout: process.stderr,
    stderr: process.stderr,
    colorMode: false,
  });
}

installProtocolSafeConsole();

function parseVersionTuple(value, label) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/.exec(String(value));
  if (!match) throw new Error(`${label} has invalid semantic version ${JSON.stringify(value)}`);
  return match.slice(1).map((part) => Number.parseInt(part, 10));
}

function compareVersionTuple(left, right) {
  for (let index = 0; index < 3; index += 1) {
    if (left[index] !== right[index]) return left[index] - right[index];
  }
  return 0;
}

function validateNodeRuntime() {
  const observed = parseVersionTuple(process.versions.node, "Node runtime");
  if (compareVersionTuple(observed, MINIMUM_NODE_VERSION) < 0) {
    throw new Error(
      `Pi ${SUPPORTED_PI_VERSION} compatibility requires Node >=${MINIMUM_NODE_VERSION.join(".")}; found ${process.versions.node}`,
    );
  }
}

function parseArgs(argv) {
  const result = {
    extensions: [],
    sourceFingerprints: [],
    agentDir: null,
    cwd: null,
    commandName: "pi",
    piPackage: null,
    lock: null,
  };
  let legacySelectorSeen = false;
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--lock") {
      if (result.lock) throw new Error("--lock may be provided only once");
      result.lock = argv[++index];
      if (!result.lock) throw new Error("--lock requires a path");
    } else if (value === "--extension" || value === "-e") {
      legacySelectorSeen = true;
      const extension = argv[++index];
      if (!extension) throw new Error("--extension requires a path");
      result.extensions.push(extension);
    } else if (value === "--source-fingerprint") {
      legacySelectorSeen = true;
      const fingerprint = argv[++index];
      if (!fingerprint) throw new Error("--source-fingerprint requires a SHA-256 digest");
      if (!/^[0-9a-f]{64}$/.test(fingerprint)) {
        throw new Error("--source-fingerprint requires a lowercase SHA-256 digest");
      }
      result.sourceFingerprints.push(fingerprint);
    } else if (value === "--agent-dir") {
      legacySelectorSeen = true;
      result.agentDir = argv[++index];
      if (!result.agentDir) throw new Error("--agent-dir requires a path");
    } else if (value === "--cwd") {
      result.cwd = argv[++index];
      if (!result.cwd) throw new Error("--cwd requires a path");
    } else if (value === "--pi-package") {
      legacySelectorSeen = true;
      result.piPackage = argv[++index];
      if (!result.piPackage) throw new Error("--pi-package requires a path");
    } else if (value === "--command") {
      legacySelectorSeen = true;
      result.commandName = argv[++index];
      if (!result.commandName) throw new Error("--command requires a name");
    } else if (value === "--help" || value === "-h") {
      process.stdout.write(
        "Usage: bridge.mjs --lock PATH [--cwd DIR]\n       bridge.mjs --extension PATH [--source-fingerprint SHA256] [--extension PATH ...] [--agent-dir DIR] [--pi-package DIR]\n",
      );
      process.exit(0);
    } else {
      throw new Error(`unknown bridge argument ${value}`);
    }
  }
  if (result.lock && legacySelectorSeen) {
    throw new Error("--lock cannot be combined with extension, fingerprint, agent, package, or command selectors");
  }
  if (result.lock) return result;
  if (result.extensions.length === 0 && process.env.YGG_PI_EXTENSION) {
    result.extensions.push(process.env.YGG_PI_EXTENSION);
  }
  if (result.extensions.length === 0) {
    throw new Error("at least one --extension path or one --lock path is required");
  }
  if (
    result.sourceFingerprints.length !== 0
    && result.sourceFingerprints.length !== result.extensions.length
  ) {
    throw new Error("provide exactly one --source-fingerprint for each --extension");
  }
  return result;
}

function keyOf(id) {
  return typeof id === "string" ? `s:${id}` : `n:${String(id)}`;
}

function parentRequestId() {
  const value = scopes.getStore()?.parentRequestId;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error("Pi compatibility host has no active Ygg request owner");
  }
  return value;
}

function send(message) {
  const line = `${JSON.stringify(message)}\n`;
  outputChain = outputChain.then(
    () =>
      new Promise((resolveWrite, rejectWrite) => {
        protocolWrite(line, (error) =>
          error ? rejectWrite(error) : resolveWrite(),
        );
      }),
  );
  return outputChain;
}

function diagnostic(message) {
  process.stderr.write(`${String(message).replaceAll("\n", " ")}\n`);
}

function boundedDiagnostic(message, maxBytes = 4096) {
  const text = String(message).replaceAll("\n", " ");
  diagnostic(Buffer.byteLength(text) <= maxBytes ? text : `${Buffer.from(text).subarray(0, maxBytes).toString("utf8")}…`);
}

async function requestHost(method, params) {
  const scope = scopes.getStore();
  const signal = scope?.signal;
  if (signal?.aborted) throw new CancellationError();

  const id = `pi:${nextHostRequestId++}`;
  const request = {
    jsonrpc: "2.0",
    id,
    method,
    params,
  };
  let rejectPending;
  const promise = new Promise((resolveReply, rejectReply) => {
    rejectPending = rejectReply;
    pendingHostRequests.set(id, { resolve: resolveReply, reject: rejectReply });
  });
  const onAbort = () => {
    if (!pendingHostRequests.delete(id)) return;
    rejectPending(new CancellationError());
    void send({
      jsonrpc: "2.0",
      method: "$/cancelRequest",
      params: { id, reason: "parent_cancelled" },
    }).catch((error) => diagnostic(`Pi compatibility cancellation send failed: ${error}`));
  };
  signal?.addEventListener("abort", onAbort, { once: true });
  if (signal?.aborted) onAbort();

  if (pendingHostRequests.has(id)) {
    try {
      await send(request);
    } catch (error) {
      pendingHostRequests.delete(id);
      signal?.removeEventListener("abort", onAbort);
      throw error;
    }
  }

  try {
    return await promise;
  } finally {
    pendingHostRequests.delete(id);
    signal?.removeEventListener("abort", onAbort);
  }
}

async function callHostService(service, scope, payload = {}) {
  const response = await requestHost("host/call", {
    operation_token: operationToken(),
    service,
    version: 1,
    scope,
    payload,
  });
  if (response?.status !== "success") {
    throw new Error(response?.message ?? `Ygg host service ${service}:${scope} failed`);
  }
  return response.value;
}

function currentScope() {
  const scope = scopes.getStore();
  if (!scope) throw new Error("Pi compatibility API used outside an active request");
  return scope;
}

function operationToken() {
  const token = currentScope().invocation?.operation;
  if (!token || typeof token !== "object") {
    throw new Error("Pi compatibility API requires an API 0.3 operation context");
  }
  return token;
}

function recordEffect(effect) {
  const scope = currentScope();
  if (!scope.invocation) {
    throw new Error("Pi compatibility mutation used outside an API 0.3 operation");
  }
  if (scope.effects.length >= 128) {
    throw new Error("Pi compatibility effect journal exceeds 128 effects");
  }
  scope.effects.push(effect);
}

function effectJournal() {
  const scope = currentScope();
  return {
    operation_token: operationToken(),
    effects: scope.effects.slice(),
  };
}

function finishObjectResult(result) {
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    throw new Error("Pi compatibility API 0.3 handler result must be an object");
  }
  return { ...result, effects: effectJournal() };
}

function notify(level, title, message) {
  return send({
    jsonrpc: "2.0",
    method: "notification",
    params: { level, title, message: String(message) },
  });
}

function progress(message, current, total) {
  if (!bridge?.features?.has("request_progress")) return Promise.resolve();
  const scope = currentScope();
  scope.progressSequence += 1;
  return send({
    jsonrpc: "2.0",
    method: "$/progress",
    params: {
      request_id: scope.parentRequestId,
      sequence: scope.progressSequence,
      event: {
        type: "status",
        message: String(message),
        ...(current === undefined ? {} : { current }),
        ...(total === undefined ? {} : { total }),
        unit: "updates",
      },
    },
  });
}

function unsupported(name) {
  throw new Error(`Pi compatibility API is not supported by Ygg: ${name}`);
}

function makeCompatibilityTheme() {
  const text = (...values) => String(values.at(-1) ?? "");
  return makeThrowingProxy("ctx.ui.theme", {
    fg: text,
    bg: text,
    bold: text,
    dim: text,
    italic: text,
    underline: text,
    strikethrough: text,
    inverse: text,
  });
}

function makeRemoteTui() {
  return makeThrowingProxy("ctx.ui.remoteTui", {
    requestRender() {},
    terminal: { columns: 80, rows: 24 },
    get width() { return 80; },
    get height() { return 24; },
  });
}

function remoteComponentValue(kind, key, content, options = {}) {
  if (content === undefined) return null;
  let component;
  let rows;
  if (Array.isArray(content)) {
    rows = content.map(String);
  } else if (typeof content === "function") {
    component = content(makeRemoteTui(), makeCompatibilityTheme(), makeThrowingProxy("keybindings"));
    if (!component || typeof component.render !== "function") {
      throw new Error(`Pi ${kind} factory did not return a renderable component`);
    }
    rows = component.render(80).map(String);
  } else {
    throw new Error(`Pi ${kind} content must be string rows or a component factory`);
  }
  const id = `${kind}:${String(key ?? "default")}`;
  bridge.remoteComponents.set(id, component ?? null);
  return {
    component_id: id,
    generation: bridge.remoteComponentGeneration,
    revision: (bridge.remoteComponentRevisions.get(id) ?? 0) + 1,
    width: 80,
    rows,
    placement: options.placement ?? null,
  };
}

function setRemoteUiState(key, value) {
  if (value?.component_id) {
    bridge.remoteComponentRevisions.set(value.component_id, value.revision);
  }
  recordEffect({ type: "set_ui_state", key, value });
}

function makeUi() {
  return {
    theme: makeCompatibilityTheme(),
    async select(title, options) {
      const prompt = `${title}\n${options.map((value, i) => `${i + 1}. ${value}`).join("\n")}\nSelect an option by name or number:`;
      const value = await this.input(prompt);
      if (value === undefined) return undefined;
      const index = Number.parseInt(value, 10);
      return Number.isInteger(index) && index >= 1 && index <= options.length
        ? options[index - 1]
        : options.find((option) => option === value);
    },
    async confirm(title, message) {
      const response = await requestHost("confirmation/request", {
        parent_request_id: parentRequestId(),
        prompt: String(title),
        detail: String(message),
        destructive: false,
        default: false,
      });
      return response?.confirmed === true;
    },
    async input(title, placeholder) {
      const prompt = placeholder ? `${String(title)} (${String(placeholder)})` : String(title);
      const response = await requestHost("input/request", {
        parent_request_id: parentRequestId(),
        prompt,
        secret: false,
      });
      return response?.value ?? undefined;
    },
    async editor(title, prefill = "") {
      const response = await requestHost("host/call", {
        operation_token: operationToken(),
        service: "ui",
        version: 1,
        scope: "editor",
        payload: { action: "open", title: String(title), prefill: String(prefill) },
      });
      if (response?.status !== "success") {
        throw new Error(response?.message ?? "remote editor is unavailable");
      }
      return response.value?.value ?? undefined;
    },
    notify(message, type = "info") {
      void notify(type, "Pi extension", message);
    },
    onTerminalInput() {
      return unsupported("ctx.ui.onTerminalInput");
    },
    setStatus(key, text) {
      recordEffect({
        type: "set_ui_state",
        key: "status",
        value: { key: String(key), text: text === undefined ? null : String(text) },
      });
    },
    setWorkingMessage(message) {
      recordEffect({ type: "set_ui_state", key: "working_message", value: message ?? null });
    },
    setWorkingVisible(visible) {
      recordEffect({ type: "set_ui_state", key: "working_visible", value: Boolean(visible) });
    },
    setWorkingIndicator(options) {
      recordEffect({ type: "set_ui_state", key: "working_indicator", value: options ?? null });
    },
    setHiddenThinkingLabel(label) {
      recordEffect({ type: "set_ui_state", key: "hidden_thinking_label", value: label ?? null });
    },
    setWidget(key, content, options) {
      setRemoteUiState("widget", remoteComponentValue("widget", key, content, options));
    },
    setFooter(factory) {
      setRemoteUiState("footer", remoteComponentValue("footer", "footer", factory));
    },
    setHeader(factory) {
      setRemoteUiState("header", remoteComponentValue("header", "header", factory));
    },
    setTitle(title) {
      recordEffect({ type: "set_ui_state", key: "title", value: String(title) });
    },
    async custom(factory, options = {}) {
      let completed = false;
      let completedValue;
      const done = (value) => {
        completed = true;
        completedValue = value;
      };
      const component = await factory(
        makeRemoteTui(),
        makeCompatibilityTheme(),
        makeThrowingProxy("keybindings"),
        done,
      );
      const frame = remoteComponentValue("custom", "focused", () => component, options.overlayOptions ?? {});
      setRemoteUiState("custom", { ...frame, overlay: options.overlay === true });
      if (completed) return completedValue;
      const response = await requestHost("host/call", {
        operation_token: operationToken(),
        service: "ui",
        version: 1,
        scope: "components",
        payload: { action: "focus", frame, overlay: options.overlay === true },
      });
      if (response?.status !== "success") {
        throw new Error(response?.message ?? "remote component host rejected the component");
      }
      return response.value?.result;
    },
    pasteToEditor(text) {
      bridge.editorText += String(text);
      recordEffect({ type: "set_ui_state", key: "editor_text", value: bridge.editorText });
    },
    setEditorText(text) {
      bridge.editorText = String(text);
      recordEffect({ type: "set_ui_state", key: "editor_text", value: bridge.editorText });
    },
    getEditorText() {
      return bridge.editorText;
    },
    addAutocompleteProvider(factory) {
      bridge.autocompleteProviders.push(factory);
      recordEffect({
        type: "set_ui_state",
        key: "autocomplete",
        value: { provider_count: bridge.autocompleteProviders.length },
      });
    },
    setAutocompleteProvider(factory) {
      bridge.autocompleteProviders = factory ? [factory] : [];
      recordEffect({
        type: "set_ui_state",
        key: "autocomplete",
        value: { provider_count: bridge.autocompleteProviders.length },
      });
    },
    setEditorComponent(factory) {
      bridge.editorComponent = factory;
      const value = factory
        ? remoteComponentValue("editor", "editor", () => factory(
            makeRemoteTui(),
            makeThrowingProxy("editorTheme"),
            makeThrowingProxy("keybindings"),
          ))
        : null;
      setRemoteUiState("editor_component", value);
    },
    getEditorComponent() {
      return bridge.editorComponent;
    },
    getAllThemes() {
      return [...bridge.themes.values()].map((theme) => ({ name: theme.name, path: theme.path }));
    },
    getTheme(name) {
      return bridge.themes.get(String(name))?.theme;
    },
    setTheme(theme) {
      const name = typeof theme === "string" ? theme : theme?.name ?? "custom";
      if (typeof theme === "string" && !bridge.themes.has(name)) {
        return { success: false, error: `Unknown theme: ${name}` };
      }
      bridge.currentTheme = typeof theme === "string" ? bridge.themes.get(name).theme : theme;
      recordEffect({ type: "set_ui_state", key: "theme", value: { name } });
      return { success: true };
    },
    getToolsExpanded() {
      return bridge.toolsExpanded;
    },
    setToolsExpanded(expanded) {
      bridge.toolsExpanded = Boolean(expanded);
      recordEffect({ type: "set_ui_state", key: "disclosure", value: bridge.toolsExpanded });
    },
  };
}

function makeThrowingProxy(label, values = {}) {
  return new Proxy(values, {
    get(target, property) {
      if (property in target) return target[property];
      return () => unsupported(`${label}.${String(property)}`);
    },
  });
}

function updateHostStateFromMessage(message) {
  const state = message?.params?.context?.host ?? message?.params?.payload?.host;
  if (state && typeof state === "object" && !Array.isArray(state)) {
    bridge.hostState = { ...bridge.hostState, ...state };
    if (state.model) {
      bridge.model = bridge.models.find((model) => model.id === state.model)
        ?? { id: String(state.model), provider: String(state.model).split("/")[0] };
    }
  }
  const payload = message?.params?.payload;
  if (payload?.session && typeof payload.session === "object") {
    const session = payload.session;
    if (Array.isArray(session.entries)) bridge.sessionSnapshot.entries = session.entries.slice();
    if (Array.isArray(session.tree)) bridge.sessionSnapshot.tree = session.tree.slice();
    if (Object.hasOwn(session, "leaf_id")) bridge.sessionSnapshot.leafId = session.leaf_id;
    if (session.header) bridge.sessionSnapshot.header = session.header;
  }
  const mode = message?.params?.invocation?.operation?.mode;
  if (mode) {
    const piMode = mode === "tui" ? "tui" : mode === "print" ? "print" : "rpc";
    bridge.runner?.setUIContext(makeUi(), piMode);
  }
}

function thinkingLevelFromHost() {
  const serialized = JSON.stringify(bridge.hostState?.reasoning ?? "off").toLowerCase();
  for (const level of ["ultra", "max", "xhigh", "high", "medium", "low", "minimal"]) {
    if (serialized.includes(level)) return level;
  }
  return "off";
}

function makeExtensionContextActions() {
  return {
    getModel: () => bridge.model,
    getScopedModels: () => bridge.scopedModels,
    isIdle: () => bridge.agentActive !== true,
    isProjectTrusted: () => bridge.hostState?.project_trusted === true,
    getSignal: () => scopes.getStore()?.signal,
    abort: () => currentScope().controller.abort(),
    hasPendingMessages: () => bridge.pendingMessages > 0,
    shutdown() {
      void requestHost("host/call", {
        operation_token: operationToken(),
        service: "control",
        version: 1,
        scope: "shutdown",
        payload: {},
      }).catch((error) => boundedDiagnostic(`Pi shutdown request failed: ${error}`));
    },
    getContextUsage: () => bridge.hostState?.context_usage,
    compact(options = {}) {
      void requestHost("host/call", {
        operation_token: operationToken(),
        service: "session",
        version: 1,
        scope: "compact",
        payload: { custom_instructions: options.customInstructions ?? null },
      }).catch((error) => options.onError?.(error));
    },
    getSystemPrompt: () => String(bridge.hostState?.system_prompt ?? ""),
    getSystemPromptOptions: () => bridge.hostState?.system_prompt_options ?? { cwd: bridge.cwd },
  };
}

function makeExtensionActions() {
  return {
    sendMessage(message, options = {}) {
      recordEffect({
        type: "append_custom_message",
        custom_type: String(message.customType),
        content: typeof message.content === "string" ? message.content : textFromContent(message.content),
        ...(message.display === undefined ? {} : { display: String(message.display) }),
        details: message.details ?? null,
      });
      if (options.triggerTurn || options.deliverAs) {
        recordEffect({
          type: "enqueue_message",
          delivery: options.deliverAs === "followUp"
            ? "follow_up"
            : options.deliverAs === "nextTurn"
              ? "next_turn"
              : "steer",
          content: typeof message.content === "string" ? message.content : textFromContent(message.content),
        });
      }
    },
    sendUserMessage(content, options = {}) {
      const text = typeof content === "string" ? content : textFromContent(content);
      if (!text) throw new Error("Pi sendUserMessage requires text content in the Ygg bridge");
      recordEffect({
        type: "enqueue_message",
        delivery: options.deliverAs === "followUp" ? "follow_up" : "user",
        content: text,
      });
    },
    appendEntry(customType, data) {
      recordEffect({ type: "append_custom", custom_type: String(customType), details: data ?? null });
    },
    setSessionName(name) {
      bridge.hostState.session_name = name === undefined ? null : String(name);
      recordEffect({ type: "set_session_name", name: bridge.hostState.session_name });
    },
    getSessionName: () => bridge.hostState?.session_name ?? undefined,
    setLabel(entryId, label) {
      recordEffect({
        type: "set_entry_label",
        entry_id: String(entryId),
        label: label === undefined ? null : String(label),
      });
    },
    getActiveTools: () => bridge.activeToolNames.slice(),
    getAllTools: () => bridge.toolInfos,
    setActiveTools(toolNames) {
      bridge.activeToolNames = toolNames.map(String);
      recordEffect({ type: "set_active_tools", tools: bridge.activeToolNames.slice() });
    },
    refreshTools: () => scheduleCatalogRefresh(),
    getCommands: () => bridge.runner?.getRegisteredCommands?.() ?? [],
    async setModel(model) {
      const modelId = String(model?.id ?? model?.apiName ?? model);
      recordEffect({ type: "select_model", model: modelId });
      bridge.model = model;
      return true;
    },
    getThinkingLevel: () => thinkingLevelFromHost(),
    setThinkingLevel(level) {
      bridge.hostState.reasoning = String(level);
      recordEffect({ type: "select_reasoning", reasoning: String(level) });
    },
  };
}

function makeModelRegistry() {
  return makeThrowingProxy("ctx.modelRegistry", {
    async refresh() {
      const response = await requestHost("host/call", {
        operation_token: operationToken(),
        service: "providers",
        version: 1,
        scope: "refresh-models",
        payload: {},
      });
      return response?.value ?? { providers: 0, models: 0 };
    },
    getError: () => undefined,
    getAll: () => bridge.models.slice(),
    getAvailable: () => bridge.models.slice(),
    find: (provider, modelId) => bridge.models.find(
      (model) => model.provider === provider && model.id === modelId,
    ),
    hasConfiguredAuth: (model) => model?.available !== false,
    getProvider: (provider) => bridge.providers.get(String(provider))?.config,
    getProviderDisplayName: (provider) => bridge.providers.get(String(provider))?.config?.name ?? String(provider),
    getRegisteredProviderConfig: (provider) => bridge.providers.get(String(provider))?.config,
    getRegisteredProviderIds: () => [...bridge.providers.keys()],
    registerProvider(name, config) {
      registerProviderLocal(name, config);
    },
    unregisterProvider(name) {
      unregisterProviderLocal(name);
    },
  });
}

function makeSessionManager() {
  const syntheticId = () => `pi-entry-${bridge.nextSyntheticEntryId++}`;
  const appendLocal = (entry) => {
    const value = {
      id: syntheticId(),
      parentId: bridge.sessionSnapshot.leafId,
      timestamp: new Date().toISOString(),
      ...entry,
    };
    bridge.sessionSnapshot.entries.push(value);
    bridge.sessionSnapshot.leafId = value.id;
    return value.id;
  };
  return makeThrowingProxy("ctx.sessionManager", {
    getCwd: () => bridge.cwd,
    getSessionDir: () => "",
    getSessionId: () => bridge.hostState?.session_id ?? "ygg-session",
    getSessionFile: () => bridge.hostState?.session_file,
    getSessionName: () => bridge.hostState?.session_name ?? undefined,
    getLeafId: () => bridge.sessionSnapshot.leafId,
    getLeafEntry: () => bridge.sessionSnapshot.entries.find(
      (entry) => entry.id === bridge.sessionSnapshot.leafId,
    ),
    getEntry: (id) => bridge.sessionSnapshot.entries.find((entry) => entry.id === id),
    getChildren: (id) => bridge.sessionSnapshot.entries.filter((entry) => entry.parentId === id),
    getLabel: (id) => bridge.sessionSnapshot.labels.get(String(id)),
    getBranch() {
      const byId = new Map(bridge.sessionSnapshot.entries.map((entry) => [entry.id, entry]));
      const branch = [];
      let id = bridge.sessionSnapshot.leafId;
      while (id) {
        const entry = byId.get(id);
        if (!entry) break;
        branch.push(entry);
        id = entry.parentId;
      }
      return branch.reverse();
    },
    buildContextEntries() {
      return this.getBranch();
    },
    buildSessionContext() {
      return {
        messages: this.getBranch().filter((entry) => entry.type === "message").map((entry) => entry.message),
        thinkingLevel: thinkingLevelFromHost(),
        model: bridge.model ? { provider: bridge.model.provider, modelId: bridge.model.id } : null,
      };
    },
    getHeader: () => bridge.sessionSnapshot.header,
    getEntries: () => bridge.sessionSnapshot.entries.slice(),
    getTree: () => bridge.sessionSnapshot.tree.slice(),
    appendCustomEntry(customType, data) {
      recordEffect({ type: "append_custom", custom_type: String(customType), details: data ?? null });
      return appendLocal({ type: "custom", customType: String(customType), data });
    },
    appendCustomMessageEntry(customType, content, display, details) {
      const text = typeof content === "string" ? content : textFromContent(content);
      recordEffect({
        type: "append_custom_message",
        custom_type: String(customType),
        content: text,
        display: display ? text : "",
        details: details ?? null,
      });
      return appendLocal({ type: "custom_message", customType, content, display, details });
    },
    appendSessionInfo(name) {
      bridge.hostState.session_name = String(name);
      recordEffect({ type: "set_session_name", name: String(name) });
      return appendLocal({ type: "session_info", name: String(name) });
    },
    appendLabelChange(targetId, label) {
      bridge.sessionSnapshot.labels.set(String(targetId), label);
      recordEffect({
        type: "set_entry_label",
        entry_id: String(targetId),
        label: label === undefined ? null : String(label),
      });
      return appendLocal({ type: "label", targetId: String(targetId), label });
    },
    branch(id) {
      if (!bridge.sessionSnapshot.entries.some((entry) => entry.id === id)) {
        throw new Error(`Unknown session entry: ${id}`);
      }
      bridge.sessionSnapshot.leafId = id;
    },
    resetLeaf() {
      bridge.sessionSnapshot.leafId = null;
    },
  });
}

function unsignedBigEndian(value, bytes) {
  const buffer = Buffer.alloc(bytes);
  if (bytes === 4) buffer.writeUInt32BE(value);
  else buffer.writeBigUInt64BE(BigInt(value));
  return buffer;
}

function sourceRelativePath(root, path) {
  const relative = relativePath(root, path);
  const components = relative.split(sep);
  if (
    !relative
    || components.length > MAX_SOURCE_DEPTH
    || components.some((component) => !component || component === "." || component === "..")
  ) {
    throw new Error(`Pi source entry has an unsupported relative path: ${path}`);
  }
  const stable = components.join("/");
  if (Buffer.byteLength(stable) > MAX_SOURCE_PATH_BYTES) {
    throw new Error(`Pi source relative path exceeds ${MAX_SOURCE_PATH_BYTES} bytes`);
  }
  return stable;
}

function collectSourceEntries(root) {
  const entries = [];
  const directories = [root];
  let files = 0;
  while (directories.length) {
    const directory = directories.pop();
    const directoryHandle = opendirSync(directory);
    try {
      while (true) {
        const child = directoryHandle.readSync();
        if (child === null) break;
        const name = child.name;
        const path = join(directory, name);
        const metadata = lstatSync(path);
        if (metadata.isSymbolicLink()) {
          throw new Error(`Pi extension source fingerprint rejects symbolic link ${path}`);
        }
        if (entries.length >= MAX_SOURCE_ENTRIES) {
          throw new Error(`Pi extension source exceeds the ${MAX_SOURCE_ENTRIES}-entry fingerprint limit`);
        }
        const relative = sourceRelativePath(root, path);
        if (metadata.isDirectory()) {
          if (SKIPPED_SOURCE_DIRECTORIES.has(name)) continue;
          entries.push({ tag: "d", relative, path });
          directories.push(path);
        } else if (metadata.isFile()) {
          if (files >= MAX_SOURCE_FILES) {
            throw new Error(`Pi extension source exceeds the ${MAX_SOURCE_FILES}-file fingerprint limit`);
          }
          files += 1;
          entries.push({ tag: "f", relative, path });
        } else {
          throw new Error(`Pi extension source fingerprint rejects non-regular entry ${path}`);
        }
      }
    } finally {
      directoryHandle.closeSync();
    }
  }
  return entries;
}

function sortedSourceEntries(entries) {
  return entries.sort((left, right) => {
    const pathOrder = Buffer.compare(Buffer.from(left.relative), Buffer.from(right.relative));
    return pathOrder || left.tag.charCodeAt(0) - right.tag.charCodeAt(0);
  });
}

function hashSourceFile(hash, path, remaining) {
  let file;
  try {
    const noFollow = fsConstants.O_NOFOLLOW ?? 0;
    file = openSync(path, fsConstants.O_RDONLY | noFollow);
    const before = fstatSync(file);
    if (!before.isFile()) throw new Error(`Pi source entry is not a regular file: ${path}`);
    if (before.size > remaining) {
      throw new Error(`Pi extension source exceeds the ${MAX_SOURCE_BYTES}-byte fingerprint limit at ${path}`);
    }
    hash.update(unsignedBigEndian(before.size, 8));
    let total = 0;
    while (true) {
      const allowed = remaining - total;
      const buffer = Buffer.allocUnsafe(Math.min(64 * 1024, allowed + 1));
      const count = readSync(file, buffer, 0, buffer.length, null);
      if (count === 0) break;
      if (count > allowed) {
        throw new Error(`Pi extension source exceeds the ${MAX_SOURCE_BYTES}-byte fingerprint limit at ${path}`);
      }
      hash.update(buffer.subarray(0, count));
      total += count;
    }
    const after = fstatSync(file);
    if (total !== before.size || before.size !== after.size || before.mtimeMs !== after.mtimeMs) {
      throw new Error(`Pi source file changed while being fingerprinted: ${path}`);
    }
    return total;
  } finally {
    if (file !== undefined) closeSync(file);
  }
}

function fingerprintSource(source) {
  const absolute = resolve(source);
  const metadata = lstatSync(absolute);
  if (metadata.isSymbolicLink()) {
    throw new Error(`Pi extension source fingerprint rejects symbolic link ${absolute}`);
  }
  const canonical = realpathSync(absolute);
  if (canonical !== absolute) {
    throw new Error(`Pi extension source fingerprint requires a canonical path: ${absolute}`);
  }
  let rootTag;
  let entries;
  if (metadata.isFile()) {
    rootTag = "f";
    entries = [{ tag: "f", relative: ".", path: absolute }];
  } else if (metadata.isDirectory()) {
    rootTag = "d";
    entries = collectSourceEntries(absolute);
  } else {
    throw new Error(`Pi extension source fingerprint accepts only files or directories: ${absolute}`);
  }
  sortedSourceEntries(entries);

  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-source-fingerprint\0"));
  hash.update(unsignedBigEndian(SOURCE_FINGERPRINT_FORMAT, 4));
  hash.update(Buffer.from(rootTag));
  let total = 0;
  let fileCount = 0;
  for (const entry of entries) {
    hash.update(Buffer.from(entry.tag));
    const relative = Buffer.from(entry.relative);
    hash.update(unsignedBigEndian(relative.length, 8));
    hash.update(relative);
    if (entry.tag === "f") {
      total += hashSourceFile(hash, entry.path, MAX_SOURCE_BYTES - total);
      fileCount += 1;
    }
  }

  if (metadata.isDirectory()) {
    const after = sortedSourceEntries(collectSourceEntries(absolute));
    const beforeShape = entries.map((entry) => `${entry.tag}\0${entry.relative}`);
    const afterShape = after.map((entry) => `${entry.tag}\0${entry.relative}`);
    if (JSON.stringify(beforeShape) !== JSON.stringify(afterShape)) {
      throw new Error("Pi extension source tree changed while it was being fingerprinted");
    }
  }
  return {
    algorithm: "sha256",
    format_version: SOURCE_FINGERPRINT_FORMAT,
    digest: hash.digest("hex"),
    file_count: fileCount,
    byte_count: total,
  };
}

function verifySourceFingerprints() {
  if (!args.sourceFingerprints.length) return;
  for (let index = 0; index < args.extensions.length; index += 1) {
    const actual = fingerprintSource(args.extensions[index]);
    const expected = args.sourceFingerprints[index];
    const expectedDigest = typeof expected === "string" ? expected : expected.digest;
    if (actual.digest !== expectedDigest) {
      throw new Error(
        `Pi extension source changed after aggregate locking: ${args.extensions[index]} (expected ${expectedDigest}, found ${actual.digest})`,
      );
    }
    if (
      typeof expected === "object"
      && (
        actual.algorithm !== expected.algorithm
        || actual.format_version !== expected.format_version
        || actual.file_count !== expected.file_count
        || actual.byte_count !== expected.byte_count
      )
    ) {
      throw new Error(`Pi extension source fingerprint metadata changed after lock publication: ${args.extensions[index]}`);
    }
  }
}

function readRegularBufferBounded(path, maxBytes) {
  const pathMetadata = lstatSync(path);
  if (pathMetadata.isSymbolicLink() || !pathMetadata.isFile()) {
    throw new Error("is not a regular non-symlink file");
  }
  let file;
  try {
    const noFollow = fsConstants.O_NOFOLLOW ?? 0;
    file = openSync(path, fsConstants.O_RDONLY | noFollow);
    const before = fstatSync(file);
    if (!before.isFile()) throw new Error("is not a regular file");
    if (
      pathMetadata.dev !== before.dev
      || pathMetadata.ino !== before.ino
      || pathMetadata.size !== before.size
      || pathMetadata.mtimeMs !== before.mtimeMs
    ) {
      throw new Error("changed before it was read");
    }
    if (before.size > maxBytes) throw new Error(`exceeds the ${maxBytes}-byte limit`);
    const chunks = [];
    let total = 0;
    while (true) {
      const buffer = Buffer.allocUnsafe(Math.min(64 * 1024, maxBytes + 1 - total));
      const count = readSync(file, buffer, 0, buffer.length, null);
      if (count === 0) break;
      total += count;
      if (total > maxBytes) throw new Error(`exceeds the ${maxBytes}-byte limit`);
      chunks.push(buffer.subarray(0, count));
    }
    const after = fstatSync(file);
    if (
      before.dev !== after.dev ||
      before.ino !== after.ino ||
      before.size !== after.size ||
      before.mtimeMs !== after.mtimeMs ||
      total !== before.size
    ) {
      throw new Error("changed while it was being read");
    }
    return Buffer.concat(chunks, total);
  } finally {
    if (file !== undefined) closeSync(file);
  }
}

function readRegularUtf8Bounded(path, maxBytes) {
  return new TextDecoder("utf-8", { fatal: true }).decode(readRegularBufferBounded(path, maxBytes));
}

const EXPECTED_PROFILE = {
  id: PROFILE_ID,
  repository: PROFILE_REPOSITORY,
  revision: PROFILE_REVISION,
  tag: PROFILE_TAG,
  coding_agent: {
    name: SUPPORTED_PI_PACKAGE,
    version: SUPPORTED_PI_VERSION,
    npm_integrity: SUPPORTED_PI_INTEGRITY,
  },
  tui: {
    name: SUPPORTED_TUI_PACKAGE,
    version: SUPPORTED_PI_VERSION,
    npm_integrity: SUPPORTED_TUI_INTEGRITY,
  },
  node_minimum_version: MINIMUM_NODE_VERSION.join("."),
};

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function requireExactObject(value, label, required, optional = []) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new Error(`${label} contains unknown field ${JSON.stringify(key)}`);
  }
  for (const key of required) {
    if (!Object.hasOwn(value, key)) throw new Error(`${label} is missing field ${JSON.stringify(key)}`);
  }
}

function canonicalJson(value) {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) throw new Error("Pi aggregate lock numbers must be safe integers");
    return String(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  throw new Error("Pi aggregate lock contains a value that JSON cannot canonically encode");
}

function aggregateLockDigest(record) {
  const unsigned = { ...record, aggregate_digest: "" };
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-aggregate-lock-v2\0"));
  hash.update(Buffer.from(canonicalJson(unsigned)));
  return hash.digest("hex");
}

function stableSourceId(path, kind) {
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-source-id-v1\0"));
  hash.update(Buffer.from(kind));
  hash.update(Buffer.from([0]));
  hash.update(Buffer.from(path));
  return `pi-source-${hash.digest("hex")}`;
}

function validateAggregateLockShape(record) {
  requireExactObject(
    record,
    "Pi aggregate lock",
    [
      "schema_version",
      "profile",
      "bridge",
      "ygg_version",
      "name",
      "sources",
      "pi_home",
      "pi_package",
      "aggregate_digest",
    ],
  );
  if (record.schema_version !== LOCK_SCHEMA_VERSION) {
    throw new Error(`unsupported Pi aggregate lock schema ${record.schema_version}`);
  }
  if (canonicalJson(record.profile) !== canonicalJson(EXPECTED_PROFILE)) {
    throw new Error(`Pi aggregate lock does not select the exact supported ${PROFILE_ID} profile`);
  }
  requireExactObject(record.bridge, "Pi aggregate bridge", ["version", "script_digest"]);
  if (record.bridge.version !== BRIDGE_VERSION || !/^[0-9a-f]{64}$/.test(record.bridge.script_digest)) {
    throw new Error("Pi aggregate lock bridge metadata is invalid");
  }
  if (typeof record.ygg_version !== "string" || !record.ygg_version || record.ygg_version.length > 128) {
    throw new Error("Pi aggregate lock Ygg version is invalid");
  }
  if (
    typeof record.name !== "string"
    || Buffer.byteLength(record.name) > 64
    || !/^[a-z][a-z0-9-]*$/.test(record.name)
  ) {
    throw new Error("Pi aggregate lock name is invalid");
  }
  if (!Array.isArray(record.sources) || record.sources.length === 0 || record.sources.length > MAX_AGGREGATE_SOURCES) {
    throw new Error(`invalid Pi aggregate source count ${record.sources?.length}`);
  }
  if (typeof record.pi_home !== "string" || !isAbsolute(record.pi_home)) {
    throw new Error("Pi aggregate lock pi_home must be absolute");
  }
  requireExactObject(
    record.pi_package,
    "Pi aggregate package",
    ["canonical_path", "metadata_path", "metadata_digest", "name", "version"],
  );
  if (
    typeof record.pi_package.canonical_path !== "string"
    || !isAbsolute(record.pi_package.canonical_path)
    || typeof record.pi_package.metadata_path !== "string"
    || record.pi_package.metadata_path !== join(record.pi_package.canonical_path, "package.json")
    || record.pi_package.name !== SUPPORTED_PI_PACKAGE
    || record.pi_package.version !== SUPPORTED_PI_VERSION
    || !/^[0-9a-f]{64}$/.test(record.pi_package.metadata_digest)
  ) {
    throw new Error("Pi aggregate lock package metadata is invalid");
  }
  if (!/^[0-9a-f]{64}$/.test(record.aggregate_digest)) {
    throw new Error("Pi aggregate lock digest is invalid");
  }

  const paths = new Set();
  const ids = new Set();
  for (const source of record.sources) {
    requireExactObject(
      source,
      "Pi aggregate source",
      ["id", "canonical_path", "kind", "source_fingerprint", "enabled"],
      ["dependency_lock_hash"],
    );
    if (
      typeof source.canonical_path !== "string"
      || !isAbsolute(source.canonical_path)
      || Buffer.byteLength(source.canonical_path) > MAX_SOURCE_PATH_BYTES
    ) {
      throw new Error("Pi aggregate source path must be a bounded absolute path");
    }
    if (source.kind !== "file" && source.kind !== "directory") {
      throw new Error("Pi aggregate source kind is invalid");
    }
    if (source.enabled !== true) throw new Error("Pi aggregate locks contain only enabled sources");
    if (source.id !== stableSourceId(source.canonical_path, source.kind)) {
      throw new Error(`Pi aggregate source ${source.id} has an invalid stable id`);
    }
    if (paths.has(source.canonical_path) || ids.has(source.id)) {
      throw new Error("Pi aggregate lock contains a duplicate source");
    }
    paths.add(source.canonical_path);
    ids.add(source.id);
    requireExactObject(
      source.source_fingerprint,
      "Pi source fingerprint",
      ["algorithm", "format_version", "digest", "file_count", "byte_count"],
    );
    const fingerprint = source.source_fingerprint;
    if (
      fingerprint.algorithm !== "sha256"
      || fingerprint.format_version !== SOURCE_FINGERPRINT_FORMAT
      || !/^[0-9a-f]{64}$/.test(fingerprint.digest)
      || !Number.isSafeInteger(fingerprint.file_count)
      || fingerprint.file_count < 1
      || fingerprint.file_count > MAX_SOURCE_FILES
      || !Number.isSafeInteger(fingerprint.byte_count)
      || fingerprint.byte_count < 0
      || fingerprint.byte_count > MAX_SOURCE_BYTES
      || (source.kind === "file" && fingerprint.file_count !== 1)
    ) {
      throw new Error(`Pi aggregate source ${source.id} fingerprint metadata is invalid`);
    }
    if (
      source.dependency_lock_hash !== undefined
      && !/^[0-9a-f]{64}$/.test(source.dependency_lock_hash)
    ) {
      throw new Error(`Pi aggregate source ${source.id} dependency lock hash is invalid`);
    }
  }
}

function configureFromAggregateLock(path) {
  if (!isAbsolute(path)) throw new Error("--lock requires an absolute path");
  const canonicalPath = realpathSync(path);
  if (canonicalPath !== path) throw new Error("Pi aggregate lock path must be canonical");
  let record;
  try {
    record = JSON.parse(readRegularUtf8Bounded(path, MAX_PI_LOCK_BYTES));
  } catch (error) {
    throw new Error(`cannot read Pi aggregate lock ${path}: ${error instanceof Error ? error.message : String(error)}`);
  }
  validateAggregateLockShape(record);
  const actualAggregate = aggregateLockDigest(record);
  if (actualAggregate !== record.aggregate_digest) {
    throw new Error(`Pi aggregate lock digest mismatch: expected ${record.aggregate_digest}, found ${actualAggregate}`);
  }
  const expectedAggregate = process.env[AGGREGATE_DIGEST_ENV];
  if (!expectedAggregate || !/^[0-9a-f]{64}$/.test(expectedAggregate)) {
    throw new Error(`generated Pi lock mode requires ${AGGREGATE_DIGEST_ENV}`);
  }
  if (expectedAggregate !== record.aggregate_digest) {
    throw new Error(`Pi aggregate lock does not match ${AGGREGATE_DIGEST_ENV}`);
  }
  const scriptDigest = sha256(readRegularBufferBounded(fileURLToPath(import.meta.url), MAX_GENERATED_FILE_BYTES));
  if (scriptDigest !== record.bridge.script_digest) {
    throw new Error("Pi bridge script does not match the digest-bound aggregate lock");
  }
  const selectedPackage = inspectPiPackage(record.pi_package.canonical_path);
  if (!selectedPackage || selectedPackage.error) {
    throw new Error(`locked Pi package is unavailable: ${selectedPackage?.error ?? "package.json is missing"}`);
  }
  if (
    selectedPackage.root !== record.pi_package.canonical_path
    || selectedPackage.manifestPath !== record.pi_package.metadata_path
    || selectedPackage.metadataDigest !== record.pi_package.metadata_digest
  ) {
    throw new Error("selected Pi package metadata changed after lock publication");
  }
  args.extensions = record.sources.map((source) => source.canonical_path);
  args.sourceFingerprints = record.sources.map((source) => source.source_fingerprint);
  args.agentDir = record.pi_home;
  args.commandName = record.name;
  args.piPackage = record.pi_package.canonical_path;
  args.lockRecord = record;
}

function inspectPiPackage(candidate) {
  const selected = resolve(candidate);
  let root;
  try {
    const metadata = lstatSync(selected);
    if (metadata.isSymbolicLink() || !metadata.isDirectory()) {
      return { root: selected, error: "package root is not a regular non-symlink directory" };
    }
    root = realpathSync(selected);
  } catch {
    return null;
  }
  const manifestPath = join(root, "package.json");
  if (!existsSync(manifestPath)) return null;
  let manifest;
  let manifestBytes;
  try {
    manifestBytes = readRegularBufferBounded(manifestPath, MAX_PI_PACKAGE_MANIFEST_BYTES);
    manifest = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(manifestBytes));
  } catch (error) {
    return { root, error: `cannot read package.json: ${error instanceof Error ? error.message : String(error)}` };
  }
  if (manifest?.name !== SUPPORTED_PI_PACKAGE) {
    return { root, error: `package name is ${JSON.stringify(manifest?.name)}, expected ${SUPPORTED_PI_PACKAGE}` };
  }
  if (manifest.version !== SUPPORTED_PI_VERSION) {
    return { root, error: `Pi version ${JSON.stringify(manifest.version)} is unsupported; expected exactly ${SUPPORTED_PI_VERSION}` };
  }
  const entrypoint = join(root, "dist/index.js");
  try {
    const metadata = lstatSync(entrypoint);
    if (metadata.isSymbolicLink() || !metadata.isFile()) {
      return { root, error: "dist/index.js is not a regular non-symlink file" };
    }
    if (realpathSync(entrypoint) !== entrypoint) {
      return { root, error: "dist/index.js escapes the canonical package root" };
    }
  } catch (error) {
    return {
      root,
      error: `cannot validate dist/index.js: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
  return {
    root,
    version: manifest.version,
    manifestPath,
    metadataDigest: sha256(manifestBytes),
  };
}

function findPiPackageRoot(extensionPaths, selectedPackage) {
  if (selectedPackage) {
    const inspected = inspectPiPackage(selectedPackage);
    if (!inspected || inspected.error) {
      throw new Error(
        `--pi-package does not select a compatible Pi runtime: ${inspected?.error ?? "package.json is missing"}`,
      );
    }
    return inspected;
  }
  for (const [name, value] of [
    ["YGG_PI_PACKAGE", process.env.YGG_PI_PACKAGE],
    ["PI_CODING_AGENT_PACKAGE", process.env.PI_CODING_AGENT_PACKAGE],
  ]) {
    if (!value) continue;
    const inspected = inspectPiPackage(value);
    if (!inspected || inspected.error) {
      throw new Error(`${name} does not select a compatible Pi runtime: ${inspected?.error ?? "package.json is missing"}`);
    }
    return inspected;
  }

  const candidates = [];
  for (const pathEntry of (process.env.PATH ?? "").split(delimiter)) {
    if (!pathEntry) continue;
    const executable = join(pathEntry, "pi");
    if (!existsSync(executable)) continue;
    try {
      let current = dirname(realpathSync(executable));
      for (let depth = 0; depth < 8; depth += 1) {
        candidates.push(current);
        current = dirname(current);
      }
    } catch {
      // Continue with extension-local and conventional locations.
    }
  }

  for (const extension of extensionPaths) {
    let current = dirname(resolve(extension));
    for (let depth = 0; depth < 8; depth += 1) {
      candidates.push(join(current, "node_modules", "@earendil-works", "pi-coding-agent"));
      current = dirname(current);
    }
  }

  candidates.push(
    join(homedir(), ".local/lib/node_modules/@earendil-works/pi-coding-agent"),
    join(homedir(), ".npm-global/lib/node_modules/@earendil-works/pi-coding-agent"),
    "/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent",
    "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent",
  );

  const seen = new Set();
  const incompatible = [];
  for (const candidate of candidates) {
    const root = resolve(candidate);
    if (seen.has(root)) continue;
    seen.add(root);
    const inspected = inspectPiPackage(root);
    if (!inspected) continue;
    if (!inspected.error) return inspected;
    incompatible.push(inspected.error);
  }
  const suffix = incompatible.length ? ` Incompatible candidates: ${[...new Set(incompatible)].join("; ")}` : "";
  throw new Error(
    `could not locate ${SUPPORTED_PI_PACKAGE}@${SUPPORTED_PI_VERSION}; set YGG_PI_PACKAGE to its package root.${suffix}`,
  );
}

function commandDefinitionToYgg(command) {
  const name = command.invocationName ?? command.name;
  return {
    name,
    description: command.description || `Run Pi command /${name}`,
    usage: `/${name}`,
  };
}

function stableCatalogId(kind, value) {
  const digest = createHash("sha256").update(`${kind}\0${value}`).digest("hex").slice(0, 24);
  return `pi_${kind}_${digest}`;
}

function registerProviderLocal(nameOrProvider, config, extensionPath) {
  const native = typeof nameOrProvider === "object" && nameOrProvider !== null;
  const name = String(native ? nameOrProvider.id : nameOrProvider);
  const providerConfig = native ? nameOrProvider : config;
  bridge.providers.set(name, { config: providerConfig, extensionPath, native });
  bridge.models = [...bridge.providers.entries()].flatMap(([provider, registration]) =>
    (registration.config?.models ?? []).map((model) => ({ ...model, provider })),
  );
  if (scopes.getStore()?.invocation) {
    recordEffect({
      type: "update_provider_catalog",
      update: { action: "register", provider: name, config: serializableProviderConfig(providerConfig) },
    });
  }
  scheduleCatalogRefresh();
}

function unregisterProviderLocal(name) {
  const provider = String(name);
  bridge.providers.delete(provider);
  bridge.models = bridge.models.filter((model) => model.provider !== provider);
  if (scopes.getStore()?.invocation) {
    recordEffect({ type: "update_provider_catalog", update: { action: "unregister", provider } });
  }
  scheduleCatalogRefresh();
}

function serializableProviderConfig(config) {
  if (!config || typeof config !== "object") return {};
  const result = {};
  for (const [key, value] of Object.entries(config)) {
    if (typeof value === "function") continue;
    if (key === "oauth" && value && typeof value === "object") {
      result.oauth = {
        name: String(value.name ?? "OAuth"),
        isSubscription: value.isSubscription === true,
        handle: stableCatalogId("oauth", String(value.name ?? "oauth")),
      };
      continue;
    }
    result[key] = value;
  }
  if (typeof config.streamSimple === "function") {
    result.custom_stream_handle = stableCatalogId("stream", String(config.name ?? "provider"));
  }
  return result;
}

function buildRuntimeCatalog(revision = bridge.catalogRevision) {
  const commands = bridge.runner.getRegisteredCommands().map(commandDefinitionToYgg);
  const flags = [];
  const shortcuts = [];
  const events = new Set();
  const toolRenderers = [];
  const messageRenderers = [];
  const entryRenderers = [];
  const markdownTransformers = [];
  for (const extension of bridge.loadedExtensions) {
    for (const flag of extension.flags?.values?.() ?? []) {
      flags.push({
        name: flag.name,
        description: flag.description ?? `Pi flag --${flag.name}`,
        kind: flag.type,
        ...(flag.default === undefined ? {} : { default: flag.default }),
      });
    }
    for (const shortcut of extension.shortcuts?.values?.() ?? []) {
      shortcuts.push({
        id: stableCatalogId("shortcut", `${extension.path}:${shortcut.shortcut}`),
        key: String(shortcut.shortcut),
        description: shortcut.description ?? `Pi shortcut ${shortcut.shortcut}`,
      });
    }
    for (const name of extension.handlers?.keys?.() ?? []) {
      if (PI_EVENT_NAMES.includes(name)) events.add(name);
    }
    for (const tool of extension.tools?.values?.() ?? []) {
      if (tool.definition.renderCall || tool.definition.renderResult) {
        toolRenderers.push(tool.definition.name);
      }
    }
    for (const customType of extension.messageRenderers?.keys?.() ?? []) {
      messageRenderers.push(stableCatalogId("message", `${extension.path}:${customType}`));
    }
    for (const customType of extension.entryRenderers?.keys?.() ?? []) {
      entryRenderers.push(stableCatalogId("entry", `${extension.path}:${customType}`));
    }
    if (extension.markdownTransformer) {
      markdownTransformers.push(stableCatalogId("markdown", extension.path));
    }
  }
  return {
    revision,
    tools: bridge.runner.getAllRegisteredTools().map((tool) => toolDefinitionToYgg(tool.definition)),
    commands,
    flags,
    shortcuts,
    events: [...events],
    tool_renderers: [...new Set(toolRenderers)],
    message_renderers: messageRenderers,
    entry_renderers: entryRenderers,
    markdown_transformers: markdownTransformers,
    providers: [...bridge.providers.keys()].map((id) => ({ id })),
    roles: [],
  };
}

function toolDefinitionToYgg(definition) {
  const parameters = definition.parameters ?? { type: "object", properties: {} };
  if (!parameters || typeof parameters !== "object" || Array.isArray(parameters)) {
    throw new Error(`Pi tool ${definition.name} has a non-object parameter schema`);
  }
  return {
    name: definition.name,
    description: definition.description || definition.label || definition.name,
    parameters,
  };
}

function setCurrentTools(tools, revision) {
  bridge.tools = tools;
  bridge.catalogRevision = revision;
  bridge.toolNames = tools.map((tool) => tool.definition.name);
  if (bridge.activeToolNames.length === 0) bridge.activeToolNames = bridge.toolNames.slice();
  else bridge.activeToolNames = bridge.activeToolNames.filter((name) => bridge.toolNames.includes(name));
  bridge.toolInfos = tools.map((tool) => ({
    name: tool.definition.name,
    description: tool.definition.description,
    parameters: tool.definition.parameters,
    promptGuidelines: tool.definition.promptGuidelines,
    sourceInfo: tool.sourceInfo,
  }));
  bridge.toolSnapshots.set(revision, tools.slice());
  while (bridge.toolSnapshots.size > 8) {
    bridge.toolSnapshots.delete(bridge.toolSnapshots.keys().next().value);
  }
}

function scheduleCatalogRefresh() {
  if (!bridge) return;
  if (!bridge.initialized || !bridge.processFence) {
    bridge.catalogRefreshRequested = true;
    return;
  }
  bridge.catalogRefreshChain = bridge.catalogRefreshChain
    .then(() => refreshPublishedCatalog())
    .catch((error) => diagnostic(`Pi catalog publication failed: ${error instanceof Error ? error.message : String(error)}`));
}

async function refreshPublishedCatalog() {
  if (!bridge.processFence) {
    bridge.catalogRefreshRequested = true;
    return;
  }
  const next = buildRuntimeCatalog(bridge.catalogRevision + 1);
  const currentComparable = { ...(bridge.currentCatalog ?? {}), revision: 0 };
  const nextComparable = { ...next, revision: 0 };
  if (JSON.stringify(currentComparable) === JSON.stringify(nextComparable)) return;
  const response = await requestHost("catalog/replace", {
    process: bridge.processFence,
    expected_revision: bridge.catalogRevision,
    catalog: next,
  });
  const accepted = response.catalog;
  if (!accepted || accepted.revision !== next.revision) {
    throw new Error("Ygg returned an invalid API 0.3 catalog acknowledgement");
  }
  const acceptedTools = new Set((accepted.tools ?? []).map((tool) => tool.name));
  setCurrentTools(
    bridge.runner.getAllRegisteredTools().filter((tool) => acceptedTools.has(tool.definition.name)),
    accepted.revision,
  );
  bridge.currentCatalog = accepted;
  bridge.commands = bridge.runner.getRegisteredCommands().filter((command) =>
    (accepted.commands ?? []).some((definition) => definition.name === (command.invocationName ?? command.name)),
  );
}

function textFromContent(content) {
  return (content ?? [])
    .filter((part) => part?.type === "text")
    .map((part) => String(part.text ?? ""))
    .join("\n");
}

async function lowerContent(content) {
  const lowered = [];
  for (const part of content ?? []) {
    if (!part || typeof part !== "object") continue;
    if (part.type === "text") {
      lowered.push({ type: "text", text: String(part.text ?? "") });
      continue;
    }
    if (part.type === "image" && typeof part.data === "string") {
      if (!bridge.features.has("artifacts")) {
        lowered.push({
          type: "text",
          text: part.alt
            ? `[Pi extension image: ${String(part.alt)}]`
            : "[Pi extension returned an image, but Ygg artifact support was not negotiated]",
        });
        continue;
      }
      const bytes = Buffer.from(part.data, "base64");
      const mimeType = String(part.mimeType ?? "image/png");
      const sha256 = createHash("sha256").update(bytes).digest("hex");
      const artifact = await requestHost("artifact/publish", {
        parent_request_id: parentRequestId(),
        mime_type: mimeType,
        size: bytes.length,
        sha256,
        data: { encoding: "base64", data: bytes.toString("base64") },
      });
      lowered.push({
        type: "image",
        artifact_id: artifact.artifact_id,
        mime_type: mimeType,
        ...(part.alt ? { alt: String(part.alt) } : {}),
      });
      continue;
    }
    diagnostic(`Pi compatibility dropped unsupported content part ${part.type}`);
  }
  if (!lowered.some((part) => part.type === "text")) {
    lowered.unshift({ type: "text", text: "[Pi extension returned no text content]" });
  }
  return lowered;
}

function contextContribution(label, content, placement = "prompt_suffix") {
  if (!content) return null;
  return { label, content: String(content), placement };
}

function messageContent(message) {
  if (!message || typeof message !== "object") return "";
  if (typeof message.content === "string") return message.content;
  return textFromContent(message.content);
}

async function collectBeforePromptContext(prompt) {
  const result = await bridge.runner.emitBeforeAgentStart(
    prompt,
    undefined,
    "",
    { cwd: bridge.cwd },
  );
  const context = [];
  if (result?.systemPrompt) {
    const contribution = contextContribution(
      "pi-before-agent-start",
      result.systemPrompt,
      "system_suffix",
    );
    if (contribution) context.push(contribution);
  }
  if (result?.messages) {
    for (const message of result.messages) {
      const contribution = contextContribution(
        "pi-before-agent-start",
        messageContent(message),
        "prompt_suffix",
      );
      if (contribution) context.push(contribution);
    }
  }
  return context;
}

async function loadBridge(params) {
  validateNodeRuntime();
  if (args.lock) configureFromAggregateLock(args.lock);
  verifySourceFingerprints();
  bridge = {
    cwd: resolve(params.workspace ?? args.cwd ?? process.cwd()),
    agentDir: resolve(args.agentDir ?? process.env.YGG_PI_AGENT_DIR ?? join(homedir(), ".pi/agent")),
    extensionPaths: args.extensions.map((path) => resolve(path)),
    commandName: args.commandName,
    hostState: params.host && typeof params.host === "object" ? { ...params.host } : {},
    agentActive: false,
    pendingMessages: 0,
    toolNames: [],
    activeToolNames: [],
    toolInfos: [],
    toolSnapshots: new Map(),
    catalogRefreshChain: Promise.resolve(),
    catalogRefreshRequested: false,
    catalogRevision: 0,
    currentCatalog: null,
    processFence: null,
    features: new Set(REQUIRED_FEATURES),
    hostServices: params.protocol?.host_services ?? [],
    loadedExtensions: [],
    providers: new Map(),
    models: [],
    model: undefined,
    scopedModels: [],
    editorText: "",
    editorComponent: undefined,
    autocompleteProviders: [],
    toolsExpanded: false,
    remoteComponents: new Map(),
    remoteComponentRevisions: new Map(),
    remoteComponentGeneration: 1,
    themes: new Map([["compat", { name: "compat", path: undefined, theme: makeCompatibilityTheme() }]]),
    currentTheme: makeCompatibilityTheme(),
    nextSyntheticEntryId: 1,
    sessionSnapshot: {
      entries: Array.isArray(params.host?.session_entries) ? params.host.session_entries.slice() : [],
      leafId: params.host?.session_leaf_id ?? null,
      labels: new Map(),
      tree: Array.isArray(params.host?.session_tree) ? params.host.session_tree.slice() : [],
      header: params.host?.session_header ?? null,
    },
    terminal: {
      pendingCompletedTurn: null,
      pendingAgentMessages: null,
      sessionShutdown: false,
    },
    pendingPiToolCalls: new Map(),
    startedPiToolCalls: new Map(),
    hostToolCallIds: new Map(),
    initialized: false,
  };

  const offeredOptionalFeatures = new Set(params.protocol?.optional_features ?? []);
  for (const feature of OPTIONAL_FEATURES) {
    if (offeredOptionalFeatures.has(feature)) bridge.features.add(feature);
  }

  const piRuntime = findPiPackageRoot(bridge.extensionPaths, args.piPackage);
  bridge.piRuntimeVersion = piRuntime.version;
  const pi = await import(pathToFileURL(join(piRuntime.root, "dist/index.js")).href);
  if (typeof pi.discoverAndLoadExtensions !== "function") {
    throw new Error("installed Pi runtime does not expose discoverAndLoadExtensions");
  }
  const eventBus = pi.createEventBus();
  const loaded = await pi.discoverAndLoadExtensions(
    bridge.extensionPaths,
    bridge.cwd,
    bridge.agentDir,
    eventBus,
  );
  verifySourceFingerprints();
  for (const error of loaded.errors ?? []) {
    diagnostic(`Pi extension load failed at ${error.path}: ${error.error}`);
  }
  if (!loaded.extensions?.length) {
    throw new Error("Pi loader found no extension modules");
  }
  bridge.pi = pi;
  bridge.loaded = loaded;
  bridge.loadedExtensions = loaded.extensions;

  bridge.runner = new pi.ExtensionRunner(
    loaded.extensions,
    loaded.runtime,
    bridge.cwd,
    makeSessionManager(),
    makeModelRegistry(),
  );
  bridge.runner.onError((error) => {
    const location = error?.extensionPath ? ` in ${error.extensionPath}` : "";
    const event = error?.event ? ` during ${error.event}` : "";
    boundedDiagnostic(`Pi extension handler failed${location}${event}: ${error?.error ?? error}`);
  });
  bridge.runner.bindCore(makeExtensionActions(), makeExtensionContextActions(), {
    registerProvider: (name, config) => registerProviderLocal(name, config),
    registerNativeProvider: (provider) => registerProviderLocal(provider),
    unregisterProvider: (name) => unregisterProviderLocal(name),
  });
  bridge.runner.bindCommandContext({
    waitForIdle: async () => {
      await callHostService("control", "wait-idle", {});
    },
    async newSession(options = {}) {
      const result = await callHostService("session", "new", {
        parent_session: options.parentSession ?? null,
      });
      if (!result?.cancelled && options.withSession) {
        await options.withSession(bridge.runner.createCommandContext());
      }
      return { cancelled: result?.cancelled === true };
    },
    async fork(entryId, options = {}) {
      const result = await callHostService("session", "fork", {
        entry_id: String(entryId),
        position: options.position ?? "at",
      });
      if (!result?.cancelled && options.withSession) {
        await options.withSession(bridge.runner.createCommandContext());
      }
      return { cancelled: result?.cancelled === true };
    },
    async navigateTree(targetId, options = {}) {
      const result = await callHostService("session", "navigate", {
        target_id: String(targetId),
        summarize: options.summarize === true,
        custom_instructions: options.customInstructions ?? null,
        replace_instructions: options.replaceInstructions === true,
        label: options.label ?? null,
      });
      return { cancelled: result?.cancelled === true };
    },
    async switchSession(sessionPath, options = {}) {
      const result = await callHostService("session", "switch", { session_path: String(sessionPath) });
      if (!result?.cancelled && options.withSession) {
        await options.withSession(bridge.runner.createCommandContext());
      }
      return { cancelled: result?.cancelled === true };
    },
    reload: async () => {
      await callHostService("control", "reload", {});
    },
  });
  bridge.runner.setUIContext(makeUi(), "rpc");

  const initialTools = bridge.runner.getAllRegisteredTools();
  setCurrentTools(initialTools, 0);
  bridge.commands = bridge.runner.getRegisteredCommands();
  bridge.currentCatalog = buildRuntimeCatalog(0);
}

async function handleInitialize(message) {
  const params = message.params ?? {};
  if (params.api_version !== API_VERSION || params.protocol?.version !== API_VERSION) {
    throw new Error(`Pi compatibility bridge requires extension API ${API_VERSION}`);
  }
  const required = params.protocol?.required_features ?? [];
  if (
    required.length !== REQUIRED_FEATURES.length
    || REQUIRED_FEATURES.some((feature) => !required.includes(feature))
  ) {
    throw new Error("Ygg did not offer the exact mandatory API 0.3 feature set");
  }
  await loadBridge(params);
  bridge.initialized = true;
  if (bridge.catalogRefreshRequested && bridge.processFence) {
    bridge.catalogRefreshRequested = false;
    setImmediate(() => scheduleCatalogRefresh());
  }
  diagnostic(`Pi compatibility profile ${bridge.piRuntimeVersion} initialized`);
  return {
    api_version: API_VERSION,
    tools: [],
    commands: [],
    protocol: {
      version: API_VERSION,
      features: [...bridge.features],
      limits: { max_concurrent_requests: 4 },
      host_services: bridge.hostServices,
      catalog: bridge.currentCatalog,
    },
  };
}

async function runScoped(id, operation, invocation = null) {
  const controller = new AbortController();
  const entry = { controller };
  inflight.set(keyOf(id), entry);
  if (invocation?.process) {
    bridge.processFence = invocation.process;
    if (bridge.catalogRefreshRequested) {
      bridge.catalogRefreshRequested = false;
      setImmediate(() => scheduleCatalogRefresh());
    }
  }
  try {
    return await scopes.run(
      {
        parentRequestId: typeof id === "number" ? id : Number(id),
        controller,
        signal: controller.signal,
        progressSequence: 0,
        invocation,
        effects: [],
      },
      operation,
    );
  } catch (error) {
    if (controller.signal.aborted && !(error instanceof CancellationError)) {
      throw new CancellationError();
    }
    throw error;
  } finally {
    inflight.delete(keyOf(id));
  }
}

function enqueueByName(map, name, value) {
  const queue = map.get(name) ?? [];
  queue.push(value);
  if (queue.length > 64) queue.shift();
  map.set(name, queue);
}

function dequeueByName(map, name) {
  const queue = map.get(name);
  const value = queue?.shift();
  if (queue?.length === 0) map.delete(name);
  return value;
}

async function callPiTool(message) {
  const name = message.params?.name;
  const revision = message.params?.catalog_revision ?? bridge.catalogRevision;
  const tools = bridge.toolSnapshots.get(revision);
  if (!tools) throw new Error(`unknown or retired Pi tool catalog revision ${revision}`);
  const registered = tools.find((tool) => tool.definition.name === name);
  if (!registered) throw new Error(`unknown bridged Pi tool ${name}`);
  const definition = registered.definition;
  const started = dequeueByName(bridge.startedPiToolCalls, name);
  let input = started?.input ?? message.params?.arguments ?? {};
  if (started?.input === undefined && definition.prepareArguments) {
    input = definition.prepareArguments(input);
  }
  const toolCallId = started?.id ?? `pi-ygg-${String(message.id)}`;
  const callEvent = { type: "tool_call", toolCallId, toolName: name, input };
  let result;
  try {
    result = await definition.execute(
      toolCallId,
      callEvent.input,
      currentScope().signal,
      async (update) => {
        const text = textFromContent(update?.content) || update?.message || "Pi tool update";
        await progress(text);
        await bridge.runner.emit({
          type: "tool_execution_update",
          toolCallId,
          toolName: name,
          args: callEvent.input,
          partialResult: update,
        });
      },
      bridge.runner.createContext(),
    );
  } catch (error) {
    throw error;
  }

  const toolResultEvent = {
    type: "tool_result",
    toolCallId,
    toolName: name,
    input: callEvent.input,
    content: result?.content ?? [],
    details: result?.details,
    isError: result?.isError === true,
  };
  const transformed = (await bridge.runner.emitToolResult(toolResultEvent)) ?? toolResultEvent;
  const finalContent = await lowerContent(transformed.content ?? result?.content);
  let metadata = transformed.details;
  if (transformed.usage !== undefined) {
    metadata = { details: transformed.details ?? null, usage: transformed.usage };
  }
  return finishObjectResult({
    content: finalContent,
    is_error: transformed.isError === true,
    ...(metadata === undefined ? {} : { metadata }),
  });
}

async function executePiCommand(message) {
  const command = message.params?.name;
  const argumentsList = message.params?.arguments ?? [];
  const registered = bridge.commands.find(
    (candidate) => (candidate.invocationName ?? candidate.name) === command,
  );
  if (!registered) throw new Error(`unknown bridged Pi command ${command}`);
  await registered.handler(argumentsList.join(" "), bridge.runner.createCommandContext());
  return finishObjectResult({
    text: `Pi command ${command} completed.`,
    notifications: [],
    context: [],
  });
}

async function runHook(message) {
  const hook = message.params?.hook;
  const payload = message.params?.payload ?? {};
  if (hook === "before_tool_call") {
    const toolCallId = `pi-ygg-hook-${String(message.id)}`;
    const registered = bridge.tools.find((tool) => tool.definition.name === payload.name);
    let input = payload.arguments ?? {};
    if (registered?.definition.prepareArguments) {
      input = registered.definition.prepareArguments(input);
    }
    const event = {
      type: "tool_call",
      toolCallId,
      toolName: payload.name,
      input,
    };
    const inputBeforeHandlers = JSON.stringify(event.input);
    const result = await bridge.runner.emitToolCall(event);
    if (result?.terminate) {
      return unsupported("tool_call.terminate");
    }
    if (
      !registered
      && JSON.stringify(event.input) !== inputBeforeHandlers
    ) {
      return unsupported("tool_call input mutation for Ygg-native tools");
    }
    if (!result?.block && bridge.toolNames.includes(payload.name)) {
      enqueueByName(bridge.pendingPiToolCalls, payload.name, { id: toolCallId, input });
    }
    return finishObjectResult({
      disposition: result?.block
        ? { action: "deny", reason: result.reason ?? "Blocked by Pi extension" }
        : { action: "continue" },
      context: [],
      notifications: [],
    });
  }
  if (hook === "after_tool_call") {
    if (!bridge.toolNames.includes(payload.name)) {
      const transformed = await bridge.runner.emitToolResult({
        type: "tool_result",
        toolCallId: `pi-ygg-hook-${String(message.id)}`,
        toolName: payload.name,
        input: payload.arguments ?? {},
        content: [{ type: "text", text: String(payload.output ?? "") }],
        isError: payload.is_error === true,
      });
      if (transformed !== undefined) {
        return unsupported("tool_result mutation for Ygg-native tools");
      }
    }
    return finishObjectResult({ disposition: { action: "continue" }, context: [], notifications: [] });
  }
  if (hook === "before_prompt") {
    return finishObjectResult({
      disposition: { action: "continue" },
      context: await collectBeforePromptContext(String(payload.prompt ?? "")),
      notifications: [],
    });
  }
  if (hook === "after_response") {
    const messages = [
      { role: "assistant", content: [{ type: "text", text: String(payload.response ?? "") }] },
    ];
    if (bridge.terminal.pendingCompletedTurn) {
      bridge.terminal.pendingCompletedTurn = null;
      await finishTurn(messages);
    } else {
      bridge.terminal.pendingAgentMessages = messages;
    }
    return finishObjectResult({ disposition: { action: "continue" }, context: [], notifications: [] });
  }
  throw new Error(`unsupported Ygg hook ${hook}`);
}

async function collectContext(message) {
  const context = await collectBeforePromptContext(String(message.params?.prompt ?? ""));
  const messages = await bridge.runner.emitContext([]);
  for (const item of messages) {
    const contribution = contextContribution("pi-context", messageContent(item));
    if (contribution) context.push(contribution);
  }
  if (currentScope().effects.length) {
    throw new Error("Pi context collection produced mutation effects outside an ordered event");
  }
  return context;
}

async function resolveOrderedPayload(payload, invocation) {
  const reference = payload?.document;
  if (!reference) return payload ?? {};
  if (payload.encoding !== "json") throw new Error("unsupported ordered-event document encoding");
  const chunks = [];
  let offset = 0;
  let index = 0;
  while (true) {
    const response = await requestHost("document/read", {
      operation_token: invocation.operation,
      document_id: reference.document_id,
      offset,
    });
    const chunk = response?.chunk;
    if (!chunk || chunk.index !== index || chunk.offset !== offset) {
      throw new Error("Ygg returned an out-of-order document chunk");
    }
    const bytes = Buffer.from(chunk.data, "base64");
    if (bytes.length !== chunk.decoded_bytes) throw new Error("document chunk length mismatch");
    chunks.push(bytes);
    offset += bytes.length;
    index += 1;
    if (response.eof) break;
  }
  const bytes = Buffer.concat(chunks);
  if (bytes.length !== reference.byte_length) throw new Error("document byte length mismatch");
  if (createHash("sha256").update(bytes).digest("hex") !== reference.sha256) {
    throw new Error("document SHA-256 mismatch");
  }
  return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
}

function topLevelPiEvent(type, payload) {
  const event = { type };
  const names = {
    session_id: "sessionId",
    run_id: "runId",
    turn_id: "turnId",
    tool_call_id: "toolCallId",
    tool_name: "toolName",
    duration_ms: "durationMs",
    turn_index: "turnIndex",
    is_error: "isError",
    custom_instructions: "customInstructions",
    replace_instructions: "replaceInstructions",
  };
  for (const [key, value] of Object.entries(payload ?? {})) {
    event[names[key] ?? key] = value;
  }
  return event;
}

async function emitOrderedPiEvent(type, payload) {
  const event = topLevelPiEvent(type, payload);
  switch (type) {
    case "project_trust": {
      if (typeof bridge.pi.emitProjectTrustEvent !== "function") {
        throw new Error("pinned Pi runtime does not expose emitProjectTrustEvent");
      }
      const emitted = await bridge.pi.emitProjectTrustEvent(
        bridge.loaded,
        event,
        bridge.runner.createContext(),
      );
      for (const error of emitted.errors ?? []) bridge.runner.emitError(error);
      return emitted.result;
    }
    case "resources_discover":
      return bridge.runner.emitResourcesDiscover(
        event.cwd ?? bridge.cwd,
        event.reason ?? "reload",
      );
    case "context":
      return { messages: await bridge.runner.emitContext(event.messages ?? []) };
    case "before_provider_request":
      return await bridge.runner.emitBeforeProviderRequest(event.payload);
    case "before_provider_headers":
      return { headers: await bridge.runner.emitBeforeProviderHeaders(event.headers ?? {}) };
    case "before_agent_start":
      return await bridge.runner.emitBeforeAgentStart(
        event.prompt ?? "",
        event.images,
        event.systemPrompt ?? "",
        event.systemPromptOptions ?? { cwd: bridge.cwd },
      );
    case "message_end": {
      const message = await bridge.runner.emitMessageEnd(event);
      return message === undefined ? undefined : { message };
    }
    case "tool_call": {
      const result = await bridge.runner.emitToolCall(event);
      return { ...(result ?? {}), input: event.input };
    }
    case "tool_result":
      return await bridge.runner.emitToolResult(event);
    case "user_bash":
      return await bridge.runner.emitUserBash(event);
    case "input":
      return await bridge.runner.emitInput(
        event.text ?? "",
        event.images,
        event.source ?? "extension",
        event.streamingBehavior,
      );
    default:
      return await bridge.runner.emit(event);
  }
}

async function handleOrderedEvent(message) {
  const dispatch = message.params ?? {};
  if (!Number.isSafeInteger(dispatch.sequence) || dispatch.sequence <= 0) {
    throw new Error("ordered event requires a positive sequence");
  }
  const invocation = dispatch.invocation;
  if (!invocation?.operation || invocation.operation.kind !== "event") {
    throw new Error("ordered event requires a host event invocation");
  }
  if (dispatch.event === "session_start") bridge.terminal.sessionShutdown = false;
  if (dispatch.event === "session_shutdown") bridge.terminal.sessionShutdown = true;
  if (dispatch.event === "turn_start" || dispatch.event === "agent_start") bridge.agentActive = true;
  if (dispatch.event === "agent_settled" || dispatch.event === "session_shutdown") bridge.agentActive = false;
  const payload = await resolveOrderedPayload(dispatch.payload, invocation);
  const result = await emitOrderedPiEvent(dispatch.event, payload);
  return {
    sequence: dispatch.sequence,
    ...(result === undefined ? {} : { result }),
    effects: effectJournal(),
  };
}

async function handleOrderedEventBatch(message) {
  const events = message.params?.events;
  if (!Array.isArray(events) || events.length === 0 || events.length > 64) {
    throw new Error("ordered event batch must contain 1 to 64 events");
  }
  const outer = currentScope();
  for (const dispatch of events) {
    const result = await scopes.run(
      { ...outer, invocation: dispatch.invocation, effects: [] },
      async () => emitOrderedPiEvent(
        dispatch.event,
        await resolveOrderedPayload(dispatch.payload, dispatch.invocation),
      ),
    );
    void result;
  }
  return {};
}

async function handleProviderCallback(message) {
  const params = message.params ?? {};
  const registration = bridge.providers.get(String(params.provider));
  if (!registration) throw new Error(`Unknown Pi provider: ${params.provider}`);
  const config = registration.config ?? {};
  switch (params.action) {
    case "custom_stream": {
      if (typeof config.streamSimple !== "function") {
        throw new Error(`Pi provider ${params.provider} has no custom stream handler`);
      }
      const options = {
        ...(params.options ?? {}),
        signal: currentScope().signal,
        onPayload: (payload) => bridge.runner.emitBeforeProviderRequest(payload),
        onResponse: async (response) => {
          await bridge.runner.emit({
            type: "after_provider_response",
            status: response.status,
            headers: Object.fromEntries(response.headers?.entries?.() ?? []),
          });
        },
      };
      const stream = config.streamSimple(params.model, params.context, options);
      const events = [];
      for await (const event of stream) {
        if (events.length >= 100_000) throw new Error("custom provider stream exceeds 100000 events");
        events.push(event);
      }
      return { events };
    }
    case "refresh_models": {
      if (typeof config.refreshModels !== "function") return { models: config.models ?? [] };
      const models = await config.refreshModels({
        signal: currentScope().signal,
        publish: async (entry) => {
          await callHostService("providers", "refresh-models", {
            provider: params.provider,
            publish: entry,
          });
        },
      });
      return { models };
    }
    case "oauth_login": {
      if (!config.oauth?.login) throw new Error(`Pi provider ${params.provider} has no OAuth login`);
      const credentials = await config.oauth.login({
        onAuth: ({ url, instructions }) => callHostService("providers", "oauth", {
          action: "authorize",
          provider: params.provider,
          url,
          instructions: instructions ?? null,
        }),
        onPrompt: async ({ message, placeholder }) => {
          const result = await callHostService("providers", "oauth", {
            action: "prompt",
            provider: params.provider,
            message,
            placeholder: placeholder ?? null,
          });
          return result.value;
        },
        onProgress: (message) => progress(String(message)),
      });
      return { credentials };
    }
    case "oauth_refresh": {
      if (!config.oauth?.refreshToken) throw new Error(`Pi provider ${params.provider} has no OAuth refresh`);
      return { credentials: await config.oauth.refreshToken(params.credentials, currentScope().signal) };
    }
    case "oauth_api_key": {
      if (!config.oauth?.getApiKey) throw new Error(`Pi provider ${params.provider} has no OAuth key projection`);
      return { api_key: config.oauth.getApiKey(params.credentials) };
    }
    default:
      throw new Error(`Unknown Pi provider callback action: ${params.action}`);
  }
}

async function finishTurn(messages) {
  bridge.agentActive = false;
  await bridge.runner.emit({
    type: "turn_end",
    turnIndex: 0,
    message: messages[0] ?? { role: "assistant", content: [{ type: "text", text: "" }] },
    toolResults: [],
  });
  await bridge.runner.emit({ type: "agent_end", messages });
  await bridge.runner.emit({ type: "agent_settled" });
  bridge.terminal.pendingAgentMessages = null;
  bridge.pendingPiToolCalls.clear();
  bridge.startedPiToolCalls.clear();
  bridge.hostToolCallIds.clear();
}

async function emitSessionShutdown() {
  if (bridge.terminal.sessionShutdown) return;
  bridge.terminal.sessionShutdown = true;
  await bridge.runner.emit({ type: "session_shutdown", reason: "quit" });
}

async function handleLifecycle(method, params) {
  if (method === "session/started") {
    bridge.terminal.sessionShutdown = false;
    await bridge.runner.emit({ type: "session_start", reason: "startup" });
  } else if (method === "session/settled") {
    await emitSessionShutdown();
  } else if (method === "turn/started") {
    bridge.agentActive = true;
    bridge.terminal.pendingCompletedTurn = null;
    bridge.terminal.pendingAgentMessages = null;
    await bridge.runner.emit({ type: "turn_start", turnIndex: 0, timestamp: Date.now() });
    await bridge.runner.emit({ type: "agent_start" });
  } else if (method === "turn/settled") {
    if (params.outcome === "completed" && !bridge.terminal.pendingAgentMessages) {
      bridge.terminal.pendingCompletedTurn = params;
      return;
    }
    const messages = bridge.terminal.pendingAgentMessages ?? [];
    await finishTurn(messages);
  } else if (method === "tool/started") {
    const hostId = params.tool_call_id ?? "ygg-tool";
    let piId = hostId;
    if (bridge.toolNames.includes(params.tool_name)) {
      const pending = dequeueByName(bridge.pendingPiToolCalls, params.tool_name);
      piId = pending?.id ?? hostId;
      enqueueByName(
        bridge.startedPiToolCalls,
        params.tool_name,
        pending ?? { id: piId, input: undefined },
      );
      bridge.hostToolCallIds.set(hostId, piId);
    }
    await bridge.runner.emit({
      type: "tool_execution_start",
      toolCallId: piId,
      toolName: params.tool_name,
      args: {},
    });
  } else if (method === "tool/settled") {
    const hostId = params.tool_call_id ?? "ygg-tool";
    const piId = bridge.hostToolCallIds.get(hostId) ?? hostId;
    bridge.hostToolCallIds.delete(hostId);
    await bridge.runner.emit({
      type: "tool_execution_end",
      toolCallId: piId,
      toolName: params.tool_name,
      result: {},
      isError: params.outcome !== "completed",
    });
  }
}

async function handleRequest(message) {
  if (message.method === "initialize") return handleInitialize(message);
  if (!bridge?.initialized) throw new Error("Pi compatibility host is not initialized");
  updateHostStateFromMessage(message);
  if (message.method === "event/handle") return handleOrderedEvent(message);
  if (message.method === "event/batch") return handleOrderedEventBatch(message);
  if (message.method === "provider/callback") return handleProviderCallback(message);
  if (LIFECYCLE_EVENTS.includes(message.method)) {
    await handleLifecycle(message.method, message.params ?? {});
    return null;
  }
  if (message.method === "tool/call") return callPiTool(message);
  if (message.method === "command/execute") return executePiCommand(message);
  if (message.method === "hook/run") return runHook(message);
  if (message.method === "context/collect") return collectContext(message);
  if (message.method === "shutdown") {
    await emitSessionShutdown();
    setImmediate(() => process.exit(0));
    return {};
  }
  if (message.method === "$/cancelRequest") {
    const target = message.params?.id;
    inflight.get(keyOf(target))?.controller.abort();
    return null;
  }
  throw new Error(`method not found: ${message.method}`);
}

async function onMessage(message) {
  if (!message || typeof message !== "object") return;
  if (message.id !== undefined && !message.method) {
    const pending = pendingHostRequests.get(message.id);
    if (!pending) return;
    pendingHostRequests.delete(message.id);
    if (message.error) pending.reject(new Error(message.error.message ?? "Ygg host request failed"));
    else pending.resolve(message.result);
    return;
  }
  if (!message.method) return;
  if (message.method === "$/cancelRequest") {
    inflight.get(keyOf(message.params?.id))?.controller.abort();
    return;
  }
  if (message.id === undefined) {
    try {
      await handleRequest(message);
    } catch (error) {
      diagnostic(`Pi compatibility notification failed: ${error instanceof Error ? error.message : String(error)}`);
    }
    return;
  }
  try {
    const invocation = message.method === "event/batch" ? null : message.params?.invocation ?? null;
    const result = await runScoped(message.id, () => handleRequest(message), invocation);
    await send({ jsonrpc: "2.0", id: message.id, result });
  } catch (error) {
    const code = error instanceof CancellationError ? -32800 : -32000;
    await send({
      jsonrpc: "2.0",
      id: message.id,
      error: { code, message: error instanceof Error ? error.message : String(error) },
    });
  }
}

function dispatchIncoming(message) {
  const isResponse = message?.id !== undefined && !message?.method;
  const isCancellation = message?.method === "$/cancelRequest";
  if (isResponse || isCancellation) {
    void onMessage(message);
    return;
  }
  if (message?.method === "tool/call") {
    void orderedInputChain.then(() => onMessage(message));
    return;
  }
  orderedInputChain = orderedInputChain
    .then(() => onMessage(message))
    .catch((error) => boundedDiagnostic(`Pi compatibility dispatch failed: ${error}`));
}

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  if (!line.trim()) return;
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    diagnostic(`Pi compatibility received invalid JSON: ${error}`);
    return;
  }
  dispatchIncoming(message);
});
input.on("close", () => process.exit(0));
process.on("uncaughtException", (error) => diagnostic(`Pi compatibility uncaught exception: ${error.stack ?? error}`));
process.on("unhandledRejection", (error) => diagnostic(`Pi compatibility unhandled rejection: ${error}`));
