// Deliberately tiny, deterministic stand-in for Pi's public extension loader.
// It implements only the public methods consumed by bridge.mjs. The aggregate
// hooks deliberately model ordered source loading and one shared event bus.

import { existsSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const fixtureMode = process.env.YGG_PI_FIXTURE_MODE ?? "";
const fixtureEvents = (process.env.YGG_PI_FIXTURE_EVENTS ?? "")
  .split(",")
  .map((event) => event.trim())
  .filter(Boolean);

function makeTool(name, execute) {
  return {
    definition: {
      name,
      label: name,
      description: `${name} fixture tool`,
      parameters: {
        type: "object",
        properties: { value: { type: "string" } },
        additionalProperties: false,
      },
      execute,
    },
    sourceInfo: { path: "fixture-extension.mjs" },
  };
}

export function createEventBus() {
  const listeners = new Map();
  return {
    on(event, handler) {
      const handlers = listeners.get(event) ?? [];
      handlers.push(handler);
      listeners.set(event, handlers);
    },
    emit(event, payload) {
      for (const handler of listeners.get(event) ?? []) handler(payload);
    },
  };
}

function fixtureExtension(path) {
  const registration = fixtureMode === "registration";
  return {
    path,
    handlers: new Map(fixtureEvents.map((event) => [event, []])),
    shortcuts: registration ? new Map([["ctrl+alt+p", {}]]) : new Map(),
    flags: registration ? new Map([["plan", { default: false }]]) : new Map(),
    messageRenderers: registration ? new Map([["fixture", {}]]) : new Map(),
    entryRenderers: registration ? new Map([["fixture", {}]]) : new Map(),
    markdownTransformer: registration ? (() => "fixture") : null,
  };
}

export async function discoverAndLoadExtensions(paths, _cwd, _agentDir, eventBus) {
  console.log("fixture loader wrote to console.log");
  process.stdout.write("fixture loader wrote directly to stdout\n");
  delete globalThis.__yggPiAggregateShared;
  const runtime = {
    aggregate: {
      loadOrder: [],
      eventOrder: [],
      globalMarker: null,
    },
  };
  const extensions = [];
  const errors = [];
  for (const path of paths) {
    try {
      const entrypoint = existsSync(join(path, "index.mjs")) ? join(path, "index.mjs") : path;
      const module = await import(pathToFileURL(entrypoint).href);
      if (typeof module.installFakePiAggregate === "function") {
        await module.installFakePiAggregate({ eventBus, runtime });
      }
      extensions.push(fixtureExtension(path));
    } catch (error) {
      errors.push({ path, error });
    }
  }
  return { extensions, runtime, errors };
}

export class ExtensionRunner {
  constructor(extensions, runtime, _cwd, sessionManager, modelRegistry) {
    this.extensions = extensions;
    this.runtime = runtime;
    this.sessionManager = sessionManager;
    this.modelRegistry = modelRegistry;
    this.ui = null;
    this.actions = null;
    this.contextActions = null;
    this.commandContext = null;
    this.errorHandler = null;
    this.providerBindings = null;
    this.flagValues = new Map();
    this.localEvents = new Map();
    this.tools = [
      makeTool("fixture_echo", async (_id, input) => {
        console.log("fixture tool console output", input.value ?? "");
        return {
          content: [
            { type: "text", text: input.value ?? "echo" },
            { type: "image", data: "aGVsbG8=", mimeType: "image/png", alt: "fixture" },
          ],
          details: { fixture: true },
        };
      }),
      makeTool("fixture_prompt", async (_id, _input, signal, _update, context) => {
        const value = await context.ui.input("fixture input");
        if (signal.aborted) throw new Error("aborted fixture prompt");
        return { content: [{ type: "text", text: value ?? "missing" }] };
      }),
      makeTool("fixture_progress", async (_id, _input, _signal, onUpdate) => {
        await onUpdate?.({ content: [{ type: "text", text: "halfway" }] });
        return { content: [{ type: "text", text: "complete" }] };
      }),
    ];
    if (this.runtime?.aggregate?.loadOrder.length) {
      this.tools.push(
        makeTool("aggregate_state", async () => ({
          content: [{ type: "text", text: JSON.stringify(this.runtime.aggregate) }],
        })),
        makeTool("aggregate_wait", async (_id, _input, _signal, _update, context) => {
          const value = await context.ui.input("aggregate input");
          return { content: [{ type: "text", text: value ?? "missing" }] };
        }),
      );
    }
  }

  bindCore(actions, contextActions, providerBindings) {
    this.actions = actions;
    this.contextActions = contextActions;
    this.providerBindings = providerBindings;
  }

  bindCommandContext(context) {
    this.commandContext = context;
  }

  setUIContext(ui) {
    this.ui = ui;
  }

  onError(handler) {
    this.errorHandler = handler;
  }

  getAllRegisteredTools() {
    return this.tools.slice();
  }

  getRegisteredCommands() {
    const commands = [
      {
        name: "add-tool",
        description: "Add a dynamic fixture tool",
        handler: async () => {
          this.tools.push(makeTool("fixture_dynamic", async () => ({
            content: [{ type: "text", text: "dynamic" }],
          })));
          this.actions.refreshTools();
        },
      },
      {
        name: "ui-methods",
        description: "Validate current Pi UI method names",
        handler: async () => {
          for (const name of [
            "editor",
            "addAutocompleteProvider",
            "getEditorComponent",
            "getAllThemes",
            "getTheme",
            "setTheme",
            "getToolsExpanded",
            "setToolsExpanded",
          ]) {
            if (typeof this.ui[name] !== "function") throw new Error(`missing UI method ${name}`);
          }
          if (this.ui.theme.fg("accent", "plain") !== "plain") {
            throw new Error("compatibility theme did not preserve text");
          }
          try {
            this.ui.getEditorComponent();
            throw new Error("unsupported UI method unexpectedly succeeded");
          } catch (error) {
            if (!String(error).includes("Pi compatibility API is not supported by Ygg")) throw error;
          }
          this.ui.notify("ui-current-methods-explicit");
        },
      },
      {
        name: "host-state",
        description: "Validate host state bindings",
        handler: async () => {
          if (this.actions.getSessionName() !== "fixture session") {
            throw new Error(`unexpected session name ${this.actions.getSessionName()}`);
          }
          if (this.actions.getThinkingLevel() !== "high") {
            throw new Error(`unexpected thinking level ${this.actions.getThinkingLevel()}`);
          }
        },
      },
      {
        name: "unsupported",
        description: "Exercise an unsupported session action",
        handler: async (_arguments, context) => context.newSession(),
      },
    ];
    commands.push({
      name: "surface-probe",
      description: "Exercise one declared Pi public-surface fixture",
      handler: async (argumentsText, context) => this.probeSurface(String(argumentsText).trim(), context),
    });
    return commands;
  }

  setFlagValue(name, value) {
    this.flagValues.set(name, value);
  }

  getFlag(name) {
    if (this.flagValues.has(name)) return this.flagValues.get(name);
    return this.extensions[0]?.flags?.get(name)?.default;
  }

  async expectExplicit(target, action) {
    try {
      await action();
    } catch (error) {
      const message = String(error);
      if (message.includes("Pi compatibility API is not supported by Ygg")) {
        this.ui.notify(`surface:${target}:explicit`);
        return;
      }
      throw error;
    }
    throw new Error(`${target} unexpectedly succeeded`);
  }

  async probeSurface(target, context) {
    if (!target) throw new Error("surface-probe requires AREA.SURFACE");
    const explicit = (action) => this.expectExplicit(target, action);
    const bounded = () => this.ui.notify(`surface:${target}:bounded`);

    if (target.startsWith("extension_api.")) {
      const name = target.slice("extension_api.".length);
      if (["on", "registerTool", "registerCommand"].includes(name)) return bounded();
      if (["registerShortcut", "registerFlag", "registerMessageRenderer", "registerMarkdownTransformer", "registerEntryRenderer"].includes(name)) {
        return bounded();
      }
      if (name === "getFlag") return bounded(this.getFlag("fixture"));
      if (name === "sendMessage") return explicit(() => this.actions.sendMessage({ role: "assistant", content: "fixture" }));
      if (name === "sendUserMessage") return explicit(() => this.actions.sendUserMessage({ role: "user", content: "fixture" }));
      if (name === "appendEntry") return explicit(() => this.actions.appendEntry({ type: "custom", data: "fixture" }));
      if (name === "setSessionName") return explicit(() => this.actions.setSessionName("fixture"));
      if (name === "getSessionName") return bounded(this.actions.getSessionName());
      if (name === "setLabel") return explicit(() => this.actions.setLabel("fixture", "label"));
      if (name === "exec") return explicit(() => {
        if (typeof this.actions.exec !== "function") {
          throw new Error("Pi compatibility API is not supported by Ygg: pi.exec binding");
        }
        return this.actions.exec("true");
      });
      if (["getActiveTools", "getAllTools", "getCommands"].includes(name)) return bounded();
      if (name === "setActiveTools") return explicit(() => this.actions.setActiveTools(["fixture_echo"]));
      if (name === "setModel") return explicit(() => this.actions.setModel("fixture"));
      if (name === "getThinkingLevel") return bounded();
      if (name === "setThinkingLevel") return explicit(() => this.actions.setThinkingLevel("high"));
      if (name === "registerProvider") return explicit(() => this.providerBindings.registerProvider({ id: "fixture" }));
      if (name === "unregisterProvider") return explicit(() => this.providerBindings.unregisterProvider("fixture"));
      if (name === "events.emit") {
        this.localEvents.set("fixture", { value: true });
        return bounded();
      }
      if (name === "events.on") return bounded();
      throw new Error(`unknown extension API fixture ${name}`);
    }

    if (target.startsWith("ui_context.")) {
      const name = target.slice("ui_context.".length);
      if (name === "select") {
        await this.ui.select("Fixture choice", ["one", "two"]);
        return bounded();
      }
      if (name === "confirm") {
        await this.ui.confirm("Fixture confirm", "detail");
        return bounded();
      }
      if (name === "input") {
        await this.ui.input("Fixture input", "placeholder");
        return bounded();
      }
      if (name === "notify") return bounded();
      if (name === "setStatus") {
        this.ui.setStatus("fixture", "status");
        return bounded();
      }
      if (name === "theme") {
        if (this.ui.theme.bold("fixture") !== "fixture") throw new Error("theme text was not preserved");
        return bounded();
      }
      const calls = {
        editor: () => this.ui.editor("Fixture editor", "seed"),
        onTerminalInput: () => this.ui.onTerminalInput(() => {}),
        setWorkingMessage: () => this.ui.setWorkingMessage("fixture"),
        setWorkingVisible: () => this.ui.setWorkingVisible(true),
        setWorkingIndicator: () => this.ui.setWorkingIndicator("fixture"),
        setHiddenThinkingLabel: () => this.ui.setHiddenThinkingLabel("fixture"),
        setWidget: () => this.ui.setWidget("fixture", "widget"),
        setFooter: () => this.ui.setFooter(() => null),
        setHeader: () => this.ui.setHeader(() => null),
        setTitle: () => this.ui.setTitle("fixture"),
        custom: () => this.ui.custom(() => null),
        pasteToEditor: () => this.ui.pasteToEditor("fixture"),
        setEditorText: () => this.ui.setEditorText("fixture"),
        getEditorText: () => this.ui.getEditorText(),
        addAutocompleteProvider: () => this.ui.addAutocompleteProvider(() => []),
        setEditorComponent: () => this.ui.setEditorComponent(() => null),
        getEditorComponent: () => this.ui.getEditorComponent(),
        getAllThemes: () => this.ui.getAllThemes(),
        getTheme: () => this.ui.getTheme(),
        setTheme: () => this.ui.setTheme("fixture"),
        getToolsExpanded: () => this.ui.getToolsExpanded(),
        setToolsExpanded: () => this.ui.setToolsExpanded(true),
      };
      if (!calls[name]) throw new Error(`unknown UI fixture ${name}`);
      return explicit(calls[name]);
    }

    if (target.startsWith("context.")) {
      const name = target.slice("context.".length);
      if (["ui", "mode", "hasUI", "cwd", "thinkingLevel", "isIdle", "isProjectTrusted", "signal", "getSystemPromptOptions", "waitForIdle"].includes(name)) {
        if (name === "waitForIdle") await context.waitForIdle();
        return bounded();
      }
      if (name === "sessionManager") return explicit(() => this.sessionManager.getEntries());
      if (name === "modelRegistry") return explicit(() => this.modelRegistry.getModel());
      // The cancellation behavior itself is covered by the bridge cancellation
      // fixture; invoking it here would intentionally cancel this probe request.
      if (name === "abort") return bounded();
      const actions = {
        model: () => this.contextActions.getModel(),
        scopedModels: () => this.contextActions.getScopedModels(),
        abort: () => this.contextActions.abort(),
        hasPendingMessages: () => this.contextActions.hasPendingMessages(),
        shutdown: () => this.contextActions.shutdown(),
        getContextUsage: () => this.contextActions.getContextUsage(),
        compact: () => this.contextActions.compact(),
        getSystemPrompt: () => this.contextActions.getSystemPrompt(),
        newSession: () => context.newSession(),
        fork: () => context.fork(),
        navigateTree: () => context.navigateTree(),
        switchSession: () => context.switchSession(),
        reload: () => context.reload(),
        "replacement.sendMessage": () => { throw new Error("Pi compatibility API is not supported by Ygg: ctx.replacement.sendMessage"); },
        "replacement.sendUserMessage": () => { throw new Error("Pi compatibility API is not supported by Ygg: ctx.replacement.sendUserMessage"); },
      };
      if (!actions[name]) throw new Error(`unknown context fixture ${name}`);
      if (["model", "scopedModels", "getContextUsage"].includes(name)) return bounded(actions[name]());
      return explicit(actions[name]);
    }
    throw new Error(`unknown surface fixture ${target}`);
  }

  createContext() {
    return { ui: this.ui };
  }

  createCommandContext() {
    return { ...this.commandContext, ui: this.ui };
  }

  async emitBeforeAgentStart(prompt) {
    this.ui?.notify("event:before_agent_start:start");
    const result = {
      systemPrompt: `system context for ${prompt}`,
      messages: [{ role: "user", content: [{ type: "text", text: `message context for ${prompt}` }] }],
    };
    this.ui?.notify("event:before_agent_start:end");
    return result;
  }

  async emitContext(messages) {
    this.ui?.notify("event:context:start");
    const result = [...messages, { role: "user", content: "context event contribution" }];
    this.ui?.notify("event:context:end");
    return result;
  }

  async emitToolCall(event) {
    this.ui?.notify("event:tool_call:start");
    if (event.input?.mutateNative) event.input.value = "mutated";
    const result = {
      block: false,
      ...(event.input?.terminate ? { terminate: true } : {}),
    };
    this.ui?.notify("event:tool_call:end");
    return result;
  }

  async emitToolResult(event) {
    this.ui.notify("event:tool_result:start");
    this.ui.notify(`terminal:tool_result:${event.toolCallId}`);
    const result = event.input?.value === "transform"
      ? {
          ...event,
          content: [{ type: "text", text: "transformed" }],
          details: { transformed: true },
          isError: true,
          usage: { input: 1, output: 2 },
        }
      : undefined;
    this.ui.notify("event:tool_result:end");
    return result;
  }

  async emit(event) {
    const type = event.type;
    this.ui?.notify(`event:${type}:start`);
    if (type === "turn_start") await sleep(80);
    if (type === "session_start") {
      this.errorHandler?.({
        extensionPath: this.extensions[0].path,
        event: "session_start",
        error: new Error("fixture lifecycle failure"),
      });
    }
    this.ui?.notify(`event:${type}:end`);
  }
}
