// Deliberately tiny, deterministic stand-in for Pi's public extension loader.
// It implements only the public methods consumed by bridge.mjs.

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
  return {};
}

export async function discoverAndLoadExtensions(paths) {
  console.log("fixture loader wrote to console.log");
  const extension = {
    path: paths[0],
    shortcuts: new Map(),
    flags: new Map(),
    messageRenderers: new Map(),
    entryRenderers: new Map(),
    markdownTransformer: null,
  };
  return { extensions: [extension], runtime: {} };
}

export class ExtensionRunner {
  constructor(extensions) {
    this.extensions = extensions;
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
    ];
  }

  bindCore(actions) {
    this.actions = actions;
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
        handler: async () => {
          this.tools.push(makeTool("fixture_dynamic", async () => ({
            content: [{ type: "text", text: "dynamic" }],
          })));
          this.actions.refreshTools();
        },
      },
      {
        name: "unsupported",
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

  async emitToolCall() {
    return { block: false };
  }

  async emitToolResult(event) {
    this.ui.notify(`terminal:tool_result:${event.toolCallId}`);
    return event;
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
