import { spawn } from "node:child_process";
import {
  access,
  chmod,
  constants,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { createServer, request as httpRequest } from "node:http";
import type {
  IncomingHttpHeaders,
  IncomingMessage,
  Server,
  ServerResponse,
} from "node:http";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

export const LIVE_PROVIDER_ENDPOINT_ID = "custom-provider-3-e2e";
export const LIVE_API_MODEL = "e2e-model";
export const LIVE_MODEL_ID = `custom/e2e/${LIVE_API_MODEL}`;
export const LIVE_PROVIDER_TOKEN = "e2e-fake-token";

export const STREAM_PROMPT = "E2E_STREAM";
export const STREAM_PARTIAL = "E2E_STREAM_PARTIAL";
export const STREAM_REPLY = "E2E_STREAM_PARTIAL_DONE";
export const ABORT_PROMPT = "E2E_ABORT";
export const ABORT_PARTIAL = "E2E_ABORT_PARTIAL";
export const BRANCH_A_PROMPT = "E2E_BRANCH_A";
export const BRANCH_A_REPLY = "E2E_BRANCH_A_ASSISTANT";
export const EXPORT_CANARY =
  "ghp_1234567890abcdef1234567890abcdef";
export const BRANCH_B_PROMPT = `E2E_BRANCH_B ${EXPORT_CANARY}`;
export const BRANCH_B_REPLY = "E2E_BRANCH_B_ASSISTANT";
export const RESUME_PROMPT = "E2E_RESUMED";
export const RESUME_REPLY = "E2E_RESUMED_ASSISTANT";
export const TOOL_PROMPT = "E2E_TOOL";
export const TOOL_REPLY = "E2E_TOOL_ASSISTANT";
export const TOOL_FILE_CONTENT = "provider conformance canary";
export const RETRY_PROMPT = "E2E_RETRY";
export const RETRY_REPLY = "E2E_RETRY_ASSISTANT";
export const TIMEOUT_PROMPT = "E2E_TIMEOUT";
export const TIMEOUT_REPLY = "E2E_TIMEOUT_ASSISTANT";
export const ERROR_PROMPT = "E2E_PROVIDER_ERROR";
export const ERROR_DIAGNOSTIC = "E2E_PROVIDER_FAILURE";
export const FAILED_TURN_CONTEXT_MARKER =
  "The previous assistant turn failed before completion. Do not continue that request unless the user asks again.";
export const COMPACTION_PROMPT = "E2E_COMPACTION_REQUEST";
export const COMPACTION_REPLY = "## Goal\nPreserve deterministic E2E history.\n\n## Progress\nConfigured-provider compaction completed.";

const launchLine =
  /^Open ygg once: (http:\/\/127\.0\.0\.1:(\d+)\/__ygg\/launch\/([0-9a-f]{64}))$/;
const launchTokenInLog =
  /(\/__ygg\/launch\/)[0-9a-f]{64}/g;
const maxProviderRequestBytes = 1024 * 1024;
const defaultWaitMs = 20_000;

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
  reject(reason: unknown): void;
}

function deferred<T>(): Deferred<T> {
  let resolvePromise!: (value: T) => void;
  let rejectPromise!: (reason: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve;
    rejectPromise = reject;
  });
  return {
    promise,
    resolve: resolvePromise,
    reject: rejectPromise,
  };
}

async function withTimeout<T>(
  promise: Promise<T>,
  description: string,
  timeoutMs = defaultWaitMs,
): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`Timed out waiting for ${description}.`)),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

function textFromContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    return value.map(textFromContent).filter(Boolean).join("");
  }
  if (!value || typeof value !== "object") return "";
  const record = value as Record<string, unknown>;
  if (typeof record.text === "string") return record.text;
  if (typeof record.content === "string") return record.content;
  return textFromContent(record.content);
}

function latestUserPrompt(body: Record<string, unknown>): string {
  const messages = Array.isArray(body.messages) ? body.messages : [];
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const candidate = messages[index];
    if (!candidate || typeof candidate !== "object") continue;
    const message = candidate as Record<string, unknown>;
    if (message.role !== "user") continue;
    return textFromContent(message.content).trim();
  }
  return "";
}

function isCompactionRequest(body: Record<string, unknown>): boolean {
  return JSON.stringify(body.messages ?? []).includes(
    "You are a context summarization assistant.",
  );
}

function responseForPrompt(prompt: string): string {
  switch (prompt) {
    case STREAM_PROMPT:
      return STREAM_REPLY;
    case ABORT_PROMPT:
      return ABORT_PARTIAL;
    case BRANCH_A_PROMPT:
      return BRANCH_A_REPLY;
    case BRANCH_B_PROMPT:
      return BRANCH_B_REPLY;
    case RESUME_PROMPT:
      return RESUME_REPLY;
    case TOOL_PROMPT:
      return TOOL_REPLY;
    case RETRY_PROMPT:
      return RETRY_REPLY;
    case TIMEOUT_PROMPT:
      return TIMEOUT_REPLY;
    case COMPACTION_PROMPT:
      return COMPACTION_REPLY;
    default:
      return `E2E_ASSISTANT_${prompt.replaceAll(/\s+/g, "_").slice(0, 80)}`;
  }
}

function streamChunk(content: string): string {
  return `data: ${JSON.stringify({
    id: "chat-ygg-e2e",
    object: "chat.completion.chunk",
    created: 1,
    model: LIVE_API_MODEL,
    choices: [
      {
        index: 0,
        delta: { role: "assistant", content },
        finish_reason: null,
      },
    ],
  })}\n\n`;
}

function streamFinished(): string {
  return `data: ${JSON.stringify({
    id: "chat-ygg-e2e",
    object: "chat.completion.chunk",
    created: 1,
    model: LIVE_API_MODEL,
    choices: [
      {
        index: 0,
        delta: {},
        finish_reason: "stop",
      },
    ],
    usage: {
      prompt_tokens: 8,
      completion_tokens: 4,
      total_tokens: 12,
    },
  })}\n\ndata: [DONE]\n\n`;
}

function streamToolCall(): string {
  const started = {
    id: "chat-ygg-e2e-tool",
    object: "chat.completion.chunk",
    created: 1,
    model: LIVE_API_MODEL,
    choices: [
      {
        index: 0,
        delta: {
          role: "assistant",
          tool_calls: [
            {
              index: 0,
              id: "call_e2e_read",
              type: "function",
              function: {
                name: "read",
                arguments: JSON.stringify({ path: "provider-canary.txt" }),
              },
            },
          ],
        },
        finish_reason: null,
      },
    ],
  };
  const finished = {
    id: "chat-ygg-e2e-tool",
    object: "chat.completion.chunk",
    created: 1,
    model: LIVE_API_MODEL,
    choices: [
      {
        index: 0,
        delta: {},
        finish_reason: "tool_calls",
      },
    ],
  };
  return `data: ${JSON.stringify(started)}\n\ndata: ${JSON.stringify(finished)}\n\ndata: [DONE]\n\n`;
}

export interface RecordedChatRequest {
  prompt: string;
  body: Record<string, unknown>;
  authorization: string | undefined;
}

export class DeterministicChatProvider {
  private server: Server | null = null;
  private requestWaiters = new Map<string, Deferred<RecordedChatRequest>>();
  private attemptWaiters = new Map<string, Deferred<RecordedChatRequest>>();
  private releaseGates = new Map<string, Deferred<void>>();
  private abortWaiters = new Map<string, Deferred<void>>();
  private abortedPrompts = new Set<string>();
  private violations: string[] = [];
  private openResponses = new Set<ServerResponse>();

  readonly requests: RecordedChatRequest[] = [];
  origin = "";

  async start(): Promise<void> {
    if (this.server) throw new Error("The deterministic provider is already running.");
    this.server = createServer((request, response) => {
      void this.handle(request, response).catch((error: unknown) => {
        this.violations.push(
          error instanceof Error ? error.message : "provider request failed",
        );
        if (!response.headersSent) response.writeHead(500);
        if (!response.writableEnded) response.end();
      });
    });
    this.server.requestTimeout = 0;
    this.server.headersTimeout = 30_000;
    await new Promise<void>((resolvePromise, rejectPromise) => {
      this.server!.once("error", rejectPromise);
      this.server!.listen(0, "127.0.0.1", () => {
        this.server!.off("error", rejectPromise);
        resolvePromise();
      });
    });
    const address = this.server.address();
    if (!address || typeof address === "string") {
      throw new Error("The deterministic provider did not bind a TCP port.");
    }
    this.origin = `http://127.0.0.1:${address.port}`;
  }

  async waitForPrompt(
    prompt: string,
    timeoutMs = defaultWaitMs,
  ): Promise<RecordedChatRequest> {
    const existing = this.requests.find((request) => request.prompt === prompt);
    if (existing) return existing;
    let waiter = this.requestWaiters.get(prompt);
    if (!waiter) {
      waiter = deferred<RecordedChatRequest>();
      this.requestWaiters.set(prompt, waiter);
    }
    return withTimeout(waiter.promise, `provider request ${prompt}`, timeoutMs);
  }

  async waitForPromptAttempt(
    prompt: string,
    attempt: number,
    timeoutMs = defaultWaitMs,
  ): Promise<RecordedChatRequest> {
    const existing = this.requests.filter((request) => request.prompt === prompt)[
      attempt - 1
    ];
    if (existing) return existing;
    const key = `${prompt}#${attempt}`;
    let waiter = this.attemptWaiters.get(key);
    if (!waiter) {
      waiter = deferred<RecordedChatRequest>();
      this.attemptWaiters.set(key, waiter);
    }
    return withTimeout(
      waiter.promise,
      `provider request ${prompt} attempt ${attempt}`,
      timeoutMs,
    );
  }

  release(prompt: string): void {
    let gate = this.releaseGates.get(prompt);
    if (!gate) {
      gate = deferred<void>();
      this.releaseGates.set(prompt, gate);
    }
    gate.resolve();
  }

  async waitForAbort(prompt: string): Promise<void> {
    if (this.abortedPrompts.has(prompt)) return;
    let waiter = this.abortWaiters.get(prompt);
    if (!waiter) {
      waiter = deferred<void>();
      this.abortWaiters.set(prompt, waiter);
    }
    await withTimeout(waiter.promise, `provider abort ${prompt}`);
  }

  assertHealthy(): void {
    if (this.violations.length > 0) {
      throw new Error(
        `Deterministic provider boundary violations:\n${this.violations.join("\n")}`,
      );
    }
  }

  async close(): Promise<void> {
    const server = this.server;
    this.server = null;
    if (!server) return;
    for (const response of this.openResponses) response.destroy();
    this.openResponses.clear();
    const closeAllConnections = (
      server as Server & { closeAllConnections?: () => void }
    ).closeAllConnections;
    closeAllConnections?.call(server);
    await new Promise<void>((resolvePromise) => {
      server.close(() => resolvePromise());
    });
  }

  private async handle(
    request: IncomingMessage,
    response: ServerResponse,
  ): Promise<void> {
    if (
      request.method !== "POST" ||
      request.url !== "/v1/chat/completions"
    ) {
      this.violations.push(
        `Unexpected provider request ${request.method ?? "UNKNOWN"} ${request.url ?? ""}.`,
      );
      response.writeHead(404, { "content-type": "application/json" });
      response.end('{"error":"not found"}');
      return;
    }

    const chunks: Buffer[] = [];
    let byteLength = 0;
    for await (const chunk of request) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      byteLength += bytes.byteLength;
      if (byteLength > maxProviderRequestBytes) {
        this.violations.push("Provider request exceeded the E2E byte limit.");
        response.writeHead(413);
        response.end();
        return;
      }
      chunks.push(bytes);
    }

    let body: Record<string, unknown>;
    try {
      const decoded = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      if (!decoded || typeof decoded !== "object" || Array.isArray(decoded)) {
        throw new Error("request body was not an object");
      }
      body = decoded as Record<string, unknown>;
    } catch (error) {
      this.violations.push(
        `Provider received invalid JSON: ${
          error instanceof Error ? error.message : "unknown error"
        }.`,
      );
      response.writeHead(400);
      response.end();
      return;
    }

    const compactionRequest = isCompactionRequest(body);
    const prompt = compactionRequest ? COMPACTION_PROMPT : latestUserPrompt(body);
    const authorization = request.headers.authorization;
    const record = { prompt, body, authorization };
    const attempt =
      this.requests.filter((candidate) => candidate.prompt === prompt).length + 1;
    this.requests.push(record);
    this.requestWaiters.get(prompt)?.resolve(record);
    this.attemptWaiters.get(`${prompt}#${attempt}`)?.resolve(record);

    if (authorization !== `Bearer ${LIVE_PROVIDER_TOKEN}`) {
      this.violations.push("Provider request used the wrong bearer token.");
    }
    if (body.model !== LIVE_API_MODEL) {
      this.violations.push("Provider request used the wrong model.");
    }
    if (body.stream !== true) {
      this.violations.push("Provider request was not streaming.");
    }
    const tools = Array.isArray(body.tools) ? body.tools : [];
    const exposesRead = tools.some((candidate) => {
      if (!candidate || typeof candidate !== "object") return false;
      const tool = candidate as Record<string, unknown>;
      const fn = tool.function;
      return (
        fn !== null &&
        typeof fn === "object" &&
        (fn as Record<string, unknown>).name === "read"
      );
    });
    if (compactionRequest ? tools.length !== 0 : !exposesRead) {
      this.violations.push(
        compactionRequest
          ? "Compaction request unexpectedly exposed tools."
          : "Provider request did not expose the read tool.",
      );
    }
    if (!prompt) {
      this.violations.push("Provider request had no user prompt.");
    }
    if (this.violations.length > 0) {
      response.writeHead(400, { "content-type": "application/json" });
      response.end('{"error":"invalid deterministic request"}');
      return;
    }

    if (
      attempt === 1 &&
      (prompt === RETRY_PROMPT || prompt === TIMEOUT_PROMPT)
    ) {
      const timedOut = prompt === TIMEOUT_PROMPT;
      response.writeHead(timedOut ? 408 : 429, {
        "content-type": "application/json",
        "retry-after": "0",
      });
      response.end(
        JSON.stringify({
          error: {
            type: timedOut ? "request_timeout" : "rate_limit_error",
            message: `temporary ${LIVE_PROVIDER_TOKEN} ${prompt}`,
          },
        }),
      );
      return;
    }

    if (prompt === ERROR_PROMPT) {
      response.writeHead(429, {
        "content-type": "application/json",
        "retry-after": "0",
      });
      response.end(
        JSON.stringify({
          error: {
            type: "rate_limit_error",
            code: "e2e_rate_limit",
            message: `${ERROR_DIAGNOSTIC}: Bearer ${LIVE_PROVIDER_TOKEN}`,
          },
        }),
      );
      return;
    }

    response.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    response.flushHeaders();
    this.openResponses.add(response);

    let completed = false;
    const aborted = deferred<void>();
    response.once("close", () => {
      this.openResponses.delete(response);
      if (completed) return;
      this.abortedPrompts.add(prompt);
      this.abortWaiters.get(prompt)?.resolve();
      aborted.resolve();
    });

    if (prompt === TOOL_PROMPT && attempt === 1) {
      completed = true;
      response.end(streamToolCall());
      this.openResponses.delete(response);
      return;
    }
    if (prompt === STREAM_PROMPT) {
      response.write(streamChunk(STREAM_PARTIAL));
      let gate = this.releaseGates.get(prompt);
      if (!gate) {
        gate = deferred<void>();
        this.releaseGates.set(prompt, gate);
      }
      const released = await Promise.race([
        gate.promise.then(() => true),
        aborted.promise.then(() => false),
      ]);
      if (!released || response.destroyed) return;
      response.write(streamChunk("_DONE"));
    } else if (prompt === ABORT_PROMPT) {
      response.write(streamChunk(ABORT_PARTIAL));
      await aborted.promise;
      return;
    } else {
      response.write(streamChunk(responseForPrompt(prompt)));
    }

    completed = true;
    response.end(streamFinished());
    this.openResponses.delete(response);
  }
}

export interface HostStart {
  origin: string;
  port: number;
  launchUrl: string;
}

export interface RawLoopbackResponse {
  status: number;
  headers: IncomingHttpHeaders;
  body: Buffer;
}

export class LiveHostHarness {
  readonly provider = new DeterministicChatProvider();
  readonly root: string;
  readonly homeDir: string;
  readonly workspaceDir: string;
  readonly sessionDir: string;
  readonly processTempDir: string;
  readonly binaryPath: string;

  private child: ReturnType<typeof spawn> | null = null;
  private sanitizedOutput: string[] = [];
  private currentOrigin = "";
  private currentPort = 0;

  private constructor(root: string, binaryPath: string) {
    this.root = root;
    this.homeDir = join(root, "home");
    this.workspaceDir = join(root, "workspace");
    this.sessionDir = join(root, "sessions");
    this.processTempDir = join(root, "process-tmp");
    this.binaryPath = binaryPath;
  }

  static async create(): Promise<LiveHostHarness> {
    const root = await mkdtemp(join(tmpdir(), "ygg-live-e2e-"));
    await chmod(root, 0o700);
    const defaultBinary = resolve(
      dirname(fileURLToPath(import.meta.url)),
      "../../../../target/debug/ygg",
    );
    const configuredBinary = process.env.YGG_E2E_BINARY;
    const binaryPath = configuredBinary
      ? isAbsolute(configuredBinary)
        ? configuredBinary
        : resolve(process.cwd(), configuredBinary)
      : defaultBinary;
    const harness = new LiveHostHarness(root, binaryPath);
    await harness.initializeFilesystem();
    await harness.provider.start();
    await harness.writeProviderRegistry();
    return harness;
  }

  get origin(): string {
    if (!this.currentOrigin) throw new Error("The ygg host is not running.");
    return this.currentOrigin;
  }

  get port(): number {
    if (!this.currentPort) throw new Error("The ygg host is not running.");
    return this.currentPort;
  }

  async start(port = 0): Promise<HostStart> {
    if (this.child) throw new Error("The ygg host is already running.");
    await access(this.binaryPath, constants.X_OK);

    const child = spawn(
      this.binaryPath,
      [
        "--workspace",
        this.workspaceDir,
        "--workspace-trusted",
        "--session-dir",
        this.sessionDir,
        "--model",
        LIVE_MODEL_ID,
        "--reasoning",
        "off",
        "--max-turns",
        "2",
        "--no-context-files",
        "--offline",
        "serve",
        "--no-open",
        "--port",
        String(port),
      ],
      {
        cwd: this.workspaceDir,
        env: {
          HOME: this.homeDir,
          XDG_CONFIG_HOME: join(this.homeDir, ".config"),
          TMPDIR: this.processTempDir,
          PATH: process.env.PATH ?? "/usr/bin:/bin",
          NO_PROXY: "127.0.0.1,localhost",
          no_proxy: "127.0.0.1,localhost",
          YGG_E2E_PROVIDER_TOKEN: LIVE_PROVIDER_TOKEN,
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    this.child = child;
    this.sanitizedOutput = [];

    let embedded = false;
    const launched = deferred<HostStart>();
    let settled = false;
    const maybeResolve = (line: string) => {
      if (line === "Web app: embedded") embedded = true;
      const match = launchLine.exec(line);
      if (!match || !embedded || settled) return;
      settled = true;
      const launchUrl = match[1];
      const boundPort = Number(match[2]);
      this.currentOrigin = `http://127.0.0.1:${boundPort}`;
      this.currentPort = boundPort;
      launched.resolve({
        origin: this.currentOrigin,
        port: boundPort,
        launchUrl,
      });
    };
    const capture = (line: string) => {
      maybeResolve(line);
      this.sanitizedOutput.push(
        line.replaceAll(launchTokenInLog, "$1<redacted>"),
      );
    };

    if (!child.stdout || !child.stderr) {
      this.child = null;
      throw new Error("The ygg host did not expose process output.");
    }
    const stdout = createInterface({ input: child.stdout });
    const stderr = createInterface({ input: child.stderr });
    stdout.on("line", capture);
    stderr.on("line", capture);
    child.once("error", (error) => {
      if (settled) return;
      settled = true;
      launched.reject(error);
    });
    child.once("exit", (code, signal) => {
      stdout.close();
      stderr.close();
      if (!settled) {
        settled = true;
        launched.reject(
          new Error(
            `ygg serve exited before launch (${code ?? signal ?? "unknown"}).\n${this.sanitizedOutput.join("\n")}`,
          ),
        );
      }
    });

    try {
      return await withTimeout(launched.promise, "the ygg launch URL", 30_000);
    } catch (error) {
      await this.stop(false);
      throw error;
    }
  }

  async stop(expectClean = true): Promise<void> {
    const child = this.child;
    this.child = null;
    this.currentOrigin = "";
    if (!child) return;

    const exited =
      child.exitCode !== null || child.signalCode !== null
        ? Promise.resolve({
            code: child.exitCode,
            signal: child.signalCode,
          })
        : new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(
            (resolvePromise) => {
              child.once("exit", (code, signal) =>
                resolvePromise({ code, signal }),
              );
            },
          );
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGINT");
    }

    let result: { code: number | null; signal: NodeJS.Signals | null };
    try {
      result = await withTimeout(exited, "clean ygg shutdown", 20_000);
    } catch {
      child.kill("SIGTERM");
      try {
        result = await withTimeout(exited, "forced ygg shutdown", 5_000);
      } catch {
        child.kill("SIGKILL");
        result = await withTimeout(exited, "killed ygg shutdown", 5_000);
      }
    }
    if (expectClean && result.code !== 0) {
      throw new Error(
        `ygg serve did not shut down cleanly (${result.code ?? result.signal ?? "unknown"}).\n${this.sanitizedOutput.join("\n")}`,
      );
    }
  }

  async rawRequest(
    path: string,
    options: {
      method?: string;
      headers?: Record<string, string>;
      body?: string | Buffer;
    } = {},
  ): Promise<RawLoopbackResponse> {
    const body =
      typeof options.body === "string"
        ? Buffer.from(options.body)
        : options.body;
    return new Promise<RawLoopbackResponse>((resolvePromise, rejectPromise) => {
      const request = httpRequest(
        {
          hostname: "127.0.0.1",
          port: this.port,
          path,
          method: options.method ?? "GET",
          headers: options.headers,
        },
        (response) => {
          const chunks: Buffer[] = [];
          response.on("data", (chunk: Buffer) => chunks.push(chunk));
          response.once("error", rejectPromise);
          response.once("end", () => {
            resolvePromise({
              status: response.statusCode ?? 0,
              headers: response.headers,
              body: Buffer.concat(chunks),
            });
          });
        },
      );
      request.once("error", rejectPromise);
      if (body) request.write(body);
      request.end();
    });
  }

  async sessionSourceText(): Promise<string> {
    const files = await this.findFiles(this.sessionDir, ".jsonl");
    const contents = await Promise.all(
      files.map((path) => readFile(path, "utf8")),
    );
    return contents.join("\n");
  }

  async exportTemporaryEntries(): Promise<string[]> {
    const stateDir = join(this.sessionDir, ".serve");
    const entries = await readdir(stateDir);
    return entries.filter((entry) => entry.startsWith(".session-export-"));
  }

  async close(): Promise<void> {
    let firstError: unknown;
    try {
      await this.stop(false);
    } catch (error) {
      firstError = error;
    }
    try {
      await this.provider.close();
    } catch (error) {
      firstError ??= error;
    }
    await rm(this.root, { recursive: true, force: true });
    if (firstError) throw firstError;
  }

  private async initializeFilesystem(): Promise<void> {
    const directories = [
      this.homeDir,
      join(this.homeDir, ".config"),
      join(this.homeDir, ".ygg"),
      join(this.homeDir, ".ygg", "credentials"),
      this.workspaceDir,
      this.sessionDir,
      this.processTempDir,
    ];
    for (const directory of directories) {
      await mkdir(directory, { recursive: true, mode: 0o700 });
      await chmod(directory, 0o700);
    }
    await writeFile(
      join(this.workspaceDir, "provider-canary.txt"),
      `${TOOL_FILE_CONTENT}\n`,
      { mode: 0o600 },
    );
    await writeFile(
      join(this.homeDir, ".ygg", "config.toml"),
      "[compaction]\nmode = \"local\"\nthreshold_fraction = 0.85\nkeep_recent_tokens = 1\n",
      { mode: 0o600 },
    );
  }

  private async writeProviderRegistry(): Promise<void> {
    const path = join(
      this.homeDir,
      ".ygg",
      "credentials",
      "custom.json",
    );
    const registry = {
      version: 1,
      providers: {
        e2e: {
          label: "E2E",
          base_url: `${this.provider.origin}/v1/`,
          api_key: "",
          api_name: "",
          headers: [],
          models: [
            {
              api_name: LIVE_API_MODEL,
              display_name: "E2E Model",
              context_window: 32_768,
              max_output_tokens: 1_024,
              tools: true,
              parallel_tool_calls: false,
              vision: false,
              structured_output: false,
              reasoning: false,
              reasoning_configurable: false,
            },
          ],
          auto_discover: false,
          auth: {
            kind: "bearer_env",
            var: "YGG_E2E_PROVIDER_TOKEN",
          },
        },
      },
    };
    await writeFile(path, `${JSON.stringify(registry, null, 2)}\n`, {
      mode: 0o600,
    });
    await chmod(path, 0o600);
  }

  private async findFiles(root: string, suffix: string): Promise<string[]> {
    const matches: string[] = [];
    const visit = async (directory: string) => {
      const entries = await readdir(directory, { withFileTypes: true });
      for (const entry of entries) {
        const path = join(directory, entry.name);
        if (entry.isDirectory()) {
          await visit(path);
        } else if (entry.isFile() && entry.name.endsWith(suffix)) {
          matches.push(path);
        }
      }
    };
    await visit(root);
    return matches;
  }
}
