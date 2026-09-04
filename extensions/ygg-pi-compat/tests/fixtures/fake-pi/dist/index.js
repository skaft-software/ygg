// Deliberately tiny, deterministic stand-in for Pi's public extension loader.
// It implements only the public methods consumed by bridge.mjs. The aggregate
// hooks deliberately model ordered source loading and one shared event bus.

import { existsSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

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
  return {
    path,
    shortcuts: new Map(),
    flags: new Map(),
    messageRenderers: new Map(),
    entryRenderers: new Map(),
    markdownTransformer: null,
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
  constructor(extensions, runtime) {
    this.extensions = extensions;
    this.runtime = runtime;
    this.ui = null;
    this.actions = null;
    this.commandContext = null;
    this.errorHandler = null;
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

  bindCore(actions, contextActions) {
    this.actions = actions;
    this.contextActions = contextActions;
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
    return [
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
  }

  createContext() {
    return { ui: this.ui };
  }

  createCommandContext() {
    return { ...this.commandContext, ui: this.ui };
  }

  async emitBeforeAgentStart(prompt) {
    return {
      systemPrompt: `system context for ${prompt}`,
      messages: [{ role: "user", content: [{ type: "text", text: `message context for ${prompt}` }] }],
    };
  }

  async emitContext(messages) {
    return [...messages, { role: "user", content: "context event contribution" }];
  }

  async emitToolCall(event) {
    if (event.input?.mutateNative) event.input.value = "mutated";
    return {
      block: false,
      ...(event.input?.terminate ? { terminate: true } : {}),
    };
  }

  async emitToolResult(event) {
    this.ui.notify(`terminal:tool_result:${event.toolCallId}`);
    if (event.input?.value === "transform") {
      return {
        ...event,
        content: [{ type: "text", text: "transformed" }],
        details: { transformed: true },
        isError: true,
        usage: { input: 1, output: 2 },
      };
    }
    return undefined;
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
