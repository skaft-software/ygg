#!/usr/bin/env node

/**
 * Ygg's deliberately small Pi compatibility host.
 *
 * This process is a compatibility boundary, not a second agent kernel. It
 * loads Pi extension factories with Pi's public loader and translates the
 * portable tool, command, lifecycle, notification, input, and confirmation
 * surfaces onto Ygg's API 0.2 JSON-RPC bus.
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
import { pathToFileURL } from "node:url";

const API_VERSION_0_2 = "0.2";
const API_VERSION_0_3 = "0.3";
const API_0_3_SCHEMA = "ygg.extension.api/0.3";
const API_0_3_ENCODING = "ygg-canonical-json-v1";
const API_0_3_MAX_FRAME_BYTES = 1024 * 1024;
const API_0_3_MAX_CONCURRENT_REQUESTS = 64;
const API_0_3_MAX_TOOLS = 256;
const API_0_3_MAX_CONTENT_PARTS = 256;
const API_0_3_MAX_PROVIDERS = 32;
const API_0_3_MAX_PROVIDER_MODELS = 256;
const API_0_3_MAX_PROVIDER_STREAM_EVENTS = 100_000;
const API_0_3_MAX_EXTENSION_FLAGS = 64;
const API_0_3_MAX_DEPTH = 32;
const API_0_3_MAX_PORTABLE_INTEGER = Number.MAX_SAFE_INTEGER;
const API_0_3_REQUIRED_CAPABILITIES = ["content_parts", "core", "request_cancellation", "tool_call"];
const API_0_3_OPTIONAL_CAPABILITIES = [
  "lifecycle_events",
  "migration.adapter.v1",
  "provider_auth",
  "provider_catalog",
  "provider_stream",
  "session_lifecycle",
];
const API_0_3_PROVIDER_CAPABILITIES = ["provider_auth", "provider_catalog", "provider_stream"];
const API_0_3_REQUIRED_METHODS = ["$/cancelRequest", "initialize", "shutdown", "tool/call"];
const API_0_3_OPTIONAL_METHODS = [
  "hook/run",
  "migration/detect",
  "migration/import",
  "provider/auth/request",
  "provider/auth/revoke",
  "provider/cancel",
  "provider/event",
  "provider/stream",
  "providers/register",
  "providers/unregister",
  "providers/update",
  "session/create",
  "session/fork",
  "session/reload",
  "session/switch",
];
const API_0_3_OPTIONAL_METHOD_CAPABILITIES = new Map([
  ["hook/run", "lifecycle_events"],
  ["migration/detect", "migration.adapter.v1"],
  ["migration/import", "migration.adapter.v1"],
  ["provider/auth/request", "provider_auth"],
  ["provider/auth/revoke", "provider_auth"],
  ["provider/cancel", "provider_stream"],
  ["provider/event", "provider_stream"],
  ["provider/stream", "provider_stream"],
  ["providers/register", "provider_catalog"],
  ["providers/unregister", "provider_catalog"],
  ["providers/update", "provider_catalog"],
  ["session/create", "session_lifecycle"],
  ["session/fork", "session_lifecycle"],
  ["session/reload", "session_lifecycle"],
  ["session/switch", "session_lifecycle"],
]);
const API_0_3_PROVIDER_METHODS = [
  "provider/auth/request",
  "provider/auth/revoke",
  "provider/cancel",
  "provider/event",
  "provider/stream",
  "providers/register",
  "providers/unregister",
  "providers/update",
];
const API_0_3_ERROR = {
  parse_error: { code: -32700, message: "parse error" },
  invalid_request: { code: -32600, message: "invalid request" },
  unknown_method: { code: -32601, message: "unknown or unnegotiated method" },
  invalid_params: { code: -32602, message: "invalid params" },
  internal_error: { code: -32603, message: "internal error" },
  version_mismatch: { code: -32010, message: "extension API version mismatch" },
  capability_mismatch: { code: -32011, message: "extension capability mismatch" },
  resource_exhausted: { code: -32012, message: "extension resource exhausted" },
  request_cancelled: { code: -32800, message: "request cancelled" },
};
const BRIDGE_VERSION = "0.3.0";
const SUPPORTED_PI_PACKAGE = "@earendil-works/pi-coding-agent";
const SUPPORTED_PI_VERSION = "0.84.4";
const MINIMUM_NODE_VERSION = [22, 19, 0];
const MAX_PI_PACKAGE_MANIFEST_BYTES = 256 * 1024;
const SOURCE_FINGERPRINT_FORMAT = 1;
const SOURCE_LOCK_FINGERPRINT_FORMAT = 1;
const PI_RUNTIME_INTEGRITY_FORMAT = 1;
const LINK_IDENTITY_FORMAT = 1;
const EXPLICIT_TRUST_MODE = "explicit_enable_and_trust_required";
const MAX_LOCK_FILE_BYTES = 16 * 1024 * 1024;
const MAX_LOCK_BYTES = 64 * 1024 * 1024;
const SUPPORTED_LOCK_FILES = [
  "package-lock.json",
  "npm-shrinkwrap.json",
  "pnpm-lock.yaml",
  "yarn.lock",
  "bun.lockb",
];
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
const REQUIRED_FEATURES = ["request_cancellation", "content_parts"];
const OPTIONAL_FEATURES = [
  "request_progress",
  "artifacts",
  "lifecycle_events",
  "dynamic_tools",
  "runtime_commands",
];
const LIFECYCLE_EVENTS = [
  "session/started",
  "session/settled",
  "turn/started",
  "turn/settled",
  "tool/started",
  "tool/settled",
];
const BRIDGED_PI_EVENTS = new Set([
  "session_start",
  "session_shutdown",
  "context",
  "before_agent_start",
  "agent_start",
  "agent_end",
  "agent_settled",
  "turn_start",
  "turn_end",
  "tool_execution_start",
  "tool_execution_update",
  "tool_execution_end",
  "tool_call",
  "tool_result",
]);

function bridgedPiEvent(event) {
  return typeof event === "string" && BRIDGED_PI_EVENTS.has(event);
}

const args = parseArgs(process.argv.slice(2));
const protocolWrite = process.stdout.write.bind(process.stdout);
const scopes = new AsyncLocalStorage();
const pendingHostRequests = new Map();
const inflight = new Map();
const pendingV03IncomingRequests = new Map();
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
    sourceLockFingerprints: [],
    agentDir: null,
    cwd: null,
    commandName: "pi",
    piPackage: null,
    piRuntimeIntegrity: null,
    aggregateDigest: null,
    linkManifest: null,
    linkIdentity: null,
    yggVersion: null,
    apiVersion: API_VERSION_0_2,
  };
  const sha256 = (value, flag) => {
    if (!/^[0-9a-f]{64}$/.test(value ?? "")) {
      throw new Error(`${flag} requires a lowercase SHA-256 digest`);
    }
    return value;
  };
  const requiredValue = (flag, index) => {
    const value = argv[index + 1];
    if (!value) throw new Error(`${flag} requires a value`);
    return value;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--extension" || value === "-e") {
      const extension = requiredValue(value, index);
      index += 1;
      result.extensions.push(extension);
    } else if (value === "--source-fingerprint") {
      const fingerprint = sha256(requiredValue(value, index), value);
      index += 1;
      result.sourceFingerprints.push(fingerprint);
    } else if (value === "--source-lock-fingerprint") {
      const fingerprint = sha256(requiredValue(value, index), value);
      index += 1;
      result.sourceLockFingerprints.push(fingerprint);
    } else if (value === "--agent-dir") {
      result.agentDir = requiredValue(value, index);
      index += 1;
    } else if (value === "--cwd") {
      result.cwd = requiredValue(value, index);
      index += 1;
    } else if (value === "--pi-package") {
      result.piPackage = requiredValue(value, index);
      index += 1;
    } else if (value === "--pi-runtime-integrity") {
      result.piRuntimeIntegrity = sha256(requiredValue(value, index), value);
      index += 1;
    } else if (value === "--aggregate-digest") {
      result.aggregateDigest = sha256(requiredValue(value, index), value);
      index += 1;
    } else if (value === "--link-manifest") {
      result.linkManifest = requiredValue(value, index);
      index += 1;
    } else if (value === "--link-identity") {
      result.linkIdentity = sha256(requiredValue(value, index), value);
      index += 1;
    } else if (value === "--ygg-version") {
      result.yggVersion = requiredValue(value, index);
      index += 1;
    } else if (value === "--api-version") {
      result.apiVersion = requiredValue(value, index);
      index += 1;
      if (result.apiVersion !== API_VERSION_0_2 && result.apiVersion !== API_VERSION_0_3) {
        throw new Error("--api-version must be 0.2 or 0.3");
      }
    } else if (value === "--command") {
      result.commandName = requiredValue(value, index);
      index += 1;
    } else if (value === "--help" || value === "-h") {
      process.stdout.write(
        "Usage: bridge.mjs --extension PATH [--source-fingerprint SHA256] [--source-lock-fingerprint SHA256] [--extension PATH ...] [--agent-dir DIR] [--pi-package DIR]\n",
      );
      process.exit(0);
    } else {
      throw new Error(`unknown bridge argument ${value}`);
    }
  }
  if (result.extensions.length === 0 && process.env.YGG_PI_EXTENSION) {
    result.extensions.push(process.env.YGG_PI_EXTENSION);
  }
  if (result.extensions.length === 0) {
    throw new Error("at least one --extension path is required");
  }
  if (
    result.sourceFingerprints.length !== 0
    && result.sourceFingerprints.length !== result.extensions.length
  ) {
    throw new Error("provide exactly one --source-fingerprint for each --extension");
  }
  if (
    result.sourceLockFingerprints.length !== 0
    && result.sourceLockFingerprints.length !== result.extensions.length
  ) {
    throw new Error("provide exactly one --source-lock-fingerprint for each --extension");
  }
  const identityFields = [
    result.piRuntimeIntegrity,
    result.aggregateDigest,
    result.linkManifest,
    result.linkIdentity,
    result.yggVersion,
  ];
  result.strictIdentity = identityFields.some(Boolean);
  if (result.strictIdentity) {
    if (
      !result.piPackage
      || result.sourceFingerprints.length !== result.extensions.length
      || result.sourceLockFingerprints.length !== result.extensions.length
      || identityFields.some((value) => !value)
    ) {
      throw new Error(
        "pinned Pi aggregate identity requires --pi-package, per-source source/lock fingerprints, runtime integrity, aggregate digest, link manifest, link identity, and Ygg version",
      );
    }
  }
  return result;
}

function keyOf(id) {
  return typeof id === "string" ? `s:${id}` : `n:${String(id)}`;
}

function reserveV03IncomingRequest(message) {
  if (!isApiV03() || !bridge?.initialized) return true;
  const key = keyOf(message.id);
  if (pendingV03IncomingRequests.has(key)) {
    void sendV03Error(
      message.id,
      v03ProtocolError("invalid_request", "API 0.3 request id is already active"),
    ).catch((error) => boundedDiagnostic(`Pi compatibility request rejection failed: ${error}`));
    return false;
  }
  if (pendingV03IncomingRequests.size >= bridge.v03Contract.limits.max_concurrent_requests) {
    void sendV03Error(
      message.id,
      v03ProtocolError("resource_exhausted", "API 0.3 concurrent request limit is exhausted"),
    ).catch((error) => boundedDiagnostic(`Pi compatibility request rejection failed: ${error}`));
    return false;
  }
  pendingV03IncomingRequests.set(key, { cancelled: false });
  return true;
}

function releaseV03IncomingRequest(message) {
  if (isApiV03()) pendingV03IncomingRequests.delete(keyOf(message.id));
}

function isApiV03IncomingRequestCancelled(message) {
  return isApiV03() && pendingV03IncomingRequests.get(keyOf(message.id))?.cancelled === true;
}

function isApiV03() {
  return args.apiVersion === API_VERSION_0_3;
}

function hasLoneSurrogate(value) {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (next < 0xdc00 || next > 0xdfff) return true;
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

function compareUnicodeCodePoints(left, right) {
  const leftPoints = Array.from(left, (character) => character.codePointAt(0));
  const rightPoints = Array.from(right, (character) => character.codePointAt(0));
  const length = Math.min(leftPoints.length, rightPoints.length);
  for (let index = 0; index < length; index += 1) {
    if (leftPoints[index] !== rightPoints[index]) return leftPoints[index] - rightPoints[index];
  }
  return leftPoints.length - rightPoints.length;
}

function canonicalJson(value, depth = 0) {
  if (depth > API_0_3_MAX_DEPTH) {
    throw new Error("canonical JSON nesting exceeds max_json_depth");
  }
  if (value === null || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "string") {
    if (hasLoneSurrogate(value)) throw new Error("canonical JSON strings must be valid UTF-8");
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value) || Math.abs(value) > API_0_3_MAX_PORTABLE_INTEGER) {
      throw new Error("canonical JSON permits only portable integers");
    }
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map((entry) => canonicalJson(entry, depth + 1)).join(",")}]`;
  }
  if (typeof value === "object") {
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) {
      throw new Error("canonical JSON objects must be plain objects");
    }
    const keys = Object.keys(value).sort(compareUnicodeCodePoints);
    for (const key of keys) {
      if (hasLoneSurrogate(key)) throw new Error("canonical JSON object keys must be valid UTF-8");
    }
    return `{${keys.map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key], depth + 1)}`).join(",")}}`;
  }
  throw new Error(`canonical JSON value is unsupported: ${typeof value}`);
}

function assertV03Frame(message) {
  const frame = canonicalJson(message);
  if (Buffer.byteLength(frame) > (bridge?.frameLimit ?? API_0_3_MAX_FRAME_BYTES)) {
    throw v03ProtocolError("resource_exhausted", "canonical frame exceeds negotiated max_frame_bytes");
  }
  return frame;
}

function parentRequestId() {
  const value = scopes.getStore()?.parentRequestId;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error("Pi compatibility host has no active Ygg request owner");
  }
  return value;
}

function send(message, shouldWrite = undefined) {
  const line = `${isApiV03() ? assertV03Frame(message) : JSON.stringify(message)}\n`;
  outputChain = outputChain.then(
    () => {
      if (shouldWrite && !shouldWrite()) return false;
      return new Promise((resolveWrite, rejectWrite) => {
        protocolWrite(line, (error) =>
          error ? rejectWrite(error) : resolveWrite(true),
        );
      });
    },
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
  if (isApiV03() && !bridge?.v03Contract?.methods?.includes(method)) {
    throw providerCapabilityError(`${method} is not selected by the API 0.3 contract`);
  }
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

function currentScope() {
  const scope = scopes.getStore();
  if (!scope) throw new Error("Pi compatibility API used outside an active request");
  return scope;
}

function notify(level, title, message) {
  if (isApiV03()) unsupported("ctx.ui.notify (API 0.3 does not negotiate notifications)");
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
    async editor() {
      return unsupported("ctx.ui.editor");
    },
    notify(message, type = "info") {
      void notify(type, "Pi extension", message);
    },
    onTerminalInput() {
      return unsupported("ctx.ui.onTerminalInput");
    },
    setStatus(key, text) {
      if (isApiV03()) unsupported("ctx.ui.setStatus (API 0.3 does not negotiate UI surfaces)");
      return send({
        jsonrpc: "2.0",
        method: "status/contribution",
        params: {
          surface: "status",
          text: text === undefined ? "" : `${key}: ${text}`,
          style_role: "extension.pi.status",
          priority: 0,
        },
      });
    },
    setWorkingMessage() {
      return unsupported("ctx.ui.setWorkingMessage");
    },
    setWorkingVisible() {
      return unsupported("ctx.ui.setWorkingVisible");
    },
    setWorkingIndicator() {
      return unsupported("ctx.ui.setWorkingIndicator");
    },
    setHiddenThinkingLabel() {
      return unsupported("ctx.ui.setHiddenThinkingLabel");
    },
    setWidget() {
      return unsupported("ctx.ui.setWidget");
    },
    setFooter() {
      return unsupported("ctx.ui.setFooter");
    },
    setHeader() {
      return unsupported("ctx.ui.setHeader");
    },
    setTitle() {
      return unsupported("ctx.ui.setTitle");
    },
    custom() {
      return unsupported("ctx.ui.custom");
    },
    pasteToEditor() {
      return unsupported("ctx.ui.pasteToEditor");
    },
    setEditorText() {
      return unsupported("ctx.ui.setEditorText");
    },
    getEditorText() {
      return unsupported("ctx.ui.getEditorText");
    },
    addAutocompleteProvider() {
      return unsupported("ctx.ui.addAutocompleteProvider");
    },
    setAutocompleteProvider() {
      return unsupported("ctx.ui.setAutocompleteProvider");
    },
    setEditorComponent() {
      return unsupported("ctx.ui.setEditorComponent");
    },
    getEditorComponent() {
      return unsupported("ctx.ui.getEditorComponent");
    },
    getAllThemes() {
      return unsupported("ctx.ui.getAllThemes");
    },
    getTheme() {
      return unsupported("ctx.ui.getTheme");
    },
    setTheme() {
      return unsupported("ctx.ui.setTheme");
    },
    getToolsExpanded() {
      return unsupported("ctx.ui.getToolsExpanded");
    },
    setToolsExpanded() {
      return unsupported("ctx.ui.setToolsExpanded");
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
  const state = message?.params?.context?.host;
  if (state && typeof state === "object" && !Array.isArray(state)) {
    bridge.hostState = { ...bridge.hostState, ...state };
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
    getModel: () => undefined,
    getScopedModels: () => [],
    isIdle: () => bridge.agentActive !== true,
    isProjectTrusted: () => false,
    getSignal: () => scopes.getStore()?.signal,
    abort: () => currentScope().controller.abort(),
    hasPendingMessages: () => unsupported("ctx.hasPendingMessages"),
    shutdown: () => unsupported("ctx.shutdown"),
    getContextUsage: () => undefined,
    compact: () => unsupported("ctx.compact"),
    getSystemPrompt: () => unsupported("ctx.getSystemPrompt"),
    getSystemPromptOptions: () => ({ cwd: bridge.cwd }),
  };
}

function makeExtensionActions() {
  return {
    sendMessage: () => unsupported("pi.sendMessage"),
    sendUserMessage: () => unsupported("pi.sendUserMessage"),
    appendEntry: () => unsupported("pi.appendEntry"),
    setSessionName: () => unsupported("pi.setSessionName"),
    getSessionName: () => bridge.hostState?.session_name,
    setLabel: () => unsupported("pi.setLabel"),
    getActiveTools: () => bridge.toolNames,
    getAllTools: () => bridge.toolInfos,
    setActiveTools: () => unsupported("pi.setActiveTools"),
    refreshTools: () => (isApiV03() ? unsupported("pi.refreshTools (API 0.3 tool catalogs are fixed at initialization)") : scheduleToolRefresh()),
    getCommands: () => bridge.runner?.getRegisteredCommands?.() ?? [],
    setModel: () => Promise.reject(new Error("Pi compatibility API is not supported by Ygg: pi.setModel")),
    getThinkingLevel: () => thinkingLevelFromHost(),
    setThinkingLevel: () => unsupported("pi.setThinkingLevel"),
  };
}

function makeModelRegistry() {
  return makeThrowingProxy("ctx.modelRegistry");
}

function makeSessionManager() {
  return makeThrowingProxy("ctx.sessionManager");
}

const PROVIDER_IDENTIFIER = /^[a-z][a-z0-9_-]*$/;
const PROVIDER_UNSAFE_FIELDS = new Set([
  "apikey",
  "authheader",
  "authorization",
  "baseurl",
  "callback",
  "credential",
  "credentials",
  "endpoint",
  "endpoints",
  "header",
  "headers",
  "lease",
  "oauth",
  "secret",
  "streamsimple",
  "token",
  "transport",
  "url",
]);
const PROVIDER_SEMANTIC_TOKEN_FIELDS = new Set([
  "completiontokens",
  "inputtokens",
  "maxtokens",
  "mintokens",
  "outputtokens",
  "prompttokens",
  "tokenbudget",
  "tokencount",
  "totaltokens",
]);
const PI_PROVIDER_CONFIG_FIELDS = new Set(["name", "api", "models", "auth", "yggAuth", "yggStream"]);
const PI_PROVIDER_MODEL_FIELDS = new Set([
  "id",
  "name",
  "api",
  "apiName",
  "api_name",
  "reasoning",
  "contextWindow",
  "maxTokens",
  "capabilities",
]);
const PROVIDER_STREAM_KINDS = new Set([
  "started",
  "text_start",
  "text_delta",
  "text_end",
  "reasoning_start",
  "reasoning_delta",
  "reasoning_end",
  "tool_call_start",
  "tool_call_args_delta",
  "tool_call_end",
  "usage",
  "finished",
  "heartbeat",
  "error",
]);

function providerError(message) {
  const error = new Error(`Pi provider compatibility rejected: ${message}`);
  error.code = API_0_3_ERROR.invalid_params.code;
  return error;
}

function providerCapabilityError(message) {
  const error = new Error(`Pi provider compatibility requires negotiated API 0.3 provider support: ${message}`);
  error.code = API_0_3_ERROR.capability_mismatch.code;
  return error;
}

function asPlainObject(value, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw providerError(`${label} must be an object`);
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) {
    throw providerError(`${label} must be a plain object`);
  }
  return value;
}

function normalizedProviderFieldName(name) {
  return String(name).replaceAll("_", "").replaceAll("-", "").toLowerCase();
}

function isUnsafeProviderField(name) {
  const normalized = normalizedProviderFieldName(name);
  return PROVIDER_UNSAFE_FIELDS.has(normalized)
    || normalized.includes("oauth")
    || normalized.includes("callback")
    || normalized.includes("credential")
    || normalized.includes("apikey")
    || normalized.includes("authorization")
    || normalized.includes("header")
    || normalized.includes("endpoint")
    || normalized.includes("transport")
    || normalized.includes("lease")
    || normalized.includes("secret")
    || (normalized.includes("token") && !PROVIDER_SEMANTIC_TOKEN_FIELDS.has(normalized))
    || normalized.includes("url");
}

function assertNoUnsafeProviderTransportFields(value, label) {
  // `request` is host-owned semantic model input. Nested values can legitimately
  // include user/tool JSON-schema keys such as `url` or `headers`; treating
  // every such key as transport authority would corrupt valid prompts. Only an
  // envelope-level authority field could change how an adapter reaches a
  // provider, and it is rejected before a Pi hook or adapter sees the request.
  if (!value || typeof value !== "object" || Array.isArray(value)) return;
  for (const name of Object.keys(value)) {
    if (isUnsafeProviderField(name)) {
      throw providerError(`${label}.${name} would expose provider network or credential authority`);
    }
  }
}

function assertOnlyProviderFields(value, allowed, label) {
  for (const name of Object.keys(value)) {
    if (isUnsafeProviderField(name)) {
      throw providerError(`${label}.${name} would expose provider network or credential authority`);
    }
    if (!allowed.has(name)) throw providerError(`${label}.${name} is not a supported secret-free field`);
  }
}

function boundedProviderIdentifier(value, label) {
  if (
    typeof value !== "string"
    || Buffer.byteLength(value) > 64
    || !PROVIDER_IDENTIFIER.test(value)
  ) {
    throw providerError(`${label} must be a lowercase ASCII provider identifier`);
  }
  return value;
}

function boundedProviderLabel(value, label) {
  if (
    typeof value !== "string"
    || !value.trim()
    || Buffer.byteLength(value) > 128
    || /[\u0000-\u001f\u007f]/.test(value)
  ) {
    throw providerError(`${label} must be a non-empty bounded display label`);
  }
  return value;
}

function positiveProviderInteger(value, label) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw providerError(`${label} must be a positive portable integer`);
  }
  return value;
}

function optionalOpaqueName(value, label, maximumBytes = 64) {
  if (
    typeof value !== "string"
    || !value
    || Buffer.byteLength(value) > maximumBytes
    || !/^[A-Za-z0-9_.-]+$/.test(value)
  ) {
    throw providerError(`${label} must be a bounded opaque name`);
  }
  return value;
}

function boundedOpaqueValue(value, label, maximumBytes = 256) {
  // Host-issued leases are opaque protocol values, not Pi identifiers. Accept
  // every contract-valid string while retaining neither its value nor a
  // reference to it after boundary validation.
  if (typeof value !== "string" || Buffer.byteLength(value) > maximumBytes) {
    throw providerError(`${label} must be a bounded opaque string`);
  }
  return value;
}

function normalizeProviderAuth(value) {
  if (value === undefined) return { kind: "none" };
  const auth = asPlainObject(value, "provider auth");
  assertOnlyProviderFields(auth, new Set(["kind", "subject", "scopes"]), "provider auth");
  if (typeof auth.kind !== "string") throw providerError("provider auth.kind must be a string");
  const kind = auth.kind;
  const subject = auth.subject === undefined ? undefined : boundedProviderIdentifier(auth.subject, "provider auth.subject");
  let scopes;
  if (auth.scopes !== undefined) {
    if (!Array.isArray(auth.scopes) || auth.scopes.length > 32) {
      throw providerError("provider auth.scopes must contain at most 32 opaque scope names");
    }
    const seen = new Set();
    scopes = auth.scopes.map((scope, index) => {
      const normalized = optionalOpaqueName(scope, `provider auth.scopes[${index}]`);
      if (seen.has(normalized)) throw providerError("provider auth.scopes must be unique");
      seen.add(normalized);
      return normalized;
    });
  }
  if (kind === "none") {
    if (subject !== undefined || scopes !== undefined) {
      throw providerError("provider auth kind none cannot carry subject or scopes");
    }
    return { kind };
  }
  if (kind === "oauth") {
    if (subject === undefined) throw providerError("OAuth provider auth requires a subject");
    return { kind, subject, ...(scopes === undefined ? {} : { scopes }) };
  }
  if (kind === "host_credential") {
    if (subject === undefined) throw providerError("host credential provider auth requires a subject");
    if (scopes !== undefined) throw providerError("host credential provider auth cannot declare OAuth scopes");
    return { kind, subject };
  }
  throw providerError(`provider auth kind ${JSON.stringify(kind)} is unsupported`);
}

function normalizeProviderProtocol(value, label) {
  if (typeof value !== "string") throw providerError(`${label} must be a supported Pi API name`);
  const protocols = new Map([
    ["openai-completions", "openai_chat"],
    ["openai-chat", "openai_chat"],
    ["openai_chat", "openai_chat"],
    ["openai-responses", "openai_responses"],
    ["openai_responses", "openai_responses"],
    ["anthropic-messages", "anthropic_messages"],
    ["anthropic_messages", "anthropic_messages"],
  ]);
  const protocol = protocols.get(value);
  if (!protocol) {
    throw providerError(`${label} ${JSON.stringify(value)} has no host-owned API 0.3 codec`);
  }
  return protocol;
}

function normalizeProviderCapabilities(value, reasoning) {
  if (value === undefined) {
    return {
      tools: true,
      parallel_tool_calls: false,
      structured_output: false,
      reasoning: reasoning === true,
    };
  }
  const capabilities = asPlainObject(value, "provider model capabilities");
  assertOnlyProviderFields(
    capabilities,
    new Set(["tools", "parallel_tool_calls", "structured_output", "reasoning"]),
    "provider model capabilities",
  );
  for (const name of ["tools", "parallel_tool_calls", "structured_output", "reasoning"]) {
    if (typeof capabilities[name] !== "boolean") {
      throw providerError(`provider model capabilities.${name} must be boolean`);
    }
  }
  if (capabilities.parallel_tool_calls && !capabilities.tools) {
    throw providerError("provider model parallel_tool_calls requires tools");
  }
  return {
    tools: capabilities.tools,
    parallel_tool_calls: capabilities.parallel_tool_calls,
    structured_output: capabilities.structured_output,
    reasoning: capabilities.reasoning,
  };
}

function normalizePiProvider(name, config) {
  const providerId = boundedProviderIdentifier(name, "provider name");
  const source = asPlainObject(config, `provider ${providerId} config`);
  assertOnlyProviderFields(source, PI_PROVIDER_CONFIG_FIELDS, `provider ${providerId} config`);
  if (source.auth !== undefined && source.yggAuth !== undefined) {
    throw providerError(`provider ${providerId} config cannot declare both auth and yggAuth`);
  }
  if (!Array.isArray(source.models) || source.models.length === 0) {
    throw providerError(`provider ${providerId} config.models must declare a complete non-empty model catalog`);
  }
  if (source.models.length > 256) throw providerError(`provider ${providerId} has too many models`);
  const auth = normalizeProviderAuth(source.auth ?? source.yggAuth);
  const modelIds = new Set();
  const models = source.models.map((sourceModel, index) => {
    const model = asPlainObject(sourceModel, `provider ${providerId} model ${index}`);
    assertOnlyProviderFields(model, PI_PROVIDER_MODEL_FIELDS, `provider ${providerId} model ${index}`);
    const id = boundedProviderIdentifier(model.id, `provider ${providerId} model ${index}.id`);
    if (modelIds.has(id)) throw providerError(`provider ${providerId} model identifiers must be unique`);
    modelIds.add(id);
    const apiNameValues = [model.apiName, model.api_name].filter((value) => value !== undefined);
    if (apiNameValues.length > 1 && apiNameValues[0] !== apiNameValues[1]) {
      throw providerError(`provider ${providerId} model ${id} cannot disagree on apiName and api_name`);
    }
    const apiName = apiNameValues[0] ?? id;
    if (
      typeof apiName !== "string"
      || !apiName.trim()
      || Buffer.byteLength(apiName) > 64
      || /[\u0000-\u001f\u007f]/.test(apiName)
    ) {
      throw providerError(`provider ${providerId} model ${id}.apiName is invalid`);
    }
    if (model.reasoning !== undefined && typeof model.reasoning !== "boolean") {
      throw providerError(`provider ${providerId} model ${id}.reasoning must be boolean`);
    }
    if (model.reasoning !== undefined && model.capabilities !== undefined) {
      throw providerError(`provider ${providerId} model ${id} cannot combine reasoning and capabilities`);
    }
    const contextWindow = positiveProviderInteger(
      model.contextWindow,
      `provider ${providerId} model ${id}.contextWindow`,
    );
    const maxOutputTokens = positiveProviderInteger(
      model.maxTokens,
      `provider ${providerId} model ${id}.maxTokens`,
    );
    if (maxOutputTokens > contextWindow) {
      throw providerError(`provider ${providerId} model ${id}.maxTokens exceeds contextWindow`);
    }
    return {
      id,
      api_name: apiName,
      protocol: normalizeProviderProtocol(model.api ?? source.api, `provider ${providerId} model ${id}.api`),
      context_window: contextWindow,
      max_output_tokens: maxOutputTokens,
      capabilities: normalizeProviderCapabilities(model.capabilities, model.reasoning),
      ...(model.name === undefined
        ? {}
        : { display_name: boundedProviderLabel(model.name, `provider ${providerId} model ${id}.name`) }),
    };
  });
  if (source.yggStream !== undefined && typeof source.yggStream !== "function") {
    throw providerError(`provider ${providerId} yggStream must be a function`);
  }
  return {
    provider: {
      id: providerId,
      label: boundedProviderLabel(source.name ?? providerId, `provider ${providerId} name`),
      auth,
    },
    models,
    // This ephemeral function is never sent to the host, serialized, or given
    // an authorization lease, endpoint, header, or credential value.
    adapter: source.yggStream,
  };
}

function canonicalClone(value) {
  return JSON.parse(canonicalJson(value));
}

function providerContractAvailable() {
  return bridge?.providerContract === true;
}

function providerCatalogParams(entry) {
  return { provider: entry.provider, models: entry.models };
}

function ensureProviderRegistrationAllowed() {
  if (!isApiV03()) unsupported("pi.registerProvider");
  if (!providerContractAvailable()) {
    throw providerCapabilityError("provider_catalog, provider_stream, and provider_auth must all be selected");
  }
}

function registerPiProvider(name, config) {
  try {
    ensureProviderRegistrationAllowed();
    const normalized = normalizePiProvider(name, config);
    if (typeof normalized.adapter !== "function") {
      throw providerError(`provider ${normalized.provider.id} requires a secret-free yggStream adapter`);
    }
    const providerId = normalized.provider.id;
    const existing = bridge.providers.get(providerId);
    if (!existing && bridge.providers.size >= API_0_3_MAX_PROVIDERS) {
      throw providerError(`provider catalog exceeds ${API_0_3_MAX_PROVIDERS} providers`);
    }
    const modelCount = [...bridge.providers.entries()]
      .filter(([id]) => id !== providerId)
      .reduce((count, [, entry]) => count + entry.models.length, 0) + normalized.models.length;
    if (modelCount > API_0_3_MAX_PROVIDER_MODELS) {
      throw providerError(`provider catalog exceeds ${API_0_3_MAX_PROVIDER_MODELS} models`);
    }
    const hostMayBePublished = existing?.hostPublished === true;
    // A declaration mutation must first fence every active stream using the
    // old adapter. Marking them synchronously prevents the pump from queuing
    // another nonterminal event while this callback returns to Pi.
    const retiringStreams = existing ? markProviderStreamsForMutation(providerId) : [];
    const entry = {
      provider: normalized.provider,
      models: normalized.models,
      adapter: normalized.adapter,
      // Do not route host calls through a changed local declaration until the
      // host has atomically accepted its replacement and authorization state.
      published: false,
      // A prior reverse request may still be in flight. Preserve that cleanup
      // obligation across a replacement even before it has acknowledged.
      hostPublished: hostMayBePublished,
      authorization_status: undefined,
    };
    bridge.providers.set(providerId, entry);
    if (bridge.initialized) {
      queueProviderMutation(
        hostMayBePublished ? "providers/update" : "providers/register",
        entry,
        retiringStreams,
      );
    }
  } catch (error) {
    bridge.providerRegistrationFailure = error;
    throw error;
  }
}

function unregisterPiProvider(name) {
  ensureProviderRegistrationAllowed();
  const providerId = boundedProviderIdentifier(name, "provider name");
  const entry = bridge.providers.get(providerId);
  if (!entry) return;
  // Fence the old adapter before the unregister reverse request can make the
  // declaration unavailable to the host.
  const retiringStreams = markProviderStreamsForMutation(providerId);
  bridge.providers.delete(providerId);
  if (bridge.initialized && (entry.published || entry.hostPublished)) {
    bridge.retiredProviders.add(entry);
    queueProviderUnregister(providerId, entry, retiringStreams);
  }
}

function assertProviderCatalogResult(result) {
  const value = asPlainObject(result, "host provider catalog result");
  const allowed = new Set(["revision", "provider_ids", "model_ids"]);
  for (const name of Object.keys(value)) {
    if (!allowed.has(name)) throw providerError(`host provider catalog result.${name} is not supported`);
  }
  if (!Number.isSafeInteger(value.revision) || value.revision < 0) {
    throw providerError("host returned an invalid provider catalog revision");
  }
  for (const [name, maximumItems] of [["provider_ids", 32], ["model_ids", 256]]) {
    const values = value[name];
    if (
      !Array.isArray(values)
      || values.length > maximumItems
      || values.some((item) => typeof item !== "string")
    ) {
      throw providerError(`host returned invalid provider catalog ${name}`);
    }
  }
  return value;
}

function assertProviderAuthorizationResult(result) {
  const value = asPlainObject(result, "host provider authorization result");
  const allowed = new Set(["status", "lease"]);
  for (const name of Object.keys(value)) {
    if (!allowed.has(name)) throw providerError(`host provider authorization result.${name} is not supported`);
  }
  if (!new Set(["ready", "pending", "denied", "unavailable", "revoked"]).has(value.status)) {
    throw providerError("host returned an invalid provider authorization status");
  }
  if (value.lease !== undefined) {
    boundedOpaqueValue(value.lease, "host provider authorization lease");
    if (value.status !== "ready") {
      throw providerError("host returned a provider authorization lease for a non-ready status");
    }
  }
  // Do not return the host's opaque lease: no caller may retain or expose it.
  return value.status;
}

function recordProviderCatalogResult(entry, result) {
  const catalog = assertProviderCatalogResult(result);
  const expectedModels = entry.models.map((model) => `${entry.provider.id}/${model.id}`);
  if (
    catalog.provider_ids.length !== 1
    || catalog.provider_ids[0] !== entry.provider.id
    || catalog.model_ids.length !== expectedModels.length
    || expectedModels.some((modelId) => !catalog.model_ids.includes(modelId))
  ) {
    throw providerError("host provider catalog acknowledgement does not match the published declaration");
  }
  bridge.providerCatalogRevision = catalog.revision;
  entry.published = true;
  entry.hostPublished = true;
}

async function requestProviderAuthorization(entry, action) {
  if (entry.provider.auth.kind === "none") {
    entry.authorization_status = "ready";
    return;
  }
  const method = action === "revoke" ? "provider/auth/revoke" : "provider/auth/request";
  const params = {
    provider_id: entry.provider.id,
    action,
    interactive: false,
    ...(entry.provider.auth.scopes ? { scopes: entry.provider.auth.scopes } : {}),
  };
  const status = assertProviderAuthorizationResult(await requestHost(method, params));
  // Deliberately retain status only. In particular, an opaque lease is neither
  // stored nor passed to Pi code, provider adapters, or future requests.
  entry.authorization_status = status;
}

async function publishProviderMutation(method, entry) {
  // A queued registration can become obsolete before its turn (for example,
  // register followed by unregister in one Pi callback). Never publish an
  // entry that is no longer the current local declaration.
  if (bridge.providers.get(entry.provider.id) !== entry) return;
  // A reverse request can time out after the host has committed its catalog
  // mutation. Treat the route as potentially published before issuing it so
  // later replacement/shutdown cleanup unregisters rather than leaking it.
  entry.hostPublished = true;
  const result = await requestHost(method, providerCatalogParams(entry));
  recordProviderCatalogResult(entry, result);
  await requestProviderAuthorization(entry, method === "providers/update" ? "refresh" : "authorize");
  bridge.providerSyncError = null;
}

function queueProviderMutation(method, entry, retiringStreams = []) {
  bridge.providerSyncChain = bridge.providerSyncChain
    .then(async () => {
      await terminateProviderStreamsForMutation(retiringStreams);
      await publishProviderMutation(method, entry);
    })
    .catch((error) => {
      bridge.providerSyncError = error;
      diagnostic(`Pi provider catalog mutation failed: ${error instanceof Error ? error.message : String(error)}`);
    });
}

function queueInitialProviders() {
  if (!providerContractAvailable()) return;
  for (const entry of bridge.providers.values()) {
    if (!entry.published) queueProviderMutation("providers/register", entry);
  }
}

async function publishProviderUnregister(providerId, entry) {
  try {
    await requestProviderAuthorization(entry, "revoke");
  } catch (error) {
    // A host registry removal is still the fail-closed cleanup path. Keep the
    // revocation failure inspectable instead of retaining a callable route.
    diagnostic(`Pi provider authorization revoke failed: ${error instanceof Error ? error.message : String(error)}`);
  }
  const result = await requestHost("providers/unregister", { provider_id: providerId });
  const catalog = assertProviderCatalogResult(result);
  if (catalog.provider_ids.length !== 0 || catalog.model_ids.length !== 0) {
    throw providerError("host provider removal acknowledgement must not retain provider or model identifiers");
  }
  bridge.providerCatalogRevision = catalog.revision;
  entry.published = false;
  entry.hostPublished = false;
  entry.authorization_status = "revoked";
}

function queueProviderUnregister(providerId, entry, retiringStreams = []) {
  bridge.providerSyncChain = bridge.providerSyncChain
    .then(async () => {
      await terminateProviderStreamsForMutation(retiringStreams);
      await publishProviderUnregister(providerId, entry);
      bridge.retiredProviders.delete(entry);
    })
    .catch((error) => {
      bridge.providerSyncError = error;
      diagnostic(`Pi provider removal failed: ${error instanceof Error ? error.message : String(error)}`);
    });
}

function providerReason(error) {
  const value = error instanceof Error ? error.message : String(error);
  const bytes = Buffer.from(value, "utf8");
  return bytes.length <= 4096 ? value : `${bytes.subarray(0, 4096).toString("utf8")}…`;
}

function publicProviderReason() {
  // Provider adapters and host policy errors may contain implementation detail.
  // A stream disposition is inspectable, but never a channel for endpoint or
  // credential material.
  return "provider stream setup failed";
}

function awaitWithCancellation(value, signal) {
  if (signal?.aborted) return Promise.reject(new CancellationError());
  if (!signal) return Promise.resolve(value);
  return new Promise((resolveValue, rejectValue) => {
    const onAbort = () => {
      signal.removeEventListener("abort", onAbort);
      rejectValue(new CancellationError());
    };
    signal.addEventListener("abort", onAbort, { once: true });
    Promise.resolve(value).then(
      (result) => {
        signal.removeEventListener("abort", onAbort);
        resolveValue(result);
      },
      (error) => {
        signal.removeEventListener("abort", onAbort);
        rejectValue(error);
      },
    );
  });
}

function hasPiProviderHook(name) {
  return bridge.loadedExtensions?.some((extension) => {
    const handlers = extension?.handlers;
    const values = handlers?.get?.(name);
    return Array.isArray(values) && values.length > 0;
  });
}

function rejectUnsupportedProviderHooks() {
  if (hasPiProviderHook("before_provider_headers")) {
    throw providerError(
      "before_provider_headers is unavailable because headers remain host-owned provider authority",
    );
  }
}

async function emitBeforeProviderRequest(request) {
  let transformed = request;
  if (typeof bridge.runner.emitBeforeProviderRequest === "function") {
    transformed = await bridge.runner.emitBeforeProviderRequest(request);
  } else if (hasPiProviderHook("before_provider_request")) {
    transformed = await bridge.runner.emit({ type: "before_provider_request", payload: request });
  }
  const candidate = transformed === undefined ? request : transformed;
  assertNoUnsafeProviderTransportFields(candidate, "before_provider_request payload");
  // Canonicalize and clone the hook result so no extension-owned object or
  // prototype crosses into the host-owned stream route.
  return canonicalClone(candidate);
}

async function emitAfterProviderResponse(status) {
  const event = { type: "after_provider_response", status, headers: {} };
  if (typeof bridge.runner.emitAfterProviderResponse === "function") {
    await bridge.runner.emitAfterProviderResponse(status, {});
  } else if (hasPiProviderHook("after_provider_response")) {
    await bridge.runner.emit(event);
  }
}

function assertProviderStreamRequest(params) {
  const request = asPlainObject(params, "provider stream request");
  const allowed = new Set([
    "stream_id",
    "provider_id",
    "model_id",
    "request",
    "authorization_lease",
  ]);
  for (const key of Object.keys(request)) {
    if (!allowed.has(key)) throw providerError(`provider stream request.${key} is not supported`);
  }
  const streamId = optionalOpaqueName(request.stream_id, "provider stream request.stream_id", 256);
  const providerId = boundedProviderIdentifier(request.provider_id, "provider stream request.provider_id");
  const modelId = boundedProviderIdentifier(request.model_id, "provider stream request.model_id");
  if (!("request" in request)) throw providerError("provider stream request.request is required");
  canonicalJson(request.request);
  // The host contract keeps transport authority outside this opaque request.
  // Reject rather than redact any unexpected authority before a Pi hook or
  // adapter can observe it.
  assertNoUnsafeProviderTransportFields(request.request, "provider stream request.request");
  if (request.authorization_lease !== undefined) {
    boundedOpaqueValue(request.authorization_lease, "provider stream request.authorization_lease");
  }
  return { streamId, providerId, modelId, request: request.request };
}

function asyncIteratorForProvider(value) {
  if (!value || typeof value[Symbol.asyncIterator] !== "function") {
    throw providerError("yggStream must return an AsyncIterable of Pi assistant-message events");
  }
  return value[Symbol.asyncIterator]();
}

function providerIndex(value, label) {
  if (!Number.isSafeInteger(value) || value < 0) throw providerError(`${label} must be a non-negative index`);
  return value;
}

function providerStopReason(value) {
  const reasons = new Map([
    ["stop", "end_turn"],
    ["length", "max_tokens"],
    ["toolUse", "tool_use"],
    ["end_turn", "end_turn"],
    ["max_tokens", "max_tokens"],
    ["tool_use", "tool_use"],
    ["stop_sequence", "stop_sequence"],
    ["refusal", "refusal"],
    ["pause_turn", "pause_turn"],
  ]);
  const reason = reasons.get(value);
  if (!reason) throw providerError(`Pi stream stop reason ${JSON.stringify(value)} is unsupported`);
  return reason;
}

function providerUsage(message) {
  const usage = message?.usage;
  if (usage === undefined || usage === null) return null;
  const source = asPlainObject(usage, "Pi provider usage");
  const values = new Map([
    ["input", "input_tokens"],
    ["output", "output_tokens"],
    ["cacheRead", "cache_read_tokens"],
    ["cacheWrite", "cache_write_tokens"],
    ["totalTokens", "total_tokens"],
  ]);
  const result = {};
  for (const [piName, hostName] of values) {
    const value = source[piName];
    if (value !== undefined && (!Number.isSafeInteger(value) || value < 0)) {
      throw providerError(`Pi provider usage.${piName} must be a non-negative portable integer`);
    }
    result[hostName] = value ?? 0;
  }
  return result;
}

function piToolCall(event) {
  const index = providerIndex(event.contentIndex, "Pi tool-call contentIndex");
  const candidate = event.toolCall ?? event.partial?.content?.[index];
  if (
    !candidate
    || candidate.type !== "toolCall"
    || typeof candidate.id !== "string"
    || !candidate.id
    || typeof candidate.name !== "string"
    || !candidate.name
  ) {
    throw providerError("Pi tool-call stream event must expose an id and name");
  }
  return { index, id: candidate.id, name: candidate.name, arguments: candidate.arguments ?? {} };
}

function providerPayloadRecord(payload, fields, label) {
  const value = asPlainObject(payload, label);
  for (const name of Object.keys(value)) {
    if (!fields.has(name)) throw providerError(`${label}.${name} is unsupported`);
  }
  return value;
}

function providerPayloadIndex(payload, label) {
  const value = providerPayloadRecord(payload, new Set(["index"]), label);
  return { index: providerIndex(value.index, `${label}.index`) };
}

function providerPayloadDelta(payload, label) {
  const value = providerPayloadRecord(payload, new Set(["index", "delta"]), label);
  if (typeof value.delta !== "string") throw providerError(`${label}.delta must be a string`);
  return { index: providerIndex(value.index, `${label}.index`), delta: value.delta };
}

function normalizeHostAdapterEvent(kind, payload) {
  // A custom yggStream adapter may use the API 0.3 event shape directly, but
  // it still cannot make the host decode arbitrary Pi-owned JSON.
  switch (kind) {
    case "started": {
      const value = providerPayloadRecord(payload, new Set(["response_id"]), "provider started payload");
      if (value.response_id !== undefined && value.response_id !== null && typeof value.response_id !== "string") {
        throw providerError("provider started payload.response_id must be a string or null");
      }
      return {
        kind,
        payload:
          value.response_id === undefined || value.response_id === null ? {} : { response_id: value.response_id },
      };
    }
    case "text_start":
    case "text_end":
    case "reasoning_start":
    case "reasoning_end":
    case "tool_call_end":
      return { kind, payload: providerPayloadIndex(payload, `provider ${kind} payload`) };
    case "text_delta":
    case "reasoning_delta":
    case "tool_call_args_delta":
      return { kind, payload: providerPayloadDelta(payload, `provider ${kind} payload`) };
    case "tool_call_start": {
      const value = providerPayloadRecord(payload, new Set(["index", "id", "name"]), "provider tool-call start payload");
      if (typeof value.id !== "string" || !value.id || typeof value.name !== "string" || !value.name) {
        throw providerError("provider tool-call start payload requires non-empty id and name");
      }
      return {
        kind,
        payload: { index: providerIndex(value.index, "provider tool-call start payload.index"), id: value.id, name: value.name },
      };
    }
    case "usage": {
      const value = providerPayloadRecord(
        payload,
        new Set([
          "input_tokens",
          "output_tokens",
          "cache_read_tokens",
          "cache_write_tokens",
          "cache_write_1h_tokens",
          "reasoning_tokens",
          "total_tokens",
        ]),
        "provider usage payload",
      );
      for (const [name, amount] of Object.entries(value)) {
        if (!Number.isSafeInteger(amount) || amount < 0) {
          throw providerError(`provider usage payload.${name} must be a non-negative portable integer`);
        }
      }
      return { kind, payload: value };
    }
    case "finished": {
      const value = providerPayloadRecord(payload, new Set(["stop_reason"]), "provider finished payload");
      if (typeof value.stop_reason !== "string") {
        throw providerError("provider finished payload.stop_reason must be a string");
      }
      return { kind, payload: { stop_reason: providerStopReason(value.stop_reason) } };
    }
    case "heartbeat":
    case "error":
      // The host does not consume either payload. Dropping it avoids treating
      // an adapter's diagnostic object as a transport payload.
      return { kind, payload: {} };
    default:
      throw providerError(`unsupported provider stream event ${kind}`);
  }
}

function normalizePiStreamEvent(event, state) {
  const source = asPlainObject(event, "Pi provider stream event");
  if (typeof source.kind === "string") {
    if (!PROVIDER_STREAM_KINDS.has(source.kind) || !("payload" in source)) {
      throw providerError("host adapter stream event has an unsupported kind or missing payload");
    }
    return [normalizeHostAdapterEvent(source.kind, source.payload)];
  }
  switch (source.type) {
    case "start": {
      const responseId = source.partial?.responseId;
      if (responseId !== undefined && typeof responseId !== "string") {
        throw providerError("Pi provider start responseId must be a string");
      }
      return [{ kind: "started", payload: responseId === undefined ? {} : { response_id: responseId } }];
    }
    case "text_start":
      return [{ kind: "text_start", payload: { index: providerIndex(source.contentIndex, "Pi text contentIndex") } }];
    case "text_delta":
      if (typeof source.delta !== "string") throw providerError("Pi text delta must be a string");
      return [{ kind: "text_delta", payload: { index: providerIndex(source.contentIndex, "Pi text contentIndex"), delta: source.delta } }];
    case "text_end":
      return [{ kind: "text_end", payload: { index: providerIndex(source.contentIndex, "Pi text contentIndex") } }];
    case "thinking_start":
      return [{ kind: "reasoning_start", payload: { index: providerIndex(source.contentIndex, "Pi thinking contentIndex") } }];
    case "thinking_delta":
      if (typeof source.delta !== "string") throw providerError("Pi thinking delta must be a string");
      return [{ kind: "reasoning_delta", payload: { index: providerIndex(source.contentIndex, "Pi thinking contentIndex"), delta: source.delta } }];
    case "thinking_end":
      return [{ kind: "reasoning_end", payload: { index: providerIndex(source.contentIndex, "Pi thinking contentIndex") } }];
    case "toolcall_start": {
      const tool = piToolCall(source);
      state.toolCalls.set(tool.index, tool);
      return [{ kind: "tool_call_start", payload: { index: tool.index, id: tool.id, name: tool.name } }];
    }
    case "toolcall_delta":
      if (typeof source.delta !== "string") throw providerError("Pi tool-call delta must be a string");
      return [{ kind: "tool_call_args_delta", payload: { index: providerIndex(source.contentIndex, "Pi tool-call contentIndex"), delta: source.delta } }];
    case "toolcall_end": {
      const tool = piToolCall(source);
      const output = [];
      if (!state.toolCalls.has(tool.index)) {
        output.push({ kind: "tool_call_start", payload: { index: tool.index, id: tool.id, name: tool.name } });
        output.push({ kind: "tool_call_args_delta", payload: { index: tool.index, delta: canonicalJson(tool.arguments) } });
      }
      state.toolCalls.delete(tool.index);
      output.push({ kind: "tool_call_end", payload: { index: tool.index } });
      return output;
    }
    case "done": {
      const output = [];
      const usage = providerUsage(source.message);
      if (usage) output.push({ kind: "usage", payload: usage });
      output.push({ kind: "finished", payload: { stop_reason: providerStopReason(source.reason) } });
      return output;
    }
    case "error":
      return [{ kind: "error", payload: {} }];
    default:
      throw providerError(`Pi assistant-message stream event ${JSON.stringify(source.type)} is unsupported`);
  }
}

function providerStreamIsTerminal(kind) {
  return kind === "finished" || kind === "error";
}

async function writeProviderStreamEvent(stream, event, allowClosing) {
  if (stream.cancelled || stream.terminal || (stream.closing && !allowClosing)) return false;
  if (stream.sequence >= API_0_3_MAX_PROVIDER_STREAM_EVENTS) {
    stream.exhausted = true;
    stream.terminal = true;
    return false;
  }
  // Reserve the final admissible sequence number for an explicit terminal
  // event. Otherwise the host would receive an over-limit nonterminal stream
  // and be forced to tear down the whole extension protocol.
  const reserveTerminal = stream.sequence === API_0_3_MAX_PROVIDER_STREAM_EVENTS - 1
    && !providerStreamIsTerminal(event.kind);
  const output = reserveTerminal
    ? { kind: "error", payload: {} }
    : event;
  if (reserveTerminal) stream.exhausted = true;
  if (!PROVIDER_STREAM_KINDS.has(output.kind)) throw providerError(`unsupported provider stream event ${output.kind}`);
  const payload = canonicalJson(output.payload);
  if (Buffer.byteLength(payload) > 65_536) {
    throw providerError("provider stream event payload exceeds 65536 bytes");
  }
  const sent = await send({
    jsonrpc: "2.0",
    method: "provider/event",
    params: {
      stream_id: stream.id,
      sequence: stream.sequence,
      kind: output.kind,
      payload: output.payload,
    },
  }, () => !stream.cancelled);
  if (!sent) return false;
  stream.sequence += 1;
  if (providerStreamIsTerminal(output.kind)) stream.terminal = true;
  return true;
}

// Pi stream pumps and declaration mutations run in separate asynchronous
// callbacks. Keep every write for one stream in a FIFO so a mutation terminal
// cannot overtake an already admitted event or race with a later pump write.
function sendProviderStreamEvent(stream, event, { allowClosing = false } = {}) {
  const scheduled = stream.writeChain.then(() =>
    writeProviderStreamEvent(stream, event, allowClosing),
  );
  // A failed write is still reported to its caller, but must not permanently
  // prevent the terminal owner from observing/draining this stream state.
  stream.writeChain = scheduled.catch(() => {});
  return scheduled;
}

async function pumpProviderStream(stream) {
  const state = { toolCalls: new Map() };
  try {
    while (!stream.cancelled && !stream.closing) {
      const next = await stream.iterator.next();
      if (next.done) break;
      for (const candidate of normalizePiStreamEvent(next.value, state)) {
        const event = normalizeHostAdapterEvent(candidate.kind, candidate.payload);
        const sent = await sendProviderStreamEvent(stream, event);
        if (!sent || stream.terminal || stream.cancelled || stream.closing) break;
      }
      if (stream.terminal || stream.closing) break;
    }
    if (!stream.cancelled && !stream.closing && !stream.terminal) {
      await sendProviderStreamEvent(stream, { kind: "error", payload: {} });
    }
  } catch (error) {
    if (!stream.cancelled && !stream.closing) {
      diagnostic(`Pi provider stream failed: ${providerReason(error)}`);
      try {
        await sendProviderStreamEvent(stream, { kind: "error", payload: {} });
      } catch (sendError) {
        diagnostic(`Pi provider stream terminal event failed: ${providerReason(sendError)}`);
      }
    }
  } finally {
    // A declaration mutation owns iterator cancellation. It sends the stream's
    // terminal event first, so the pump must not close the adapter early.
    if (!stream.cancelled && !stream.closing) {
      if (stream.exhausted) stream.controller.abort("provider stream event limit reached");
      try {
        await stream.iterator.return?.();
      } catch (error) {
        diagnostic(`Pi provider stream terminal cleanup failed: ${providerReason(error)}`);
      }
    }
    bridge.providerStreams.delete(stream.id);
  }
}

async function cancelProviderStream(stream) {
  if (!stream || stream.cancelled) return;
  stream.cancelled = true;
  // A host cancellation reason is diagnostic-only; do not make arbitrary host
  // text observable through the Pi-owned AbortSignal.
  stream.controller.abort("provider stream cancelled");
  try {
    await stream.iterator.return?.();
  } catch (error) {
    diagnostic(`Pi provider stream cancellation cleanup failed: ${providerReason(error)}`);
  }
}

function markProviderStreamsForMutation(providerId) {
  const active = [...bridge.providerStreams.values()].filter(
    (stream) => stream.providerId === providerId && !stream.cancelled && !stream.terminal,
  );
  for (const stream of active) stream.closing = true;
  return active;
}

async function terminateProviderStreamForMutation(stream) {
  if (!stream || stream.cancelled) return;
  stream.closing = true;
  if (!stream.terminal) {
    // Preserve the send failure for the catalog mutation owner. The host must
    // never observe a changed declaration after a stream terminal failed.
    try {
      await sendProviderStreamEvent(stream, { kind: "error", payload: {} }, { allowClosing: true });
    } finally {
      await cancelProviderStream(stream);
    }
    return;
  }
  await cancelProviderStream(stream);
}

async function terminateProviderStreamsForMutation(streams) {
  await Promise.all(streams.map((stream) => terminateProviderStreamForMutation(stream)));
}

async function cancelAllProviderStreams() {
  await Promise.all([...bridge.providerStreams.values()].map((stream) => cancelProviderStream(stream)));
}

async function handleProviderStream(message) {
  if (!providerContractAvailable()) throw providerCapabilityError("provider_stream was not negotiated");
  rejectUnsupportedProviderHooks();
  const request = assertProviderStreamRequest(message.params);
  // Host catalog publication and authorization are asynchronous reverse-RPCs.
  // A host may send a stream request in the same read chunk as its authorization
  // response, so wait for the publication chain rather than treating that valid
  // ordering as an unavailable provider.
  await awaitWithCancellation(bridge.providerSyncChain, currentScope().signal);
  if (currentScope().signal.aborted) throw new CancellationError();
  if (bridge.providerSyncError) {
    return { stream_id: request.streamId, accepted: false, reason: publicProviderReason() };
  }
  const entry = bridge.providers.get(request.providerId);
  if (
    !entry
    || !entry.published
    || entry.authorization_status !== "ready"
    || !entry.models.some((model) => model.id === request.modelId)
  ) {
    return { stream_id: request.streamId, accepted: false, reason: "provider or model is unavailable" };
  }
  if (typeof entry.adapter !== "function") {
    return {
      stream_id: request.streamId,
      accepted: false,
      reason: "provider has no secret-free host stream adapter",
    };
  }
  if (bridge.providerStreams.has(request.streamId)) {
    return { stream_id: request.streamId, accepted: false, reason: "stream identifier is already active" };
  }
  const controller = new AbortController();
  const parentSignal = currentScope().signal;
  const abortForParent = () => controller.abort("provider request cancelled");
  parentSignal.addEventListener("abort", abortForParent, { once: true });
  if (parentSignal.aborted) abortForParent();
  let iterator;
  try {
    if (parentSignal.aborted) throw new CancellationError();
    const transformedRequest = await awaitWithCancellation(
      emitBeforeProviderRequest(request.request),
      parentSignal,
    );
    // The adapter receives only secret-free declarations and the canonical
    // request. Host leases, endpoints, headers, and credential material do not
    // cross this compatibility boundary.
    const adapterInput = canonicalClone({
      provider: entry.provider,
      model: entry.models.find((model) => model.id === request.modelId),
      request: transformedRequest,
    });
    const source = await awaitWithCancellation(
      entry.adapter({
        ...adapterInput,
        signal: controller.signal,
      }),
      parentSignal,
    );
    iterator = asyncIteratorForProvider(source);
    // Pi defines this hook at response acquisition, before an assistant stream
    // is consumed. The host deliberately supplies no response headers.
    await awaitWithCancellation(emitAfterProviderResponse(200), parentSignal);
    if (parentSignal.aborted) throw new CancellationError();
  } catch (error) {
    parentSignal.removeEventListener("abort", abortForParent);
    controller.abort();
    if (error instanceof CancellationError || parentSignal.aborted) throw new CancellationError();
    if (error?.code === API_0_3_ERROR.invalid_params.code) throw error;
    diagnostic(`Pi provider stream setup failed: ${providerReason(error)}`);
    return { stream_id: request.streamId, accepted: false, reason: publicProviderReason() };
  }
  parentSignal.removeEventListener("abort", abortForParent);
  const stream = {
    id: request.streamId,
    providerId: request.providerId,
    controller,
    iterator,
    sequence: 0,
    terminal: false,
    exhausted: false,
    cancelled: false,
    closing: false,
    writeChain: Promise.resolve(),
  };
  bridge.providerStreams.set(stream.id, stream);
  setImmediate(() => void pumpProviderStream(stream));
  return { stream_id: request.streamId, accepted: true };
}

async function handleProviderCancel(params) {
  if (!providerContractAvailable()) throw providerCapabilityError("provider_cancel was not negotiated");
  const value = asPlainObject(params, "provider stream cancellation");
  for (const name of Object.keys(value)) {
    if (name !== "stream_id" && name !== "reason") {
      throw providerError(`provider stream cancellation.${name} is not supported`);
    }
  }
  const streamId = optionalOpaqueName(value.stream_id, "provider stream cancellation.stream_id", 256);
  if (
    value.reason !== undefined
    && (typeof value.reason !== "string" || Buffer.byteLength(value.reason) > 4096)
  ) {
    throw providerError("provider stream cancellation.reason must be a bounded string");
  }
  await cancelProviderStream(bridge.providerStreams.get(streamId));
}

function assertV03ToolCallParams(params) {
  const value = asPlainObject(params, "tool call params");
  for (const name of Object.keys(value)) {
    if (name !== "name" && name !== "arguments" && name !== "context") {
      throw v03ProtocolError("invalid_params", `tool call params.${name} is not supported`);
    }
  }
  if (typeof value.name !== "string" || !("arguments" in value) || !("context" in value)) {
    throw v03ProtocolError("invalid_params", "tool call params require name, arguments, and context");
  }
  canonicalJson(value.arguments);
  canonicalJson(value.context);
}

function assertV03PiToolDispatch(value) {
  const dispatch = assertV03ExactFields(
    value,
    new Set(["tool_name", "arguments"]),
    "Pi tool dispatcher arguments",
  );
  if (
    typeof dispatch.tool_name !== "string"
    || !dispatch.tool_name
    || Buffer.byteLength(dispatch.tool_name) > 128
    || !("arguments" in dispatch)
  ) {
    throw v03ProtocolError(
      "invalid_params",
      "Pi tool dispatcher arguments require a bounded tool_name and arguments",
    );
  }
  canonicalJson(dispatch.arguments);
  return dispatch;
}

async function cleanupProviderCatalog() {
  if (!isApiV03() || !bridge?.initialized) return;
  await cancelAllProviderStreams();
  await bridge.providerSyncChain;
  const entries = new Map();
  for (const [providerId, entry] of bridge.providers) {
    if (entry.published || entry.hostPublished) entries.set(providerId, entry);
  }
  for (const entry of bridge.retiredProviders) {
    if ((entry.published || entry.hostPublished) && !entries.has(entry.provider.id)) {
      entries.set(entry.provider.id, entry);
    }
  }
  for (const [providerId, entry] of entries) {
    try {
      await publishProviderUnregister(providerId, entry);
    } catch (error) {
      diagnostic(`Pi provider shutdown cleanup failed: ${providerReason(error)}`);
    }
  }
  bridge.providers.clear();
  bridge.retiredProviders.clear();
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
  for (const entry of entries) {
    hash.update(Buffer.from(entry.tag));
    const relative = Buffer.from(entry.relative);
    hash.update(unsignedBigEndian(relative.length, 8));
    hash.update(relative);
    if (entry.tag === "f") {
      total += hashSourceFile(hash, entry.path, MAX_SOURCE_BYTES - total);
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
  return hash.digest("hex");
}

function hashFramed(hash, value) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(String(value));
  hash.update(unsignedBigEndian(bytes.length, 8));
  hash.update(bytes);
}

function sourceLabel(index) {
  return `#${index + 1}`;
}

function sourceVerificationError(index, reason) {
  return new Error(`Pi source ${sourceLabel(index)} ${reason}; review it and publish a replacement link`);
}

function sourceLockEntries(root) {
  const entries = [];
  for (const name of SUPPORTED_LOCK_FILES) {
    const path = join(root, name);
    try {
      const metadata = lstatSync(path);
      if (metadata.isSymbolicLink() || !metadata.isFile()) {
        throw new Error("dependency lock is not a regular non-symlink file");
      }
      if (metadata.size > MAX_LOCK_FILE_BYTES) {
        throw new Error("dependency lock exceeds the supported size limit");
      }
      entries.push({ name, path });
    } catch (error) {
      if (error?.code === "ENOENT") continue;
      throw error;
    }
  }
  return entries;
}

function fingerprintSourceLocks(source) {
  const absolute = resolve(source);
  const metadata = lstatSync(absolute);
  if (metadata.isSymbolicLink() || (!metadata.isFile() && !metadata.isDirectory())) {
    throw new Error("Pi source lock fingerprint requires a regular source");
  }
  if (realpathSync(absolute) !== absolute) {
    throw new Error("Pi source lock fingerprint requires a canonical source path");
  }
  const root = metadata.isDirectory() ? absolute : dirname(absolute);
  const before = sourceLockEntries(root);
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-source-lock-fingerprint\0"));
  hash.update(unsignedBigEndian(SOURCE_LOCK_FINGERPRINT_FORMAT, 4));
  hash.update(unsignedBigEndian(before.length, 4));
  let total = 0;
  for (const entry of before) {
    hashFramed(hash, entry.name);
    total += hashSourceFile(hash, entry.path, MAX_LOCK_BYTES - total);
  }
  const after = sourceLockEntries(root);
  if (before.length !== after.length || before.some((entry, index) => entry.name !== after[index].name)) {
    throw new Error("Pi source dependency lock set changed while it was being fingerprinted");
  }
  return hash.digest("hex");
}

function piRuntimeIntegrity(root) {
  const manifest = Buffer.from(readRegularUtf8Bounded(join(root, "package.json"), MAX_PI_PACKAGE_MANIFEST_BYTES));
  const distribution = fingerprintSource(join(root, "dist"));
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-runtime-integrity\0"));
  hash.update(unsignedBigEndian(PI_RUNTIME_INTEGRITY_FORMAT, 4));
  hashFramed(hash, manifest);
  hashFramed(hash, distribution);
  return hash.digest("hex");
}

function canonicalManifestPath(path) {
  if (typeof path !== "string" || !isAbsolute(path)) {
    throw new Error("Pi link manifest path must be absolute");
  }
  const canonical = realpathSync(path);
  const metadata = lstatSync(canonical);
  if (metadata.isSymbolicLink() || !metadata.isFile()) {
    throw new Error("Pi link manifest path must resolve to a regular file");
  }
  return canonical;
}

function calculateLinkIdentity(piRuntime) {
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-aggregate-link-identity\0"));
  hash.update(unsignedBigEndian(LINK_IDENTITY_FORMAT, 4));
  for (const value of [
    BRIDGE_VERSION,
    SUPPORTED_PI_VERSION,
    args.yggVersion,
    args.commandName,
    bridge.linkManifest,
    piRuntime.root,
    args.piRuntimeIntegrity,
    args.aggregateDigest,
    EXPLICIT_TRUST_MODE,
    bridge.agentDir,
  ]) {
    hashFramed(hash, value);
  }
  hash.update(unsignedBigEndian(args.extensions.length, 4));
  for (let index = 0; index < args.extensions.length; index += 1) {
    hashFramed(hash, resolve(args.extensions[index]));
    hashFramed(hash, args.sourceFingerprints[index]);
    hashFramed(hash, args.sourceLockFingerprints[index]);
  }
  return hash.digest("hex");
}

function verifySourceFingerprints() {
  if (!args.sourceFingerprints.length) return;
  for (let index = 0; index < args.extensions.length; index += 1) {
    let actual;
    try {
      actual = fingerprintSource(args.extensions[index]);
    } catch {
      throw sourceVerificationError(index, "cannot be verified");
    }
    if (actual !== args.sourceFingerprints[index]) {
      throw sourceVerificationError(index, "changed after link publication");
    }
    if (!args.sourceLockFingerprints.length) continue;
    let lock;
    try {
      lock = fingerprintSourceLocks(args.extensions[index]);
    } catch {
      throw sourceVerificationError(index, "dependency lock cannot be verified");
    }
    if (lock !== args.sourceLockFingerprints[index]) {
      throw sourceVerificationError(index, "dependency lock changed after link publication");
    }
  }
}

function verifyRuntimeIdentity(piRuntime, params) {
  if (!args.strictIdentity) return;
  if (piRuntime.integrity !== args.piRuntimeIntegrity) {
    throw new Error("Pinned Pi runtime integrity changed; review the selected package and publish a replacement link");
  }
  const extension = params.extension;
  if (
    !extension
    || extension.name !== args.commandName
    || params.ygg_version !== args.yggVersion
  ) {
    throw new Error("Pi link identity does not match the selected trusted manifest; review trust/enablement and publish a replacement link");
  }
  let selectedManifest;
  try {
    selectedManifest = canonicalManifestPath(extension.manifest_path);
  } catch {
    throw new Error("Pi link identity does not match the selected trusted manifest; review trust/enablement and publish a replacement link");
  }
  if (selectedManifest !== bridge.linkManifest) {
    throw new Error("Pi link identity does not match the selected trusted manifest; review trust/enablement and publish a replacement link");
  }
  if (calculateLinkIdentity(piRuntime) !== args.linkIdentity) {
    throw new Error("Pi link identity changed; review trust/enablement and publish a replacement link");
  }
}

function readRegularUtf8Bounded(path, maxBytes) {
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
    return Buffer.concat(chunks, total).toString("utf8");
  } finally {
    if (file !== undefined) closeSync(file);
  }
}

function inspectPiPackage(candidate) {
  let root;
  try {
    root = realpathSync(resolve(candidate));
  } catch {
    return null;
  }
  const manifestPath = join(root, "package.json");
  if (!existsSync(manifestPath)) return null;
  let manifest;
  try {
    manifest = JSON.parse(readRegularUtf8Bounded(manifestPath, MAX_PI_PACKAGE_MANIFEST_BYTES));
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
  let integrity;
  try {
    integrity = piRuntimeIntegrity(root);
  } catch (error) {
    return { root, error: `cannot verify runtime integrity: ${error instanceof Error ? error.message : String(error)}` };
  }
  return { root, version: manifest.version, integrity };
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

function apiV03PiToolDispatcher() {
  // API 0.3's initial tool catalog is fixed and must exactly match the
  // install-time manifest. Pi discovers tool names only after loading trusted
  // source, so expose one manifest-declared dispatcher rather than publish an
  // undeclared dynamic catalog.
  return {
    name: bridge.commandName,
    description: "Invoke a registered Pi compatibility tool by name.",
    parameters: {
      type: "object",
      properties: {
        tool_name: { type: "string", maxLength: 128 },
        arguments: {},
      },
      required: ["tool_name", "arguments"],
      additionalProperties: false,
    },
  };
}

function setCurrentTools(tools, revision) {
  bridge.tools = tools;
  bridge.catalogRevision = revision;
  bridge.toolNames = tools.map((tool) => tool.definition.name);
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

function sameToolDefinition(left, right) {
  return JSON.stringify(toolDefinitionToYgg(left.definition)) === JSON.stringify(toolDefinitionToYgg(right.definition));
}

function scheduleToolRefresh() {
  if (!bridge) return;
  if (!bridge.initialized) {
    bridge.toolRefreshRequested = true;
    return;
  }
  if (!bridge.features?.has("dynamic_tools")) return;
  bridge.toolRefreshChain = bridge.toolRefreshChain
    .then(() => refreshPublishedTools())
    .catch((error) => diagnostic(`Pi dynamic tool publication failed: ${error instanceof Error ? error.message : String(error)}`));
}

async function refreshPublishedTools() {
  const runtimeTools = bridge.runner.getAllRegisteredTools();
  const runtimeByName = new Map(runtimeTools.map((tool) => [tool.definition.name, tool]));
  const currentByName = new Map(bridge.tools.map((tool) => [tool.definition.name, tool]));
  const removed = [...currentByName.keys()].filter((name) => !runtimeByName.has(name));
  if (removed.length) {
    const response = await requestHost("tools/unregister", { names: removed });
    const accepted = new Set(response.tools ?? []);
    setCurrentTools(
      bridge.tools.filter((tool) => accepted.has(tool.definition.name)),
      response.revision,
    );
  }

  const publishedByName = new Map(bridge.tools.map((tool) => [tool.definition.name, tool]));
  const changed = runtimeTools.filter((tool) => {
    const published = publishedByName.get(tool.definition.name);
    return !published || published !== tool || !sameToolDefinition(published, tool);
  });
  if (changed.length) {
    const response = await requestHost("tools/register", {
      tools: changed.map((tool) => toolDefinitionToYgg(tool.definition)),
    });
    const accepted = new Set(response.tools ?? []);
    setCurrentTools(
      runtimeTools.filter((tool) => accepted.has(tool.definition.name)),
      response.revision,
    );
  }
}

function textFromContent(content) {
  return (content ?? [])
    .filter((part) => part?.type === "text")
    .map((part) => String(part.text ?? ""))
    .join("\n");
}

function appendLoweredContent(lowered, part) {
  if (isApiV03() && lowered.length >= API_0_3_MAX_CONTENT_PARTS) {
    throw v03ProtocolError("resource_exhausted", "Pi tool result exceeds API 0.3 max_content_parts");
  }
  lowered.push(part);
}

async function lowerContent(content) {
  const lowered = [];
  for (const part of content ?? []) {
    if (!part || typeof part !== "object") continue;
    if (part.type === "text") {
      appendLoweredContent(lowered, { type: "text", text: String(part.text ?? "") });
      continue;
    }
    if (part.type === "image" && typeof part.data === "string") {
      if (!bridge.features.has("artifacts")) {
        appendLoweredContent(lowered, {
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
      appendLoweredContent(lowered, {
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
    if (isApiV03() && lowered.length >= API_0_3_MAX_CONTENT_PARTS) {
      throw v03ProtocolError("resource_exhausted", "Pi tool result exceeds API 0.3 max_content_parts");
    }
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
  const messages = [];
  if (result?.message) messages.push(result.message);
  if (Array.isArray(result?.messages)) messages.push(...result.messages);
  for (const message of messages) {
    const contribution = contextContribution(
      "pi-before-agent-start",
      messageContent(message),
      "prompt_suffix",
    );
    if (contribution) context.push(contribution);
  }
  return context;
}

function v03ProtocolError(name, detail) {
  const specification = API_0_3_ERROR[name] ?? API_0_3_ERROR.internal_error;
  const error = new Error(detail ?? specification.message);
  error.v03Error = name in API_0_3_ERROR ? name : "internal_error";
  error.code = specification.code;
  return error;
}

function v03StringSet(value, label, maximumItems, maximumBytes) {
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw v03ProtocolError("invalid_params", `${label} must be an array of strings`);
  }
  if (value.length > maximumItems || value.some((item) => !item || Buffer.byteLength(item) > maximumBytes)) {
    throw v03ProtocolError("resource_exhausted", `${label} exceeds API 0.3 bounds`);
  }
  if (new Set(value).size !== value.length) {
    throw v03ProtocolError("capability_mismatch", `${label} contains duplicates`);
  }
  return new Set(value);
}

function assertV03SetSubset(values, supported, label) {
  for (const value of values) {
    if (!supported.includes(value)) {
      throw v03ProtocolError("capability_mismatch", `${label} contains unsupported ${JSON.stringify(value)}`);
    }
  }
}

function assertV03SetContains(values, expected, label) {
  for (const value of expected) {
    if (!values.has(value)) {
      throw v03ProtocolError("capability_mismatch", `${label} ${value} is absent`);
    }
  }
}

function assertV03AllOrNothing(values, expected, label) {
  const offered = expected.filter((value) => values.has(value));
  if (offered.length === 0) return false;
  if (offered.length !== expected.length) {
    throw v03ProtocolError("capability_mismatch", `${label} must be absent or complete`);
  }
  return true;
}

function assertV03PlainObject(value, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw v03ProtocolError("invalid_params", `${label} must be an object`);
  }
  return value;
}

function assertV03ExactFields(value, fields, label) {
  const object = assertV03PlainObject(value, label);
  for (const name of Object.keys(object)) {
    if (!fields.has(name)) {
      throw v03ProtocolError("invalid_params", `${label}.${name} is not supported`);
    }
  }
  return object;
}

function assertV03InitializeParams(params) {
  const value = assertV03ExactFields(
    params,
    new Set([
      "api_version",
      "ygg_version",
      "extension",
      "workspace",
      "capabilities",
      "contributes",
      "flag_values",
      "host",
      "contract",
    ]),
    "initialize params",
  );
  for (const name of ["api_version", "ygg_version", "workspace"]) {
    if (typeof value[name] !== "string") {
      throw v03ProtocolError("invalid_params", `initialize params.${name} must be a string`);
    }
  }
  for (const name of ["extension", "capabilities", "contributes", "host", "contract"]) {
    if (!(name in value) || value[name] === null) {
      throw v03ProtocolError("invalid_params", `initialize params.${name} is required`);
    }
    canonicalJson(value[name]);
  }
  if (!Array.isArray(value.flag_values) || value.flag_values.length > API_0_3_MAX_EXTENSION_FLAGS) {
    throw v03ProtocolError("invalid_params", "initialize params.flag_values must be a bounded array");
  }
  const flags = new Set();
  for (const [index, flag] of value.flag_values.entries()) {
    const entry = assertV03ExactFields(flag, new Set(["name", "value"]), `initialize params.flag_values[${index}]`);
    if (typeof entry.name !== "string" || !entry.name || flags.has(entry.name)) {
      throw v03ProtocolError("invalid_params", `initialize params.flag_values[${index}].name is invalid`);
    }
    if (!("value" in entry)) {
      throw v03ProtocolError("invalid_params", `initialize params.flag_values[${index}].value is required`);
    }
    flags.add(entry.name);
    canonicalJson(entry.value);
  }
  return value;
}

function assertV03CancellationParams(params) {
  const value = assertV03ExactFields(params, new Set(["id", "reason"]), "cancellation params");
  if (!validV03RpcId(value.id)) {
    throw v03ProtocolError("invalid_params", "cancellation params.id is invalid");
  }
  if (value.reason !== undefined && (typeof value.reason !== "string" || Buffer.byteLength(value.reason) > 4096)) {
    throw v03ProtocolError("invalid_params", "cancellation params.reason is invalid");
  }
  return value;
}

function assertV03ShutdownParams(params) {
  assertV03ExactFields(params, new Set(), "shutdown params");
}

function selectV03Contract(params) {
  const initialize = assertV03InitializeParams(params);
  if (initialize.api_version !== API_VERSION_0_3) {
    throw v03ProtocolError("version_mismatch", "initialize did not select API 0.3");
  }
  const offer = assertV03ExactFields(
    initialize.contract,
    new Set([
      "schema",
      "encoding",
      "required_capabilities",
      "optional_capabilities",
      "required_methods",
      "optional_methods",
      "limits",
    ]),
    "initialize contract",
  );
  if (offer.schema !== API_0_3_SCHEMA || offer.encoding !== API_0_3_ENCODING) {
    throw v03ProtocolError("version_mismatch", "initialize contract schema or encoding is incompatible");
  }
  const requiredCapabilities = v03StringSet(offer.required_capabilities, "required_capabilities", 32, 64);
  const optionalCapabilities = v03StringSet(offer.optional_capabilities, "optional_capabilities", 32, 64);
  const requiredMethods = v03StringSet(offer.required_methods, "required_methods", 64, 128);
  const optionalMethods = v03StringSet(offer.optional_methods, "optional_methods", 64, 128);
  assertV03SetSubset(requiredCapabilities, API_0_3_REQUIRED_CAPABILITIES, "required_capabilities");
  assertV03SetSubset(optionalCapabilities, API_0_3_OPTIONAL_CAPABILITIES, "optional_capabilities");
  assertV03SetSubset(requiredMethods, API_0_3_REQUIRED_METHODS, "required_methods");
  assertV03SetSubset(optionalMethods, API_0_3_OPTIONAL_METHODS, "optional_methods");
  for (const capability of requiredCapabilities) {
    if (optionalCapabilities.has(capability)) {
      throw v03ProtocolError("capability_mismatch", `${capability} is both required and optional`);
    }
  }
  for (const method of requiredMethods) {
    if (optionalMethods.has(method)) {
      throw v03ProtocolError("capability_mismatch", `${method} is both required and optional`);
    }
  }
  assertV03SetContains(requiredCapabilities, API_0_3_REQUIRED_CAPABILITIES, "required capability");
  assertV03SetContains(requiredMethods, API_0_3_REQUIRED_METHODS, "required method");
  for (const method of optionalMethods) {
    const capability = API_0_3_OPTIONAL_METHOD_CAPABILITIES.get(method);
    if (!optionalCapabilities.has(capability)) {
      throw v03ProtocolError(
        "capability_mismatch",
        `optional method ${JSON.stringify(method)} lacks offered capability ${JSON.stringify(capability)}`,
      );
    }
  }
  const limits = assertV03ExactFields(
    offer.limits,
    new Set(["max_frame_bytes", "max_concurrent_requests", "max_tools"]),
    "initialize contract limits",
  );
  for (const name of ["max_frame_bytes", "max_concurrent_requests", "max_tools"]) {
    if (!Number.isSafeInteger(limits[name]) || limits[name] <= 0) {
      throw v03ProtocolError("invalid_params", `initialize contract limit ${name} is invalid`);
    }
  }
  if (
    limits.max_frame_bytes > API_0_3_MAX_FRAME_BYTES
    || limits.max_concurrent_requests > API_0_3_MAX_CONCURRENT_REQUESTS
    || limits.max_tools > API_0_3_MAX_TOOLS
  ) {
    throw v03ProtocolError("resource_exhausted", "host offered an API 0.3 limit above the contract maximum");
  }
  const providerCapabilitiesOffered = assertV03AllOrNothing(
    optionalCapabilities,
    API_0_3_PROVIDER_CAPABILITIES,
    "optional provider capabilities",
  );
  const providerMethodsOffered = assertV03AllOrNothing(
    optionalMethods,
    API_0_3_PROVIDER_METHODS,
    "optional provider methods",
  );
  if (providerCapabilitiesOffered !== providerMethodsOffered) {
    throw v03ProtocolError("capability_mismatch", "provider capabilities and methods must be selected together");
  }
  const providerContract = providerCapabilitiesOffered && providerMethodsOffered;
  const capabilities = [...API_0_3_REQUIRED_CAPABILITIES];
  const methods = [...API_0_3_REQUIRED_METHODS];
  if (providerContract) {
    capabilities.push(...API_0_3_PROVIDER_CAPABILITIES);
    methods.push(...API_0_3_PROVIDER_METHODS);
  }
  return {
    providerContract,
    frameLimit: limits.max_frame_bytes,
    contract: {
      schema: API_0_3_SCHEMA,
      encoding: API_0_3_ENCODING,
      capabilities: capabilities.sort(compareUnicodeCodePoints),
      methods: methods.sort(compareUnicodeCodePoints),
      limits: {
        max_frame_bytes: limits.max_frame_bytes,
        max_concurrent_requests: Math.min(limits.max_concurrent_requests, 4),
        max_tools: Math.min(limits.max_tools, API_0_3_MAX_TOOLS),
      },
    },
  };
}

async function loadBridge(params, v03Selection = undefined) {
  validateNodeRuntime();
  const linkManifest = args.strictIdentity ? canonicalManifestPath(args.linkManifest) : null;
  verifySourceFingerprints();
  bridge = {
    cwd: resolve(params.workspace ?? args.cwd ?? process.cwd()),
    agentDir: resolve(args.agentDir ?? process.env.YGG_PI_AGENT_DIR ?? join(homedir(), ".pi/agent")),
    extensionPaths: args.extensions.map((path) => resolve(path)),
    linkManifest,
    commandName: args.commandName,
    hostState: params.host && typeof params.host === "object" ? { ...params.host } : {},
    agentActive: false,
    toolNames: [],
    toolInfos: [],
    toolSnapshots: new Map(),
    toolRefreshChain: Promise.resolve(),
    toolRefreshRequested: false,
    catalogRevision: 0,
    features: new Set(REQUIRED_FEATURES),
    terminal: {
      pendingCompletedTurn: null,
      pendingAgentMessages: null,
      sessionShutdown: false,
    },
    pendingPiToolCalls: new Map(),
    startedPiToolCalls: new Map(),
    hostToolCallIds: new Map(),
    initialized: false,
    apiVersion: args.apiVersion,
    // The API 0.3 selection becomes active only after initialization succeeds.
    // While loading fails, subsequent input remains subject to the offered
    // bootstrap bound rather than a partially negotiated one.
    frameLimit: API_0_3_MAX_FRAME_BYTES,
    v03Contract: v03Selection?.contract,
    providerContract: v03Selection?.providerContract === true,
    providers: new Map(),
    retiredProviders: new Set(),
    providerStreams: new Map(),
    providerSyncChain: Promise.resolve(),
    providerSyncError: null,
    providerRegistrationFailure: null,
    providerCatalogRevision: 0,
    loadedExtensions: [],
  };

  if (!isApiV03()) {
    const offeredOptionalFeatures = new Set(params.protocol?.optional_features ?? []);
    for (const feature of OPTIONAL_FEATURES) {
      if (offeredOptionalFeatures.has(feature)) bridge.features.add(feature);
    }
  }

  const piRuntime = findPiPackageRoot(bridge.extensionPaths, args.piPackage);
  verifyRuntimeIdentity(piRuntime, params);
  verifySourceFingerprints();
  bridge.piRuntimeVersion = piRuntime.version;
  bridge.piRuntimeIntegrity = piRuntime.integrity;
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
  if (args.strictIdentity && piRuntimeIntegrity(piRuntime.root) !== args.piRuntimeIntegrity) {
    throw new Error("Pinned Pi runtime integrity changed during startup; review the package and publish a replacement link");
  }
  const loadErrors = loaded.errors ?? [];
  for (const error of loadErrors) {
    const index = bridge.extensionPaths.findIndex((path) => path === resolve(error?.path ?? ""));
    diagnostic(`Pi source ${sourceLabel(index >= 0 ? index : 0)} failed to load; review it and publish a replacement link`);
  }
  if (loadErrors.length || loaded.extensions?.length !== bridge.extensionPaths.length) {
    throw new Error("Pi aggregate loader did not load every pinned source; review the sources and publish a replacement link");
  }

  bridge.loadedExtensions = loaded.extensions;
  bridge.runner = new pi.ExtensionRunner(
    loaded.extensions,
    loaded.runtime,
    bridge.cwd,
    makeSessionManager(),
    makeModelRegistry(),
  );
  bridge.runner.onError((error) => {
    const index = bridge.extensionPaths.findIndex((path) => path === resolve(error?.extensionPath ?? ""));
    const event = error?.event ? ` during ${String(error.event).replace(/[^A-Za-z0-9_/-]/g, "?")}` : "";
    boundedDiagnostic(`Pi source ${sourceLabel(index >= 0 ? index : 0)} handler failed${event}; review the source and publish a replacement link`);
  });
  if (isApiV03()) rejectUnsupportedProviderHooks();
  bridge.runner.bindCore(makeExtensionActions(), makeExtensionContextActions(), {
    registerProvider: isApiV03()
      ? (name, config) => registerPiProvider(name, config)
      : () => unsupported("pi.registerProvider"),
    // Native Pi registrations can carry an executable transport. The bridge
    // only accepts the secret-free declarative registerProvider shape.
    registerNativeProvider: () => unsupported("pi.registerNativeProvider"),
    unregisterProvider: isApiV03()
      ? (name) => unregisterPiProvider(name)
      : () => unsupported("pi.unregisterProvider"),
  });
  if (bridge.providerRegistrationFailure) throw bridge.providerRegistrationFailure;
  bridge.runner.bindCommandContext({
    waitForIdle: async () => {},
    newSession: async () => unsupported("ctx.newSession"),
    fork: async () => unsupported("ctx.fork"),
    navigateTree: async () => unsupported("ctx.navigateTree"),
    switchSession: async () => unsupported("ctx.switchSession"),
    reload: async () => unsupported("ctx.reload"),
  });
  bridge.runner.setUIContext(makeUi(), "rpc");

  const initialTools = bridge.runner.getAllRegisteredTools();
  setCurrentTools(initialTools, 0);
  bridge.commands = bridge.runner.getRegisteredCommands();
  bridge.unsupported = [];
  for (const [index, extension] of loaded.extensions.entries()) {
    const label = sourceLabel(index);
    const registeredEvents = extension.handlers ?? extension.eventHandlers;
    if (registeredEvents instanceof Map) {
      for (const event of registeredEvents.keys()) {
        if (!bridgedPiEvent(event)) bridge.unsupported.push(`${label}: event ${event}`);
      }
    }
    if (extension.shortcuts?.size) bridge.unsupported.push(`${label}: shortcuts`);
    // Pi exposes flags only after loading extension code. Do not execute an
    // untrusted source merely to synthesize Ygg's pre-start API 0.3 manifest flags.
    if (extension.flags?.size) bridge.unsupported.push(`${label}: flags`);
    if (extension.messageRenderers?.size) bridge.unsupported.push(`${label}: message renderers`);
    if (extension.entryRenderers?.size) bridge.unsupported.push(`${label}: entry renderers`);
    if (extension.markdownTransformer) bridge.unsupported.push(`${label}: markdown transformer`);
  }
}

async function handleInitialize(message) {
  if (isApiV03() && bridge?.initialized) {
    throw v03ProtocolError("invalid_request", "API 0.3 initialize may only be requested once");
  }
  const params = message.params ?? {};
  const v03Selection = isApiV03() ? selectV03Contract(params) : undefined;
  await loadBridge(params, v03Selection);
  // This bridge selects the offered frame limit exactly, so the bootstrap and
  // selected limits agree while the initialize response is serialized.
  if (v03Selection) bridge.frameLimit = v03Selection.frameLimit;
  const tools = isApiV03()
    ? [apiV03PiToolDispatcher()]
    : bridge.tools.map((tool) => toolDefinitionToYgg(tool.definition));
  if (isApiV03()) {
    if (tools.length > bridge.v03Contract.limits.max_tools) {
      throw v03ProtocolError("resource_exhausted", "Pi tool catalog exceeds negotiated max_tools");
    }
    // Keep the offered bound through the initialize response. The negotiated
    // bound becomes active only after that frame is written successfully.
    bridge.initialized = true;
    diagnostic(`Pi compatibility profile ${bridge.piRuntimeVersion} initialized with API 0.3`);
    return {
      api_version: API_VERSION_0_3,
      tools,
      contract: bridge.v03Contract,
    };
  }
  bridge.initialized = true;
  if (bridge.toolRefreshRequested) {
    bridge.toolRefreshRequested = false;
    setImmediate(() => scheduleToolRefresh());
  }
  const commands = bridge.features.has("runtime_commands")
    ? bridge.commands.map(commandDefinitionToYgg)
    : [
        {
          name: bridge.commandName,
          description: `Run bridged Pi command(s): /${bridge.commandName} COMMAND [arguments]`,
          usage: `/${bridge.commandName} COMMAND [arguments]`,
        },
      ];
  for (const warning of bridge.unsupported) diagnostic(`Pi compatibility: ${warning} is unavailable in Ygg`);
  diagnostic(
    `Pi compatibility readiness profile=pi_aggregate bridge_api=${API_VERSION_0_2} evidence_api=0.3 sources=${bridge.extensionPaths.length} pinned=${args.strictIdentity ? "yes" : "legacy"}`,
  );
  diagnostic(`Pi compatibility profile ${bridge.piRuntimeVersion} initialized`);
  return {
    api_version: API_VERSION_0_2,
    tools,
    commands,
    protocol: {
      version: API_VERSION_0_2,
      features: [...bridge.features],
      limits: { max_concurrent_requests: 4 },
      ...(bridge.features.has("lifecycle_events") ? { lifecycle_events: LIFECYCLE_EVENTS } : {}),
    },
  };
}

async function runScoped(id, operation) {
  const controller = new AbortController();
  const entry = { controller };
  inflight.set(keyOf(id), entry);
  try {
    const result = await scopes.run(
      {
        parentRequestId: typeof id === "number" ? id : Number(id),
        controller,
        signal: controller.signal,
        progressSequence: 0,
      },
      operation,
    );
    if (controller.signal.aborted) throw new CancellationError();
    return result;
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
  // A Pi command may have requested a catalog refresh immediately before the
  // host invokes the new tool revision. Wait for that transactional publish
  // rather than racing a valid revision against its acknowledgement.
  await bridge.toolRefreshChain;
  let name = message.params?.name;
  let revision = message.params?.catalog_revision ?? bridge.catalogRevision;
  let requestedInput = message.params?.arguments ?? {};
  if (isApiV03()) {
    if (name !== bridge.commandName) {
      throw v03ProtocolError(
        "invalid_params",
        `unknown API 0.3 Pi tool dispatcher ${JSON.stringify(name)}`,
      );
    }
    const dispatch = assertV03PiToolDispatch(requestedInput);
    name = dispatch.tool_name;
    requestedInput = dispatch.arguments;
    revision = bridge.catalogRevision;
  }
  const tools = bridge.toolSnapshots.get(revision);
  if (!tools) throw new Error(`unknown or retired Pi tool catalog revision ${revision}`);
  const registered = tools.find((tool) => tool.definition.name === name);
  if (!registered) throw new Error(`unknown bridged Pi tool ${name}`);
  const definition = registered.definition;
  const started = dequeueByName(bridge.startedPiToolCalls, name);
  let input = started?.input ?? requestedInput;
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
  return {
    content: finalContent,
    is_error: transformed.isError === true,
    ...(isApiV03() ? { metadata: metadata ?? null } : metadata === undefined ? {} : { metadata }),
  };
}

async function executePiCommand(message) {
  let command;
  let argumentsList;
  if (bridge.features.has("runtime_commands")) {
    command = message.params?.name;
    argumentsList = message.params?.arguments ?? [];
  } else {
    if (message.params?.name !== bridge.commandName) {
      throw new Error(`unknown bridged Pi command ${message.params?.name}`);
    }
    command = message.params?.arguments?.[0];
    if (!command) throw new Error(`/${bridge.commandName} requires the bridged Pi command name`);
    argumentsList = message.params.arguments.slice(1) ?? [];
  }
  const registered = bridge.commands.find(
    (candidate) => (candidate.invocationName ?? candidate.name) === command,
  );
  if (!registered) throw new Error(`unknown bridged Pi command ${command}`);
  await registered.handler(argumentsList.join(" "), bridge.runner.createCommandContext());
  return {
    text: `Pi command ${command} completed.`,
    notifications: [],
    context: [],
  };
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
    return {
      disposition: result?.block
        ? { action: "deny", reason: result.reason ?? "Blocked by Pi extension" }
        : { action: "continue" },
      context: [],
      notifications: [],
    };
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
    return { disposition: { action: "continue" }, context: [], notifications: [] };
  }
  if (hook === "before_prompt") {
    return {
      disposition: { action: "continue" },
      context: await collectBeforePromptContext(String(payload.prompt ?? "")),
      notifications: [],
    };
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
    return { disposition: { action: "continue" }, context: [], notifications: [] };
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
  return context;
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
  if (isApiV03()) {
    if (!bridge.v03Contract.methods.includes(message.method)) {
      throw v03ProtocolError("unknown_method", `method ${message.method} is not selected`);
    }
    if (message.method === "tool/call") {
      assertV03ToolCallParams(message.params);
      return callPiTool(message);
    }
    if (message.method === "provider/stream") return handleProviderStream(message);
    if (message.method === "provider/cancel") {
      await handleProviderCancel(message.params);
      return null;
    }
    if (message.method === "shutdown") {
      assertV03ShutdownParams(message.params);
      await cleanupProviderCatalog();
      return { terminal: "shutdown" };
    }
    if (message.method === "$/cancelRequest") {
      const target = message.params?.id;
      inflight.get(keyOf(target))?.controller.abort();
      return null;
    }
    throw v03ProtocolError("unknown_method", `method ${message.method} is unavailable`);
  }
  updateHostStateFromMessage(message);
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
    return {};
  }
  if (message.method === "$/cancelRequest") {
    const target = message.params?.id;
    inflight.get(keyOf(target))?.controller.abort();
    return null;
  }
  throw new Error(`method not found: ${message.method}`);
}

function validV03RpcId(value) {
  return (typeof value === "string" && value.length > 0 && Buffer.byteLength(value) <= 256)
    || (Number.isSafeInteger(value) && value >= 0);
}

function assertV03ErrorObject(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw v03ProtocolError("invalid_request", "JSON-RPC error must be an object");
  }
  for (const name of Object.keys(value)) {
    if (name !== "code" && name !== "message" && name !== "data") {
      throw v03ProtocolError("invalid_request", "JSON-RPC error has unknown fields");
    }
  }
  const error = value;
  if (!Number.isSafeInteger(error.code) || typeof error.message !== "string") {
    throw v03ProtocolError("invalid_request", "JSON-RPC error code and message are invalid");
  }
  if (Object.prototype.hasOwnProperty.call(error, "data")) canonicalJson(error.data);
  const expected = Object.values(API_0_3_ERROR).find((item) => item.code === error.code);
  if (!expected || expected.message !== error.message) {
    throw v03ProtocolError("invalid_request", "JSON-RPC error does not match the API 0.3 error table");
  }
}

function validateV03Envelope(message) {
  if (!message || typeof message !== "object" || Array.isArray(message)) {
    throw v03ProtocolError("invalid_request", "JSON-RPC envelope must be an object");
  }
  if (message.jsonrpc !== "2.0") {
    throw v03ProtocolError("invalid_request", "JSON-RPC envelope must use jsonrpc 2.0");
  }
  const hasId = Object.prototype.hasOwnProperty.call(message, "id");
  const hasMethod = Object.prototype.hasOwnProperty.call(message, "method");
  const hasResult = Object.prototype.hasOwnProperty.call(message, "result");
  const hasError = Object.prototype.hasOwnProperty.call(message, "error");
  if (hasMethod) {
    const expected = hasId
      ? new Set(["jsonrpc", "id", "method", "params"])
      : new Set(["jsonrpc", "method", "params"]);
    if (hasResult || hasError || Object.keys(message).some((key) => !expected.has(key))) {
      throw v03ProtocolError("invalid_request", "JSON-RPC request has unknown or response fields");
    }
    if (hasId && !validV03RpcId(message.id)) {
      throw v03ProtocolError("invalid_request", "JSON-RPC request id is invalid");
    }
    if (
      typeof message.method !== "string"
      || Buffer.byteLength(message.method) > 128
      || !Object.prototype.hasOwnProperty.call(message, "params")
    ) {
      throw v03ProtocolError("invalid_request", "JSON-RPC request requires a bounded method and params");
    }
    canonicalJson(message.params);
    const notificationMethods = new Set(["$/cancelRequest", "provider/cancel"]);
    if (notificationMethods.has(message.method) !== !hasId) {
      throw v03ProtocolError("invalid_request", "JSON-RPC method id presence violates API 0.3 semantics");
    }
    if (message.method === "$/cancelRequest") assertV03CancellationParams(message.params);
    return;
  }
  if (!hasId || !validV03RpcId(message.id) || hasResult === hasError) {
    throw v03ProtocolError("invalid_request", "JSON-RPC response shape is invalid");
  }
  const expected = hasResult
    ? new Set(["jsonrpc", "id", "result"])
    : new Set(["jsonrpc", "id", "error"]);
  if (Object.keys(message).some((key) => !expected.has(key))) {
    throw v03ProtocolError("invalid_request", "JSON-RPC response has unknown fields");
  }
  if (hasError) assertV03ErrorObject(message.error);
  else canonicalJson(message.result);
}

function v03ErrorName(error) {
  if (error instanceof CancellationError) return "request_cancelled";
  if (typeof error?.v03Error === "string" && API_0_3_ERROR[error.v03Error]) return error.v03Error;
  if (error?.code === API_0_3_ERROR.invalid_params.code) return "invalid_params";
  if (error?.code === API_0_3_ERROR.capability_mismatch.code) return "capability_mismatch";
  return "internal_error";
}

async function sendV03Error(id, error) {
  const specification = API_0_3_ERROR[v03ErrorName(error)];
  await send({
    jsonrpc: "2.0",
    id,
    error: { code: specification.code, message: specification.message },
  });
}

async function onMessage(message) {
  if (!message || typeof message !== "object") return;
  if (isApiV03()) {
    try {
      validateV03Envelope(message);
    } catch (error) {
      const hasMethod = Object.prototype.hasOwnProperty.call(message, "method");
      const hasResponseField = Object.prototype.hasOwnProperty.call(message, "result")
        || Object.prototype.hasOwnProperty.call(message, "error");
      const pending = validV03RpcId(message.id) ? pendingHostRequests.get(message.id) : undefined;
      if (pending && (!hasMethod || hasResponseField)) {
        pendingHostRequests.delete(message.id);
        pending.reject(error);
        diagnostic(`Pi compatibility rejected API 0.3 host response: ${providerReason(error)}`);
      } else if (validV03RpcId(message.id) && hasMethod) {
        await sendV03Error(message.id, error);
      } else if (validV03RpcId(message.id) && !hasMethod) {
        diagnostic(`Pi compatibility rejected API 0.3 host response: ${providerReason(error)}`);
      } else {
        diagnostic(`Pi compatibility rejected API 0.3 envelope: ${providerReason(error)}`);
      }
      return;
    }
  }
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
    const target = message.params?.id;
    inflight.get(keyOf(target))?.controller.abort();
    if (isApiV03()) {
      const queued = pendingV03IncomingRequests.get(keyOf(target));
      if (queued) queued.cancelled = true;
    }
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
    if (isApiV03IncomingRequestCancelled(message)) throw new CancellationError();
    const result = await runScoped(message.id, () => handleRequest(message));
    await send({ jsonrpc: "2.0", id: message.id, result });
    if (isApiV03() && message.method === "initialize") {
      bridge.frameLimit = bridge.v03Contract.limits.max_frame_bytes;
      // Reverse provider registration must start only after the initialize
      // response and negotiated writer bound are both established.
      if (bridge.providers.size) queueInitialProviders();
    }
    if (message.method === "shutdown") setImmediate(() => process.exit(0));
  } catch (error) {
    if (isApiV03()) {
      const failedInitialize = message.method === "initialize";
      try {
        await sendV03Error(message.id, error);
      } finally {
        if (failedInitialize && bridge) {
          bridge.initialized = false;
          bridge.frameLimit = API_0_3_MAX_FRAME_BYTES;
        }
      }
      return;
    }
    const code = error instanceof CancellationError ? -32800 : -32000;
    await send({
      jsonrpc: "2.0",
      id: message.id,
      error: { code, message: error instanceof Error ? error.message : String(error) },
    });
  }
}

function dispatchIncoming(message) {
  const hasId = message?.id !== undefined;
  const hasMethod = Object.prototype.hasOwnProperty.call(message ?? {}, "method");
  const isResponse = hasId && !hasMethod;
  const isCancellation = message?.method === "$/cancelRequest"
    || (isApiV03() && message?.method === "provider/cancel");
  const isRequest = hasId && hasMethod && !isCancellation;
  const resumesV03Input = isApiV03() && message?.method === "initialize" && v03InitializationPending;
  const finish = () => {
    if (isRequest) releaseV03IncomingRequest(message);
    if (resumesV03Input) resumeV03InputAfterInitialize();
  };
  if (isRequest && !reserveV03IncomingRequest(message)) return;
  if (isResponse || isCancellation) {
    void onMessage(message)
      .catch((error) => boundedDiagnostic(`Pi compatibility dispatch failed: ${error}`))
      .finally(finish);
    return;
  }
  if (message?.method === "tool/call") {
    void orderedInputChain.then(() => onMessage(message))
      .catch((error) => boundedDiagnostic(`Pi compatibility dispatch failed: ${error}`))
      .finally(finish);
    return;
  }
  orderedInputChain = orderedInputChain
    .then(() => onMessage(message))
    .catch((error) => boundedDiagnostic(`Pi compatibility dispatch failed: ${error}`))
    .finally(finish);
}

let incomingBytes = Buffer.alloc(0);
let inputProtocolFailed = false;
let v03InitializationPending = false;
let inputDraining = false;
let inputEnded = false;

function failInputProtocol(message) {
  if (inputProtocolFailed) return;
  inputProtocolFailed = true;
  diagnostic(`Pi compatibility API 0.3 protocol failure: ${message}`);
  process.exitCode = 1;
  process.stdin.pause();
  process.stdin.destroy();
}

function dispatchFrame(bytes) {
  if (isApiV03()) {
    const limit = bridge?.frameLimit ?? API_0_3_MAX_FRAME_BYTES;
    if (bytes.length === 0 || bytes.length > limit) {
      failInputProtocol(bytes.length === 0 ? "empty frame" : "frame exceeds negotiated max_frame_bytes");
      return;
    }
    let line;
    try {
      // Keep a BOM visible so it cannot be silently normalized into an
      // apparently canonical JSON frame.
      line = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
    } catch (error) {
      failInputProtocol(`frame is not UTF-8: ${error}`);
      return;
    }
    let message;
    try {
      message = JSON.parse(line);
      if (canonicalJson(message) !== line) {
        failInputProtocol("frame is not canonical JSON");
        return;
      }
    } catch (error) {
      failInputProtocol(`invalid JSON: ${error}`);
      return;
    }
    if (message?.method === "initialize" && !bridge?.initialized) {
      // Do not decode a following frame against the pre-negotiation bound.
      // The host reader uses the same initialization barrier.
      v03InitializationPending = true;
    }
    dispatchIncoming(message);
    return;
  }

  const line = bytes.toString("utf8").replace(/\r$/, "");
  if (!line.trim()) return;
  try {
    dispatchIncoming(JSON.parse(line));
  } catch (error) {
    diagnostic(`Pi compatibility received invalid JSON: ${error}`);
  }
}

function drainIncomingFrames() {
  if (inputDraining || inputProtocolFailed || (isApiV03() && v03InitializationPending)) return;
  inputDraining = true;
  try {
    while (!inputProtocolFailed && !(isApiV03() && v03InitializationPending)) {
      const delimiterIndex = incomingBytes.indexOf(0x0a);
      const limit = isApiV03() ? (bridge?.frameLimit ?? API_0_3_MAX_FRAME_BYTES) : Number.MAX_SAFE_INTEGER;
      if (delimiterIndex < 0) {
        if (incomingBytes.length > limit) failInputProtocol("unterminated frame exceeds negotiated max_frame_bytes");
        return;
      }
      const frame = incomingBytes.subarray(0, delimiterIndex);
      incomingBytes = incomingBytes.subarray(delimiterIndex + 1);
      dispatchFrame(frame);
      if (
        isApiV03()
        && v03InitializationPending
        && incomingBytes.length > API_0_3_MAX_FRAME_BYTES + 1
      ) {
        failInputProtocol("input buffered during initialization exceeds the API 0.3 frame bound");
      }
    }
  } finally {
    inputDraining = false;
  }
}

function finishInputEnd() {
  if (inputProtocolFailed || (isApiV03() && v03InitializationPending)) return;
  drainIncomingFrames();
  if (inputProtocolFailed || (isApiV03() && v03InitializationPending)) return;
  if (isApiV03() && incomingBytes.length) {
    failInputProtocol("stream ended without the required LF delimiter");
    return;
  }
  if (!isApiV03() && incomingBytes.length) {
    dispatchFrame(incomingBytes);
    incomingBytes = Buffer.alloc(0);
  }
  if (!inputProtocolFailed) process.exit(0);
}

function resumeV03InputAfterInitialize() {
  v03InitializationPending = false;
  setImmediate(() => {
    drainIncomingFrames();
    if (inputEnded) finishInputEnd();
  });
}

process.stdin.on("data", (chunk) => {
  if (inputProtocolFailed) return;
  const bytes = Buffer.from(chunk);
  // Initialization is an explicit read barrier. Bound buffered pipelined input
  // while it is active so a peer cannot turn the barrier into an unbounded
  // allocation before the selected frame limit is installed.
  if (
    isApiV03()
    && v03InitializationPending
    && incomingBytes.length + bytes.length > API_0_3_MAX_FRAME_BYTES + 1
  ) {
    failInputProtocol("input buffered during initialization exceeds the API 0.3 frame bound");
    return;
  }
  incomingBytes = Buffer.concat([incomingBytes, bytes]);
  drainIncomingFrames();
});
process.stdin.on("end", () => {
  inputEnded = true;
  finishInputEnd();
});
process.stdin.on("close", () => {
  if (inputProtocolFailed) process.exit(1);
});
process.on("uncaughtException", (error) => diagnostic(`Pi compatibility uncaught exception: ${error.stack ?? error}`));
process.on("unhandledRejection", (error) => diagnostic(`Pi compatibility unhandled rejection: ${error}`));
