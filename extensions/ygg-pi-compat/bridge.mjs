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
import { existsSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const API_VERSION = "0.2";
const REQUIRED_FEATURES = ["request_cancellation", "content_parts"];
const OPTIONAL_FEATURES = [
  "request_progress",
  "artifacts",
  "lifecycle_events",
  "dynamic_tools",
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
  globalThis.console = new Console({
    stdout: process.stderr,
    stderr: process.stderr,
    colorMode: false,
  });
}

installProtocolSafeConsole();

function parseArgs(argv) {
  const result = { extensions: [], agentDir: null, cwd: null, commandName: "pi" };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--extension" || value === "-e") {
      const extension = argv[++index];
      if (!extension) throw new Error("--extension requires a path");
      result.extensions.push(extension);
    } else if (value === "--agent-dir") {
      result.agentDir = argv[++index];
      if (!result.agentDir) throw new Error("--agent-dir requires a path");
    } else if (value === "--cwd") {
      result.cwd = argv[++index];
      if (!result.cwd) throw new Error("--cwd requires a path");
    } else if (value === "--command") {
      result.commandName = argv[++index];
      if (!result.commandName) throw new Error("--command requires a name");
    } else if (value === "--help" || value === "-h") {
      process.stdout.write(
        "Usage: bridge.mjs --extension PATH [--extension PATH ...] [--agent-dir DIR]\n",
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
        process.stdout.write(line, (error) =>
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

function makeUi() {
  return {
    async select(title, options) {
      const prompt = `${title}\n${options.map((value, i) => `${i + 1}. ${value}`).join("\n")}\nSelect an option by name or number:`;
      const value = await this.input(prompt);
      if (value === undefined) return undefined;
      const index = Number.parseInt(value, 10);
      return Number.isInteger(index) && index >= 1 && index <= options.length
        ? options[index - 1]
        : options.find((option) => option === value) ?? value;
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
    async input(title) {
      const response = await requestHost("input/request", {
        parent_request_id: parentRequestId(),
        prompt: String(title),
        secret: false,
      });
      return response?.value ?? undefined;
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
    setEditorComponent() {
      return unsupported("ctx.ui.setEditorComponent");
    },
    setAutocompleteProvider() {
      return unsupported("ctx.ui.setAutocompleteProvider");
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

function makeExtensionContextActions() {
  return {
    getModel: () => undefined,
    getScopedModels: () => [],
    isIdle: () => true,
    isProjectTrusted: () => true,
    getSignal: () => scopes.getStore()?.signal,
    abort: () => currentScope().controller.abort(),
    hasPendingMessages: () => false,
    shutdown: () => unsupported("ctx.shutdown"),
    getContextUsage: () => undefined,
    compact: () => unsupported("ctx.compact"),
    getSystemPrompt: () => "",
    getSystemPromptOptions: () => ({ cwd: bridge.cwd }),
  };
}

function makeExtensionActions() {
  return {
    sendMessage: () => unsupported("pi.sendMessage"),
    sendUserMessage: () => unsupported("pi.sendUserMessage"),
    appendEntry: () => unsupported("pi.appendEntry"),
    setSessionName: () => unsupported("pi.setSessionName"),
    getSessionName: () => undefined,
    setLabel: () => unsupported("pi.setLabel"),
    getActiveTools: () => bridge.toolNames,
    getAllTools: () => bridge.toolInfos,
    setActiveTools: () => unsupported("pi.setActiveTools"),
    refreshTools: () => scheduleToolRefresh(),
    getCommands: () => bridge.runner?.getRegisteredCommands?.() ?? [],
    setModel: () => Promise.reject(new Error("Pi compatibility API is not supported by Ygg: pi.setModel")),
    getThinkingLevel: () => "off",
    setThinkingLevel: () => unsupported("pi.setThinkingLevel"),
  };
}

function makeModelRegistry() {
  return makeThrowingProxy("ctx.modelRegistry", {
    getAll: () => [],
    getAvailable: () => [],
    find: () => undefined,
    hasConfiguredAuth: () => false,
  });
}

function makeSessionManager() {
  return makeThrowingProxy("ctx.sessionManager", {
    getEntries: () => [],
    getLeafId: () => undefined,
  });
}

function findPiPackageRoot(extensionPaths) {
  const candidates = [];
  for (const value of [
    process.env.YGG_PI_PACKAGE,
    process.env.PI_CODING_AGENT_PACKAGE,
  ]) {
    if (value) candidates.push(resolve(value));
  }

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
      // Continue with conventional locations.
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
  for (const candidate of candidates) {
    const root = resolve(candidate);
    if (seen.has(root)) continue;
    seen.add(root);
    if (existsSync(join(root, "dist/index.js"))) return root;
  }
  throw new Error(
    "could not locate @earendil-works/pi-coding-agent; set YGG_PI_PACKAGE to its package root",
  );
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
  bridge = {
    cwd: resolve(params.workspace ?? args.cwd ?? process.cwd()),
    agentDir: resolve(args.agentDir ?? process.env.YGG_PI_AGENT_DIR ?? join(homedir(), ".pi/agent")),
    extensionPaths: args.extensions.map((path) => resolve(path)),
    commandName: args.commandName,
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

  const packageRoot = findPiPackageRoot(bridge.extensionPaths);
  const pi = await import(pathToFileURL(join(packageRoot, "dist/index.js")).href);
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
  for (const error of loaded.errors ?? []) {
    diagnostic(`Pi extension load failed at ${error.path}: ${error.error}`);
  }
  if (!loaded.extensions?.length) {
    throw new Error("Pi loader found no extension modules");
  }

  bridge.runner = new pi.ExtensionRunner(
    loaded.extensions,
    loaded.runtime,
    bridge.cwd,
    makeSessionManager(),
    makeModelRegistry(),
  );
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
  bridge.runner.onError((error) => {
    const location = error?.extensionPath ? ` in ${error.extensionPath}` : "";
    const event = error?.event ? ` during ${error.event}` : "";
    boundedDiagnostic(`Pi extension handler failed${location}${event}: ${error?.error ?? error}`);
  });

  const initialTools = bridge.runner.getAllRegisteredTools();
  setCurrentTools(initialTools, 0);
  bridge.commands = bridge.runner.getRegisteredCommands();
  bridge.unsupported = [];
  for (const extension of loaded.extensions) {
    if (extension.shortcuts.size) bridge.unsupported.push(`${extension.path}: shortcuts`);
    if (extension.flags.size) bridge.unsupported.push(`${extension.path}: flags`);
    if (extension.messageRenderers.size) bridge.unsupported.push(`${extension.path}: message renderers`);
    if (extension.entryRenderers?.size) bridge.unsupported.push(`${extension.path}: entry renderers`);
    if (extension.markdownTransformer) bridge.unsupported.push(`${extension.path}: markdown transformer`);
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
  for (const warning of bridge.unsupported) diagnostic(`Pi compatibility: ${warning} is unavailable in Ygg`);
  return {
    api_version: API_VERSION,
    tools,
    commands: [
      {
        name: bridge.commandName,
        description: `Run bridged Pi command(s): /${bridge.commandName} COMMAND [arguments]`,
        usage: `/${bridge.commandName} COMMAND [arguments]`,
      },
    ],
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
  const transformed = await bridge.runner.emitToolResult(toolResultEvent);
  const finalContent = await lowerContent(transformed?.content ?? result?.content);
  return {
    content: finalContent,
    is_error: transformed?.isError ?? result?.isError === true,
    ...(result?.details === undefined ? {} : { metadata: result.details }),
  };
}

async function executePiCommand(message) {
  if (message.params?.name !== bridge.commandName) {
    throw new Error(`unknown bridged Pi command ${message.params?.name}`);
  }
  const command = message.params?.arguments?.[0];
  if (!command) throw new Error(`/${bridge.commandName} requires the bridged Pi command name`);
  const registered = bridge.commands.find((candidate) => candidate.name === command);
  if (!registered) throw new Error(`unknown bridged Pi command ${command}`);
  await registered.handler((message.params.arguments.slice(1) ?? []).join(" "), bridge.runner.createCommandContext());
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
    const result = await bridge.runner.emitToolCall(event);
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
      await bridge.runner.emitToolResult({
        type: "tool_result",
        toolCallId: `pi-ygg-hook-${String(message.id)}`,
        toolName: payload.name,
        input: payload.arguments ?? {},
        content: [{ type: "text", text: String(payload.output ?? "") }],
        isError: payload.is_error === true,
      });
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
