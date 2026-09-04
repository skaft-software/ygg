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
import { delimiter, dirname, join, relative as relativePath, resolve, sep } from "node:path";
import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const API_VERSION = "0.2";
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

function currentScope() {
  const scope = scopes.getStore();
  if (!scope) throw new Error("Pi compatibility API used outside an active request");
  return scope;
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
    refreshTools: () => scheduleToolRefresh(),
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

function calculateLinkIdentity(piRuntime) {
  const hash = createHash("sha256");
  hash.update(Buffer.from("ygg-pi-aggregate-link-identity\0"));
  hash.update(unsignedBigEndian(LINK_IDENTITY_FORMAT, 4));
  for (const value of [
    BRIDGE_VERSION,
    SUPPORTED_PI_VERSION,
    args.yggVersion,
    args.commandName,
    resolve(args.linkManifest),
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
    || resolve(extension.manifest_path ?? "") !== resolve(args.linkManifest)
    || params.ygg_version !== args.yggVersion
  ) {
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

async function loadBridge(params) {
  validateNodeRuntime();
  verifySourceFingerprints();
  bridge = {
    cwd: resolve(params.workspace ?? args.cwd ?? process.cwd()),
    agentDir: resolve(args.agentDir ?? process.env.YGG_PI_AGENT_DIR ?? join(homedir(), ".pi/agent")),
    extensionPaths: args.extensions.map((path) => resolve(path)),
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
  };

  const offeredOptionalFeatures = new Set(params.protocol?.optional_features ?? []);
  for (const feature of OPTIONAL_FEATURES) {
    if (offeredOptionalFeatures.has(feature)) bridge.features.add(feature);
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
  bridge.runner.bindCore(makeExtensionActions(), makeExtensionContextActions(), {
    registerProvider: () => unsupported("pi.registerProvider"),
    registerNativeProvider: () => unsupported("pi.registerProvider"),
    unregisterProvider: () => unsupported("pi.unregisterProvider"),
  });
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
    if (extension.flags?.size) bridge.unsupported.push(`${label}: flags`);
    if (extension.messageRenderers?.size) bridge.unsupported.push(`${label}: message renderers`);
    if (extension.entryRenderers?.size) bridge.unsupported.push(`${label}: entry renderers`);
    if (extension.markdownTransformer) bridge.unsupported.push(`${label}: markdown transformer`);
  }
}

async function handleInitialize(message) {
  await loadBridge(message.params ?? {});
  bridge.initialized = true;
  if (bridge.toolRefreshRequested) {
    bridge.toolRefreshRequested = false;
    setImmediate(() => scheduleToolRefresh());
  }
  const tools = bridge.tools.map((tool) => toolDefinitionToYgg(tool.definition));
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
    `Pi compatibility readiness profile=pi_aggregate bridge_api=${API_VERSION} evidence_api=0.3 sources=${bridge.extensionPaths.length} pinned=${args.strictIdentity ? "yes" : "legacy"}`,
  );
  diagnostic(`Pi compatibility profile ${bridge.piRuntimeVersion} initialized`);
  return {
    api_version: API_VERSION,
    tools,
    commands,
    protocol: {
      version: API_VERSION,
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
    return await scopes.run(
      {
        parentRequestId: typeof id === "number" ? id : Number(id),
        controller,
        signal: controller.signal,
        progressSequence: 0,
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
  // A host may dispatch the first call for a newly published dynamic tool as
  // soon as it answers tools/register. Wait for that publication chain before
  // checking the catalog revision so the valid call cannot race its own reply.
  await bridge.toolRefreshChain;
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
  return {
    content: finalContent,
    is_error: transformed.isError === true,
    ...(metadata === undefined ? {} : { metadata }),
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
    const result = await runScoped(message.id, () => handleRequest(message));
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
