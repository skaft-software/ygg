import { PROTOCOL_VERSION } from "./protocol";
import type {
  ActionItem,
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  CommandAck,
  HostBootstrap,
  HostEvent,
  ModelSummary,
  OutputRef,
  PreviewRef,
  ProgressStep,
  ReasoningEffort,
  SessionBranchEntry,
  SessionBranchGraph,
  SessionEvent,
  SessionSnapshot,
  SessionStatus,
  SessionSummary,
  SourceRef,
  ThemeColor,
  ThemeDto,
  ThemeOption,
  TranscriptItem,
} from "./protocol";
import {
  deriveSessionTitle,
  isUntitledSession,
} from "./session-title";

type JsonObject = Record<string, unknown>;

const iso = (timestampMs: number) => new Date(timestampMs).toISOString();

function object(
  value: unknown,
  path: string,
  allowedKeys?: readonly string[],
): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new WireContractError(path, "must be an object");
  }
  const result = value as JsonObject;
  if (allowedKeys) {
    const allowed = new Set(allowedKeys);
    const unknown = Object.keys(result).find((key) => !allowed.has(key));
    if (unknown) {
      throw new WireContractError(`${path}.${unknown}`, "is not supported");
    }
  }
  return result;
}

function string(value: unknown, path: string): string {
  if (typeof value !== "string") {
    throw new WireContractError(path, "must be a string");
  }
  return value;
}

function boundedString(
  value: unknown,
  path: string,
  maxLength: number,
  allowBlank = false,
): string {
  const decoded = string(value, path);
  if (decoded.length > maxLength) {
    throw new WireContractError(path, `must be at most ${maxLength} characters`);
  }
  if (!allowBlank && decoded.trim().length === 0) {
    throw new WireContractError(path, "must not be blank");
  }
  return decoded;
}

function boolean(value: unknown, path: string): boolean {
  if (typeof value !== "boolean") {
    throw new WireContractError(path, "must be a boolean");
  }
  return value;
}

function number(value: unknown, path: string): number {
  if (
    typeof value !== "number" ||
    !Number.isSafeInteger(value) ||
    value < 0
  ) {
    throw new WireContractError(path, "must be a non-negative safe integer");
  }
  return value;
}

function array(value: unknown, path: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new WireContractError(path, "must be an array");
  }
  return value;
}

function optionalString(value: unknown, path: string): string | undefined {
  return value === undefined ? undefined : string(value, path);
}

function enumeration<const Values extends readonly string[]>(
  value: unknown,
  path: string,
  values: Values,
): Values[number] {
  const decoded = string(value, path);
  if (!values.includes(decoded)) {
    throw new WireContractError(
      path,
      `must be one of ${values.join(", ")}`,
    );
  }
  return decoded as Values[number];
}

function protocol(value: unknown, path: string): void {
  if (number(value, path) !== PROTOCOL_VERSION.major) {
    throw new WireContractError(path, "uses an incompatible protocol major");
  }
}

export class WireContractError extends Error {
  constructor(readonly path: string, message: string) {
    super(`${path} ${message}`);
    this.name = "WireContractError";
  }
}

export class UnsupportedWireCommandError extends Error {
  constructor(readonly commandType: ClientCommand["type"], message: string) {
    super(`${commandType}: ${message}`);
    this.name = "UnsupportedWireCommandError";
  }
}

const liveStates = [
  "idle",
  "working",
  "needsApproval",
  "needsInput",
  "done",
  "failed",
  "stopped",
  "offline",
  "locked",
] as const;

const attentionStates = [
  "none",
  "unreadCompletion",
  "approval",
  "input",
  "failure",
] as const;

const wireAuthorities = ["readOnly", "workspace", "fullAccess"] as const;

function projectStatus(value: unknown, path: string): SessionStatus {
  const state = enumeration(value, path, liveStates);
  switch (state) {
    case "needsApproval":
    case "needsInput":
      return "needs_attention";
    case "offline":
    case "locked":
      return "disconnected";
    default:
      return state;
  }
}

function projectAuthority(
  value: unknown,
  path: string,
): AuthorityProfile {
  return enumeration(value, path, wireAuthorities);
}

function encodeAuthority(authority: AuthorityProfile): AuthorityProfile {
  return authority;
}

function projectReasoning(value: unknown, path: string): ReasoningEffort {
  return boundedString(value, path, 128);
}

function projectThemeColor(value: unknown, path: string): ThemeColor {
  const candidate = object(value, path);
  const kind = enumeration(candidate.kind, `${path}.kind`, [
    "default",
    "rgb",
    "ansi",
  ] as const);
  if (kind === "default") {
    object(value, path, ["kind"]);
    return { kind };
  }
  if (kind === "ansi") {
    const color = object(value, path, ["kind", "index"]);
    return { kind, index: number(color.index, `${path}.index`) };
  }
  const color = object(value, path, ["kind", "red", "green", "blue"]);
  return {
    kind,
    red: number(color.red, `${path}.red`),
    green: number(color.green, `${path}.green`),
    blue: number(color.blue, `${path}.blue`),
  };
}

function projectTheme(value: unknown, path: string): ThemeDto {
  const theme = object(value, path, [
    "name",
    "source",
    "revision",
    "scheme",
    "density",
    "motion",
    "typography",
    "colors",
    "roles",
  ]);
  const typography = object(theme.typography, `${path}.typography`, [
    "bodyFamily",
    "monoFamily",
    "bodySize",
    "displayRatioMilli",
  ]);
  const colorsObject = object(theme.colors, `${path}.colors`);
  const colors = Object.fromEntries(
    Object.entries(colorsObject).map(([key, color]) => [
      key,
      projectThemeColor(color, `${path}.colors.${key}`),
    ]),
  );
  const rolesObject = object(theme.roles, `${path}.roles`);
  const roles = Object.fromEntries(
    Object.entries(rolesObject).map(([key, rawRole]) => {
      const role = object(rawRole, `${path}.roles.${key}`, [
        "foreground",
        "background",
        "bold",
        "dim",
        "italic",
        "underline",
        "strikethrough",
      ]);
      return [
        key,
        {
          foreground: optionalString(
            role.foreground,
            `${path}.roles.${key}.foreground`,
          ),
          background: optionalString(
            role.background,
            `${path}.roles.${key}.background`,
          ),
          bold: boolean(role.bold, `${path}.roles.${key}.bold`),
          dim: boolean(role.dim, `${path}.roles.${key}.dim`),
          italic: boolean(role.italic, `${path}.roles.${key}.italic`),
          underline: boolean(
            role.underline,
            `${path}.roles.${key}.underline`,
          ),
          strikethrough: boolean(
            role.strikethrough,
            `${path}.roles.${key}.strikethrough`,
          ),
        },
      ];
    }),
  );
  return {
    name: string(theme.name, `${path}.name`),
    source: enumeration(theme.source, `${path}.source`, [
      "bundled",
      "global",
      "project",
      "explicit",
    ] as const),
    revision: number(theme.revision, `${path}.revision`),
    scheme: enumeration(theme.scheme, `${path}.scheme`, [
      "light",
      "dark",
      "unknown",
    ] as const),
    density: enumeration(theme.density, `${path}.density`, [
      "compact",
      "comfortable",
      "airy",
    ] as const),
    motion: enumeration(theme.motion, `${path}.motion`, [
      "full",
      "reduced",
      "none",
    ] as const),
    typography: {
      body_family: string(
        typography.bodyFamily,
        `${path}.typography.bodyFamily`,
      ),
      mono_family: string(
        typography.monoFamily,
        `${path}.typography.monoFamily`,
      ),
      body_size: number(
        typography.bodySize,
        `${path}.typography.bodySize`,
      ),
      display_ratio_milli: number(
        typography.displayRatioMilli,
        `${path}.typography.displayRatioMilli`,
      ),
    },
    colors,
    roles,
  };
}

function projectThemeOption(value: unknown, path: string): ThemeOption {
  const option = object(value, path, ["id", "theme"]);
  return {
    id: string(option.id, `${path}.id`),
    theme: projectTheme(option.theme, `${path}.theme`),
  };
}

function projectModel(value: unknown, path: string): ModelSummary {
  const model = object(value, path, [
    "id",
    "name",
    "provider",
    "local",
    "available",
    "reasoning",
    "defaultReasoning",
    "inputModalities",
  ]);
  const rawEfforts = array(model.reasoning, `${path}.reasoning`);
  if (rawEfforts.length > 32) {
    throw new WireContractError(`${path}.reasoning`, "has more than 32 options");
  }
  const efforts = rawEfforts.map((effort, index) =>
    projectReasoning(effort, `${path}.reasoning[${index}]`),
  );
  if (new Set(efforts).size !== efforts.length) {
    throw new WireContractError(`${path}.reasoning`, "contains duplicates");
  }
  const inputModalities = array(
    model.inputModalities,
    `${path}.inputModalities`,
  ).map((modality, index) =>
    enumeration(modality, `${path}.inputModalities[${index}]`, [
        "text",
        "image",
        "audio",
        "document",
      ] as const),
  );
  if (
    !inputModalities.includes("text") ||
    new Set(inputModalities).size !== inputModalities.length
  ) {
    throw new WireContractError(
      `${path}.inputModalities`,
      "must contain text and no duplicates",
    );
  }
  const defaultReasoning =
    model.defaultReasoning === undefined
      ? undefined
      : projectReasoning(model.defaultReasoning, `${path}.defaultReasoning`);
  if (defaultReasoning !== undefined && !efforts.includes(defaultReasoning)) {
    throw new WireContractError(
      `${path}.defaultReasoning`,
      "must be advertised by the model",
    );
  }
  return {
    id: boundedString(model.id, `${path}.id`, 256),
    name: boundedString(model.name, `${path}.name`, 256),
    provider: boundedString(model.provider, `${path}.provider`, 128),
    local: boolean(model.local, `${path}.local`),
    available: boolean(model.available, `${path}.available`),
    reasoning: efforts,
    defaultReasoning,
    inputModalities,
  };
}

interface WireModelSelection {
  provider: string;
  model: string;
  reasoning: ReasoningEffort;
}

function projectModelSelection(
  value: unknown,
  path: string,
  models?: readonly ModelSummary[],
): WireModelSelection {
  const model = object(value, path, ["provider", "model", "reasoning"]);
  const projected = {
    provider: boundedString(model.provider, `${path}.provider`, 128),
    model: boundedString(model.model, `${path}.model`, 256),
    reasoning: projectReasoning(model.reasoning, `${path}.reasoning`),
  };
  if (models) {
    const catalogModel = models.find(
      (candidate) => candidate.id === projected.model,
    );
    if (!catalogModel || catalogModel.provider !== projected.provider) {
      throw new WireContractError(path, "does not identify a catalog model");
    }
    if (!catalogModel.reasoning.includes(projected.reasoning)) {
      throw new WireContractError(
        `${path}.reasoning`,
        "is not advertised by the selected model",
      );
    }
  }
  return projected;
}

function summaryPreview(
  status: SessionStatus,
  attention: (typeof attentionStates)[number],
): string {
  if (attention === "approval") return "Needs your approval";
  if (attention === "input") return "Needs your input";
  if (attention === "failure") return "Needs inspection";
  if (attention === "unreadCompletion") return "Completed";
  if (status === "working") return "Working";
  return "Ready when you are";
}

function projectSummary(
  value: unknown,
  path: string,
  models?: readonly ModelSummary[],
): SessionSummary {
  const summary = object(value, path, [
    "id",
    "projectId",
    "title",
    "tags",
    "createdAtMs",
    "modifiedAtMs",
    "pinned",
    "archived",
    "provisional",
    "liveState",
    "attention",
    "owner",
    "model",
  ]);
  const status = projectStatus(summary.liveState, `${path}.liveState`);
  const attention = enumeration(
    summary.attention,
    `${path}.attention`,
    attentionStates,
  );
  enumeration(summary.owner, `${path}.owner`, [
    "inactive",
    "hosted",
    "externallyLocked",
  ] as const);
  const model = projectModelSelection(summary.model, `${path}.model`, models);
  array(summary.tags ?? [], `${path}.tags`).forEach((tag, index) =>
    string(tag, `${path}.tags[${index}]`),
  );
  return {
    id: string(summary.id, `${path}.id`),
    projectId:
      optionalString(summary.projectId, `${path}.projectId`) ?? "",
    title: string(summary.title, `${path}.title`),
    preview: summaryPreview(status, attention),
    status,
    updatedAt: iso(number(summary.modifiedAtMs, `${path}.modifiedAtMs`)),
    pinned: boolean(summary.pinned, `${path}.pinned`),
    archived: boolean(summary.archived, `${path}.archived`),
    unread: attention === "unreadCompletion",
    modelId: model.model,
    attentionCount:
      attention === "approval" ||
      attention === "input" ||
      attention === "failure"
        ? 1
        : 0,
  };
}

function projectAttachment(value: unknown, path: string): AttachmentRef {
  const attachment = object(value, path, [
    "handle",
    "displayName",
    "mediaType",
    "byteLen",
  ]);
  const handle = string(attachment.handle, `${path}.handle`);
  return {
    id: handle,
    handle,
    name: string(attachment.displayName, `${path}.displayName`),
    mediaType: string(attachment.mediaType, `${path}.mediaType`),
    size: number(attachment.byteLen, `${path}.byteLen`),
  };
}

function itemState(value: unknown, path: string) {
  const lifecycle = enumeration(value, path, [
    "provisional",
    "committed",
  ] as const);
  return lifecycle === "provisional" ? ("streaming" as const) : ("committed" as const);
}

function actionKind(name: string): ActionItem["actionKind"] {
  const normalized = name.toLocaleLowerCase();
  if (normalized.includes("search")) return "web_search";
  if (normalized.includes("preview") || normalized.includes("browser")) {
    return "preview";
  }
  if (normalized.includes("write") || normalized.includes("edit")) {
    return "file_write";
  }
  if (normalized.includes("read")) return "file_read";
  if (
    normalized.includes("shell") ||
    normalized.includes("command") ||
    normalized.includes("exec")
  ) {
    return "command";
  }
  return "analysis";
}

interface ProjectItemContext {
  timestampMs: number;
}

function projectSource(value: unknown, path: string): SourceRef {
  const source = object(value, path, [
    "id",
    "kind",
    "title",
    "handle",
    "originItemId",
    "consultedAtMs",
    "cited",
    "available",
  ]);
  const kind = enumeration(source.kind, `${path}.kind`, [
    "attachment",
    "file",
    "web",
    "resource",
    "other",
  ] as const);
  const originItemId = optionalString(
    source.originItemId,
    `${path}.originItemId`,
  );
  boolean(source.cited, `${path}.cited`);
  boolean(source.available, `${path}.available`);
  string(source.handle, `${path}.handle`);
  const consultedAt = number(source.consultedAtMs, `${path}.consultedAtMs`);
  return {
    id: string(source.id, `${path}.id`),
    handle: string(source.handle, `${path}.handle`),
    originItemId,
    kind:
      kind === "resource"
        ? "documentation"
        : kind === "other"
          ? "file"
          : kind,
    title: string(source.title, `${path}.title`),
    subtitle: `Consulted · ${iso(consultedAt)}`,
    consultedAt: iso(consultedAt),
    iconLabel:
      kind === "web" ? "WEB" : kind === "attachment" ? "FILE" : "SRC",
    available: boolean(source.available, `${path}.available`),
  };
}

function projectArtifact(value: unknown, path: string): OutputRef {
  const artifact = object(value, path, [
    "id",
    "kind",
    "name",
    "mediaType",
    "handle",
    "byteLen",
    "contentHash",
    "originItemId",
    "available",
  ]);
  const kind = enumeration(artifact.kind, `${path}.kind`, [
    "file",
    "image",
    "document",
    "spreadsheet",
    "presentation",
    "site",
    "other",
  ] as const);
  string(artifact.handle, `${path}.handle`);
  optionalString(artifact.contentHash, `${path}.contentHash`);
  const originItemId = optionalString(
    artifact.originItemId,
    `${path}.originItemId`,
  );
  boolean(artifact.available, `${path}.available`);
  const byteLen = number(artifact.byteLen, `${path}.byteLen`);
  return {
    id: string(artifact.id, `${path}.id`),
    handle: string(artifact.handle, `${path}.handle`),
    originItemId,
    kind:
      kind === "image" || kind === "document" || kind === "site"
        ? kind
        : "file",
    title: string(artifact.name, `${path}.name`),
    subtitle: `${byteLen.toLocaleString()} bytes`,
    mimeType: string(artifact.mediaType, `${path}.mediaType`),
    updatedAt: iso(0),
    available: boolean(artifact.available, `${path}.available`),
  };
}

function projectPreview(value: unknown, path: string): PreviewRef {
  const preview = object(value, path, ["handle", "title", "live"]);
  const handle = string(preview.handle, `${path}.handle`);
  return {
    id: handle,
    title: string(preview.title, `${path}.title`),
    kind: "web",
    status: boolean(preview.live, `${path}.live`) ? "live" : "stopped",
  };
}

function taggedPayload(value: unknown, path: string) {
  const payload = object(value, path, ["type", "data"]);
  return {
    type: string(payload.type, `${path}.type`),
    data: object(payload.data, `${path}.data`),
  };
}

interface ToolResultProjection {
  toolCallItemId: string;
  content: string;
  failed: boolean;
}

function projectToolResult(
  value: unknown,
  path: string,
): ToolResultProjection {
  const data = object(value, path, [
    "toolCallItemId",
    "content",
    "isError",
  ]);
  return {
    toolCallItemId: string(
      data.toolCallItemId,
      `${path}.toolCallItemId`,
    ),
    content: string(data.content, `${path}.content`),
    failed: boolean(data.isError, `${path}.isError`),
  };
}

function projectItem(
  value: unknown,
  path: string,
  context: ProjectItemContext,
): TranscriptItem | null {
  const item = object(value, path, [
    "id",
    "runId",
    "turnId",
    "providerAttempt",
    "lifecycle",
    "durableEntryId",
    "payload",
  ]);
  optionalString(item.runId, `${path}.runId`);
  const id = string(item.id, `${path}.id`);
  const turnId =
    optionalString(item.turnId, `${path}.turnId`) ??
    optionalString(item.runId, `${path}.runId`) ??
    id;
  if (item.providerAttempt !== undefined) {
    number(item.providerAttempt, `${path}.providerAttempt`);
  }
  optionalString(item.durableEntryId, `${path}.durableEntryId`);
  const state = itemState(item.lifecycle, `${path}.lifecycle`);
  const payload = taggedPayload(item.payload, `${path}.payload`);
  const base = {
    id,
    turnId,
    state,
    createdAt: iso(context.timestampMs),
  };

  switch (payload.type) {
    case "userMessage": {
      const data = object(payload.data, `${path}.payload.data`, [
        "text",
        "attachments",
        "delivery",
      ]);
      const delivery =
        data.delivery === undefined
          ? undefined
          : enumeration(data.delivery, `${path}.payload.data.delivery`, [
              "submit",
              "steer",
              "followUp",
            ] as const);
      return {
        ...base,
        kind: "user_message",
        content: string(data.text, `${path}.payload.data.text`),
        delivery,
        attachments: array(
          data.attachments ?? [],
          `${path}.payload.data.attachments`,
        ).map((attachment, index) =>
          projectAttachment(
            attachment,
            `${path}.payload.data.attachments[${index}]`,
          ),
        ),
      };
    }
    case "assistantMessage": {
      const data = object(payload.data, `${path}.payload.data`, ["text"]);
      return {
        ...base,
        kind: "assistant_message",
        content: string(data.text, `${path}.payload.data.text`),
      };
    }
    case "reasoning": {
      const data = object(payload.data, `${path}.payload.data`, ["text"]);
      return {
        ...base,
        kind: "reasoning",
        content: string(data.text, `${path}.payload.data.text`),
        summary: state === "streaming" ? "Thinking" : "Reasoning",
      };
    }
    case "toolCall": {
      const data = object(payload.data, `${path}.payload.data`, [
        "name",
        "arguments",
        "progress",
        "droppedProgressBytes",
      ]);
      const name = string(data.name, `${path}.payload.data.name`);
      if (!("arguments" in data)) {
        throw new WireContractError(
          `${path}.payload.data.arguments`,
          "is required",
        );
      }
      const progress = optionalString(
        data.progress,
        `${path}.payload.data.progress`,
      );
      number(
        data.droppedProgressBytes ?? 0,
        `${path}.payload.data.droppedProgressBytes`,
      );
      return {
        ...base,
        kind: "action",
        actionKind: actionKind(name),
        label: name,
        detail: progress,
      };
    }
    case "toolResult": {
      projectToolResult(payload.data, `${path}.payload.data`);
      return null;
    }
    case "fileChange": {
      const data = object(payload.data, `${path}.payload.data`, [
        "handle",
        "resultHandle",
        "displayPath",
        "additions",
        "deletions",
      ]);
      const diffHandle = string(
        data.handle,
        `${path}.payload.data.handle`,
      );
      const resultHandle = optionalString(
        data.resultHandle,
        `${path}.payload.data.resultHandle`,
      );
      return {
        ...base,
        kind: "action",
        actionKind: "file_write",
        label: "Changed file",
        target: string(
          data.displayPath,
          `${path}.payload.data.displayPath`,
        ),
        additions: number(
          data.additions,
          `${path}.payload.data.additions`,
        ),
        deletions: number(
          data.deletions,
          `${path}.payload.data.deletions`,
        ),
        diffHandle,
        resultHandle,
      };
    }
    case "compaction": {
      const data = object(payload.data, `${path}.payload.data`, ["reason"]);
      return {
        ...base,
        kind: "action",
        actionKind: "analysis",
        label: "Compacted session context",
        detail: string(data.reason, `${path}.payload.data.reason`),
      };
    }
    case "runOutcome": {
      const data = object(payload.data, `${path}.payload.data`, [
        "outcome",
        "message",
      ]);
      const outcome = enumeration(
        data.outcome,
        `${path}.payload.data.outcome`,
        ["completed", "stopped", "failed"] as const,
      );
      const message = optionalString(
        data.message,
        `${path}.payload.data.message`,
      );
      return {
        ...base,
        kind: "run_outcome",
        outcome: outcome === "completed" ? "done" : outcome,
        durationMs: 0,
        summary: message ?? (outcome === "completed" ? "Run completed" : `Run ${outcome}`),
      };
    }
    case "plan":
    case "source":
    case "artifact":
    case "preview":
      return null;
    default:
      throw new WireContractError(
        `${path}.payload.type`,
        `contains unsupported item payload ${payload.type}`,
      );
  }
}

function projectPlan(value: unknown, path: string): ProgressStep[] {
  const payload = taggedPayload(value, path);
  if (payload.type !== "plan") return [];
  const data = object(payload.data, `${path}.data`, ["steps"]);
  return array(data.steps, `${path}.data.steps`).map((step, index) => {
    const item = object(step, `${path}.data.steps[${index}]`, [
      "id",
      "content",
      "activeForm",
      "state",
    ]);
    const state = enumeration(
      item.state,
      `${path}.data.steps[${index}].state`,
      ["pending", "inProgress", "completed", "blocked"] as const,
    );
    return {
      id: string(item.id, `${path}.data.steps[${index}].id`),
      content: string(
        item.content,
        `${path}.data.steps[${index}].content`,
      ),
      activeForm:
        optionalString(
          item.activeForm,
          `${path}.data.steps[${index}].activeForm`,
        ) ??
        string(item.content, `${path}.data.steps[${index}].content`),
      status:
        state === "inProgress"
          ? "in_progress"
          : state === "completed"
            ? "completed"
            : "pending",
    };
  });
}

function projectPendingRequest(
  value: unknown,
  path: string,
  timestampMs: number,
): TranscriptItem {
  const request = object(value, path, [
    "id",
    "actorGeneration",
    "kind",
    "state",
  ]);
  const id = string(request.id, `${path}.id`);
  const generation = number(
    request.actorGeneration,
    `${path}.actorGeneration`,
  );
  const state = enumeration(request.state, `${path}.state`, [
    "pending",
    "resolved",
    "denied",
    "expired",
  ] as const);
  const kind = taggedPayload(request.kind, `${path}.kind`);
  if (kind.type === "userInput") {
    const data = object(kind.data, `${path}.kind.data`, [
      "prompt",
      "choices",
    ]);
    const choices = array(
      data.choices ?? [],
      `${path}.kind.data.choices`,
    ).map((choice, index) =>
      string(choice, `${path}.kind.data.choices[${index}]`),
    );
    return {
      id: `request-${id}`,
      turnId: `request-${id}`,
      kind: "user_input_request",
      requestId: id,
      prompt: string(data.prompt, `${path}.kind.data.prompt`),
      choices,
      resolved:
        state === "resolved"
          ? "answered"
          : state === "denied" || state === "expired"
            ? "denied"
            : undefined,
      state: state === "pending" ? "streaming" : "committed",
      createdAt: iso(timestampMs),
    };
  }
  if (kind.type !== "approval") {
    throw new WireContractError(
      `${path}.kind.type`,
      `contains unsupported request kind ${kind.type}`,
    );
  }
  const data = object(kind.data, `${path}.kind.data`, ["action", "itemId"]);
  const action = string(data.action, `${path}.kind.data.action`);
  optionalString(data.itemId, `${path}.kind.data.itemId`);
  return {
    id: `request-${id}`,
    turnId: `request-${id}`,
    kind: "approval",
    requestId: id,
    title: `Allow ${action}?`,
    description: action,
    scopeLabel: `Session generation ${generation}`,
    resolved:
      state === "resolved"
        ? "allowed_once"
        : state === "denied" || state === "expired"
          ? "denied"
          : undefined,
    state: state === "pending" ? "streaming" : "committed",
    createdAt: iso(timestampMs),
  };
}

interface SnapshotContext {
  summary?: SessionSummary;
  projectIdFallback?: string;
  timestampMs?: number;
  models?: readonly ModelSummary[];
}

function projectBranchEntry(
  value: unknown,
  path: string,
): SessionBranchEntry {
  const entry = object(value, path, [
    "entryId",
    "parentEntryId",
    "kind",
    "checkoutable",
    "label",
  ]);
  const kind = enumeration(entry.kind, `${path}.kind`, [
    "userMessage",
    "assistantMessage",
    "compaction",
    "internal",
  ] as const);
  const checkoutable = boolean(
    entry.checkoutable,
    `${path}.checkoutable`,
  );
  if (kind === "internal" && checkoutable) {
    throw new WireContractError(
      `${path}.checkoutable`,
      "must be false for internal entries",
    );
  }
  return {
    entryId: boundedString(entry.entryId, `${path}.entryId`, 512),
    parentEntryId:
      entry.parentEntryId === undefined
        ? undefined
        : boundedString(entry.parentEntryId, `${path}.parentEntryId`, 512),
    kind,
    checkoutable,
    label: boundedString(entry.label, `${path}.label`, 256),
  };
}

function projectBranchGraph(
  value: unknown,
  path: string,
  durableHead: string | undefined,
): SessionBranchGraph {
  const graph = object(value, path, ["head", "entries", "truncated"]);
  const head =
    graph.head === undefined
      ? undefined
      : boundedString(graph.head, `${path}.head`, 512);
  if (head !== durableHead) {
    throw new WireContractError(
      `${path}.head`,
      "must match the snapshot durable head",
    );
  }
  const rawEntries = array(graph.entries, `${path}.entries`);
  if (rawEntries.length > 2_048) {
    throw new WireContractError(`${path}.entries`, "has more than 2048 entries");
  }
  const truncated = boolean(graph.truncated, `${path}.truncated`);
  const entries = rawEntries.map((entry, index) =>
    projectBranchEntry(entry, `${path}.entries[${index}]`),
  );
  const ids = new Set<string>();
  for (const entry of entries) {
    if (ids.has(entry.entryId)) {
      throw new WireContractError(
        `${path}.entries`,
        "contains duplicate entry IDs",
      );
    }
    ids.add(entry.entryId);
  }
  if (!truncated) {
    for (const entry of entries) {
      if (entry.parentEntryId !== undefined && !ids.has(entry.parentEntryId)) {
        throw new WireContractError(
          `${path}.entries`,
          "contains a parent outside the preserved graph",
        );
      }
    }
  }
  if (head !== undefined && !ids.has(head)) {
    throw new WireContractError(
      `${path}.head`,
      "must identify a preserved entry",
    );
  }
  return { head, entries, truncated };
}

export function projectSessionSnapshot(
  value: unknown,
  context: SnapshotContext = {},
): SessionSnapshot {
  const snapshot = object(value, "sessionSnapshot", [
    "sessionId",
    "actorGeneration",
    "cursor",
    "durableHead",
    "branches",
    "liveState",
    "activeRunId",
    "model",
    "authority",
    "context",
    "items",
    "pendingRequests",
    "sources",
    "artifacts",
  ]);
  const sessionId = string(snapshot.sessionId, "sessionSnapshot.sessionId");
  const actorGeneration = number(
    snapshot.actorGeneration,
    "sessionSnapshot.actorGeneration",
  );
  const cursor = object(snapshot.cursor, "sessionSnapshot.cursor", [
    "actorGeneration",
    "sequence",
  ]);
  const cursorGeneration = number(
    cursor.actorGeneration,
    "sessionSnapshot.cursor.actorGeneration",
  );
  if (cursorGeneration !== actorGeneration) {
    throw new WireContractError(
      "sessionSnapshot.cursor.actorGeneration",
      "must match the snapshot actor generation",
    );
  }
  const durableHead = optionalString(
    snapshot.durableHead,
    "sessionSnapshot.durableHead",
  );
  const branches = projectBranchGraph(
    snapshot.branches,
    "sessionSnapshot.branches",
    durableHead,
  );
  const activeRunId = optionalString(
    snapshot.activeRunId,
    "sessionSnapshot.activeRunId",
  );
  const model = projectModelSelection(
    snapshot.model,
    "sessionSnapshot.model",
    context.models,
  );
  const contextUsage = object(snapshot.context, "sessionSnapshot.context", [
    "usage",
    "compactions",
  ]);
  const usage = object(contextUsage.usage, "sessionSnapshot.context.usage", [
    "inputTokens",
    "outputTokens",
    "contextTokens",
    "contextLimit",
  ]);
  number(usage.inputTokens, "sessionSnapshot.context.usage.inputTokens");
  number(usage.outputTokens, "sessionSnapshot.context.usage.outputTokens");
  const contextTokens = number(
    usage.contextTokens,
    "sessionSnapshot.context.usage.contextTokens",
  );
  const contextLimit =
    usage.contextLimit === undefined
      ? undefined
      : number(
          usage.contextLimit,
          "sessionSnapshot.context.usage.contextLimit",
        );
  number(contextUsage.compactions, "sessionSnapshot.context.compactions");
  const timestampMs =
    context.timestampMs ??
    (context.summary ? Date.parse(context.summary.updatedAt) : 0);
  const rawItems = array(snapshot.items, "sessionSnapshot.items");
  const items: TranscriptItem[] = [];
  rawItems.forEach((item, index) => {
    const path = `sessionSnapshot.items[${index}]`;
    const wireItem = object(item, path);
    const payload = taggedPayload(wireItem.payload, `${path}.payload`);
    if (payload.type === "toolResult") {
      const result = projectToolResult(payload.data, `${path}.payload.data`);
      const targetIndex = items.findIndex(
        (candidate) =>
          candidate.id === result.toolCallItemId &&
          candidate.kind === "action",
      );
      if (targetIndex !== -1) {
        const target = items[targetIndex];
        if (target?.kind === "action") {
          items[targetIndex] = {
            ...target,
            detail: result.content,
            state: result.failed
              ? "failed"
              : itemState(wireItem.lifecycle, `${path}.lifecycle`),
          };
        }
      }
      return;
    }
    const projected = projectItem(item, path, { timestampMs });
    if (projected) items.push(projected);
  });
  const requests = array(
    snapshot.pendingRequests ?? [],
    "sessionSnapshot.pendingRequests",
  )
    .map((request, index) =>
      projectPendingRequest(
        request,
        `sessionSnapshot.pendingRequests[${index}]`,
        timestampMs,
      ),
    )
    .filter((item): item is TranscriptItem => item !== null);
  const summaryTitle = context.summary?.title ?? "Session";
  const firstUserInput = items.find(
    (item) => item.kind === "user_message",
  );
  const title =
    isUntitledSession(summaryTitle) &&
    firstUserInput?.kind === "user_message"
      ? deriveSessionTitle(
          firstUserInput.content,
          firstUserInput.attachments?.at(0)?.name,
        )
      : summaryTitle;

  const itemSources = rawItems.flatMap((item, index) => {
    const wireItem = object(item, `sessionSnapshot.items[${index}]`);
    const payload = taggedPayload(
      wireItem.payload,
      `sessionSnapshot.items[${index}].payload`,
    );
    return payload.type === "source"
      ? [
          projectSource(
            payload.data,
            `sessionSnapshot.items[${index}].payload.data`,
          ),
        ]
      : [];
  });
  const itemArtifacts = rawItems.flatMap((item, index) => {
    const wireItem = object(item, `sessionSnapshot.items[${index}]`);
    const payload = taggedPayload(
      wireItem.payload,
      `sessionSnapshot.items[${index}].payload`,
    );
    return payload.type === "artifact"
      ? [
          projectArtifact(
            payload.data,
            `sessionSnapshot.items[${index}].payload.data`,
          ),
        ]
      : [];
  });
  const previews = rawItems.flatMap((item, index) => {
    const wireItem = object(item, `sessionSnapshot.items[${index}]`);
    const payload = taggedPayload(
      wireItem.payload,
      `sessionSnapshot.items[${index}].payload`,
    );
    return payload.type === "preview"
      ? [
          projectPreview(
            payload.data,
            `sessionSnapshot.items[${index}].payload.data`,
          ),
        ]
      : [];
  });
  const progress = rawItems.flatMap((item, index) => {
    const wireItem = object(item, `sessionSnapshot.items[${index}]`);
    return projectPlan(
      wireItem.payload,
      `sessionSnapshot.items[${index}].payload`,
    );
  });
  const sources = [
    ...array(snapshot.sources ?? [], "sessionSnapshot.sources").map(
      (source, index) =>
        projectSource(source, `sessionSnapshot.sources[${index}]`),
    ),
    ...itemSources,
  ].filter(
    (source, index, all) =>
      all.findIndex((candidate) => candidate.id === source.id) === index,
  );
  const outputs = [
    ...array(snapshot.artifacts ?? [], "sessionSnapshot.artifacts").map(
      (artifact, index) =>
        projectArtifact(artifact, `sessionSnapshot.artifacts[${index}]`),
    ),
    ...itemArtifacts,
  ].filter(
    (output, index, all) =>
      all.findIndex((candidate) => candidate.id === output.id) === index,
  );

  return {
    sessionId,
    actorGeneration,
    sequence: number(cursor.sequence, "sessionSnapshot.cursor.sequence"),
    title,
    status: projectStatus(
      snapshot.liveState,
      "sessionSnapshot.liveState",
    ),
    activeRunId,
    projectId:
      context.summary?.projectId ?? context.projectIdFallback ?? "",
    modelId: model.model,
    reasoning: model.reasoning,
    authority: projectAuthority(
      snapshot.authority,
      "sessionSnapshot.authority",
    ),
    contextPercent:
      contextLimit && contextLimit > 0
        ? Math.min(100, Math.round((contextTokens / contextLimit) * 100))
        : 0,
    startedAt: context.summary?.updatedAt ?? iso(timestampMs),
    branches,
    items: [...items, ...requests],
    progress,
    sources,
    outputs,
    previews,
  };
}

export interface HostBootstrapProjection {
  bootstrap: HostBootstrap;
  selectedSession: SessionSnapshot;
}

export function projectHostBootstrap(value: unknown): HostBootstrapProjection {
  const wire = object(value, "hostBootstrap", [
    "protocol",
    "host",
    "capabilities",
    "catalogCursor",
    "models",
    "authorityProfiles",
    "authorityCeiling",
    "themes",
    "selectedThemeId",
    "projects",
    "sessions",
    "selectedSessionId",
    "selectedSession",
  ]);
  protocol(wire.protocol, "hostBootstrap.protocol");
  const host = object(wire.host, "hostBootstrap.host", ["id", "name"]);
  const capabilities = object(
    wire.capabilities,
    "hostBootstrap.capabilities",
    [
      "concurrentSessions",
      "opaqueResources",
      "attachments",
      "attachmentPolicy",
      "previews",
      "connectedDevices",
      "lanClients",
      "terminal",
      "childAgents",
      "sessionMetadata",
      "sessionBranches",
      "sessionExport",
    ],
  );
  boolean(
    capabilities.concurrentSessions,
    "hostBootstrap.capabilities.concurrentSessions",
  );
  boolean(
    capabilities.opaqueResources,
    "hostBootstrap.capabilities.opaqueResources",
  );
  boolean(capabilities.terminal, "hostBootstrap.capabilities.terminal");
  boolean(capabilities.childAgents, "hostBootstrap.capabilities.childAgents");
  const sessionMetadata = boolean(
    capabilities.sessionMetadata,
    "hostBootstrap.capabilities.sessionMetadata",
  );
  const sessionBranches = boolean(
    capabilities.sessionBranches,
    "hostBootstrap.capabilities.sessionBranches",
  );
  const sessionExport = boolean(
    capabilities.sessionExport,
    "hostBootstrap.capabilities.sessionExport",
  );
  const attachments = boolean(
    capabilities.attachments,
    "hostBootstrap.capabilities.attachments",
  );
  const attachmentPolicy =
    capabilities.attachmentPolicy === undefined
      ? undefined
      : (() => {
          const policy = object(
            capabilities.attachmentPolicy,
            "hostBootstrap.capabilities.attachmentPolicy",
            [
              "acceptedMediaTypes",
              "maxCount",
              "maxFileBytes",
              "maxTotalBytes",
            ],
          );
          return {
            acceptedMediaTypes: array(
              policy.acceptedMediaTypes,
              "hostBootstrap.capabilities.attachmentPolicy.acceptedMediaTypes",
            ).map((mediaType, index) =>
              boundedString(
                mediaType,
                `hostBootstrap.capabilities.attachmentPolicy.acceptedMediaTypes[${index}]`,
                256,
              ),
            ),
            maxCount: number(
              policy.maxCount,
              "hostBootstrap.capabilities.attachmentPolicy.maxCount",
            ),
            maxFileBytes: number(
              policy.maxFileBytes,
              "hostBootstrap.capabilities.attachmentPolicy.maxFileBytes",
            ),
            maxTotalBytes: number(
              policy.maxTotalBytes,
              "hostBootstrap.capabilities.attachmentPolicy.maxTotalBytes",
            ),
          };
        })();
  const projects = array(wire.projects, "hostBootstrap.projects").map(
    (project, index) => {
      const entry = object(project, `hostBootstrap.projects[${index}]`, [
        "id",
        "name",
        "trusted",
        "sessionCount",
        "liveSessionCount",
      ]);
      number(
        entry.sessionCount,
        `hostBootstrap.projects[${index}].sessionCount`,
      );
      number(
        entry.liveSessionCount,
        `hostBootstrap.projects[${index}].liveSessionCount`,
      );
      const name = string(entry.name, `hostBootstrap.projects[${index}].name`);
      return {
        id: string(entry.id, `hostBootstrap.projects[${index}].id`),
        name,
        pathLabel: name,
        trusted: boolean(
          entry.trusted,
          `hostBootstrap.projects[${index}].trusted`,
        ),
      };
    },
  );
  const models = array(wire.models, "hostBootstrap.models").map(
    (model, index) => projectModel(model, `hostBootstrap.models[${index}]`),
  );
  const sessions = array(wire.sessions, "hostBootstrap.sessions").map(
    (session, index) =>
      projectSummary(session, `hostBootstrap.sessions[${index}]`, models),
  );
  const selectedSessionId = string(
    wire.selectedSessionId,
    "hostBootstrap.selectedSessionId",
  );
  const selectedSummary = sessions.find(
    (session) => session.id === selectedSessionId,
  );
  if (!selectedSummary) {
    throw new WireContractError(
      "hostBootstrap.selectedSessionId",
      "must identify a catalog session",
    );
  }
  const selectedSession = projectSessionSnapshot(wire.selectedSession, {
    summary: selectedSummary,
    projectIdFallback: projects[0]?.id,
    timestampMs: Date.parse(selectedSummary.updatedAt),
    models,
  });
  if (selectedSession.sessionId !== selectedSessionId) {
    throw new WireContractError(
      "hostBootstrap.selectedSession.sessionId",
      "must match selectedSessionId",
    );
  }
  const authorityProfiles = array(
    wire.authorityProfiles,
    "hostBootstrap.authorityProfiles",
  ).map((authority, index) =>
    projectAuthority(
      authority,
      `hostBootstrap.authorityProfiles[${index}]`,
    ),
  );
  projectAuthority(
    wire.authorityCeiling,
    "hostBootstrap.authorityCeiling",
  );
  const themes = array(wire.themes, "hostBootstrap.themes").map(
    (theme, index) =>
      projectThemeOption(theme, `hostBootstrap.themes[${index}]`),
  );
  const selectedThemeId = string(
    wire.selectedThemeId,
    "hostBootstrap.selectedThemeId",
  );
  if (!themes.some((theme) => theme.id === selectedThemeId)) {
    throw new WireContractError(
      "hostBootstrap.selectedThemeId",
      "must identify an advertised theme",
    );
  }
  const bootstrap: HostBootstrap = {
    protocolVersion: PROTOCOL_VERSION,
    host: {
      id: string(host.id, "hostBootstrap.host.id"),
      name: string(host.name, "hostBootstrap.host.name"),
      connection: "local",
    },
    catalogRevision: number(
      wire.catalogCursor,
      "hostBootstrap.catalogCursor",
    ),
    selectedSessionId,
    projects,
    sessions,
    models,
    authorityProfiles,
    themes,
    selectedThemeId,
    devices: [],
    capabilities: {
      attachments,
      attachmentPolicy,
      previews: boolean(
        capabilities.previews,
        "hostBootstrap.capabilities.previews",
      ),
      resources: boolean(
        capabilities.opaqueResources,
        "hostBootstrap.capabilities.opaqueResources",
      ),
      connectedDevices: boolean(
        capabilities.connectedDevices,
        "hostBootstrap.capabilities.connectedDevices",
      ),
      lanClients: boolean(
        capabilities.lanClients,
        "hostBootstrap.capabilities.lanClients",
      ),
      // These describe UI paths that require more than a host capability bit.
      // Keep pairing disabled until an authenticated pairing lifecycle exists.
      attachmentIngest: attachments && attachmentPolicy !== undefined,
      pairDevices: false,
      sessionMetadata,
      sessionBranches,
      sessionExport,
      themeSelection: themes.length > 1,
      steer: true,
      followUp: true,
    },
  };
  return { bootstrap, selectedSession };
}

function resourceEventFromItem(
  itemValue: unknown,
  sessionId: string,
  actorGeneration: number,
  sequence: number,
  timestampMs: number,
  committed: boolean,
): SessionEvent {
  const item = object(itemValue, "event.event.data.item");
  const payload = taggedPayload(
    item.payload,
    "event.event.data.item.payload",
  );
  if (payload.type === "plan") {
    return {
      type: "session.resources",
      sessionId,
      actorGeneration,
      sequence,
      merge: true,
      progress: projectPlan(
        item.payload,
        "event.event.data.item.payload",
      ),
    };
  }
  if (payload.type === "source") {
    return {
      type: "session.resources",
      sessionId,
      actorGeneration,
      sequence,
      merge: true,
      sources: [
        projectSource(payload.data, "event.event.data.item.payload.data"),
      ],
    };
  }
  if (payload.type === "artifact") {
    return {
      type: "session.resources",
      sessionId,
      actorGeneration,
      sequence,
      merge: true,
      outputs: [
        projectArtifact(
          payload.data,
          "event.event.data.item.payload.data",
        ),
      ],
    };
  }
  if (payload.type === "preview") {
    return {
      type: "session.resources",
      sessionId,
      actorGeneration,
      sequence,
      merge: true,
      previews: [
        projectPreview(
          payload.data,
          "event.event.data.item.payload.data",
        ),
      ],
    };
  }
  if (payload.type === "toolResult") {
    const result = projectToolResult(
      payload.data,
      "event.event.data.item.payload.data",
    );
    const itemId = string(
      item.id,
      "event.event.data.item.id",
    );
    return {
      type: "item.tool_result",
      sessionId,
      actorGeneration,
      sequence,
      itemId: result.toolCallItemId,
      resultItemId: itemId,
      detail: result.content,
      state: result.failed
        ? "failed"
        : committed
          ? "committed"
          : "streaming",
    };
  }
  const projected = projectItem(itemValue, "event.event.data.item", {
    timestampMs,
  });
  if (!projected) {
    throw new WireContractError(
      "event.event.data.item.payload",
      "could not be projected",
    );
  }
  return {
    type: committed ? "item.committed" : "item.started",
    sessionId,
    actorGeneration,
    sequence,
    item: projected,
  };
}

export function projectEventEnvelope(
  value: unknown,
  context: Pick<SnapshotContext, "models"> = {},
): SessionEvent {
  const envelope = object(value, "event", [
    "protocol",
    "sessionId",
    "cursor",
    "timestampMs",
    "event",
  ]);
  protocol(envelope.protocol, "event.protocol");
  const sessionId = string(envelope.sessionId, "event.sessionId");
  const cursor = object(envelope.cursor, "event.cursor", [
    "actorGeneration",
    "sequence",
  ]);
  const actorGeneration = number(
    cursor.actorGeneration,
    "event.cursor.actorGeneration",
  );
  const sequence = number(cursor.sequence, "event.cursor.sequence");
  const timestampMs = number(envelope.timestampMs, "event.timestampMs");
  const event = taggedPayload(envelope.event, "event.event");

  switch (event.type) {
    case "session.stateChanged": {
      const data = object(event.data, "event.event.data", [
        "state",
        "activeRunId",
      ]);
      const activeRunId = optionalString(
        data.activeRunId,
        "event.event.data.activeRunId",
      );
      return {
        type: "session.updated",
        sessionId,
        actorGeneration,
        sequence,
        patch: {
          status: projectStatus(
            data.state,
            "event.event.data.state",
          ),
          activeRunId,
        },
      };
    }
    case "session.settingsChanged": {
      const data = object(event.data, "event.event.data", [
        "model",
        "authority",
      ]);
      const model = projectModelSelection(
        data.model,
        "event.event.data.model",
        context.models,
      );
      return {
        type: "session.updated",
        sessionId,
        actorGeneration,
        sequence,
        patch: {
          modelId: model.model,
          reasoning: model.reasoning,
          authority: projectAuthority(
            data.authority,
            "event.event.data.authority",
          ),
        },
      };
    }
    case "session.metadataChanged": {
      const data = object(event.data, "event.event.data", [
        "title",
        "pinned",
        "archived",
      ]);
      const title =
        data.title === undefined
          ? undefined
          : boundedString(data.title, "event.event.data.title", 120);
      if (data.pinned !== undefined) {
        boolean(data.pinned, "event.event.data.pinned");
      }
      if (data.archived !== undefined) {
        boolean(data.archived, "event.event.data.archived");
      }
      return {
        type: "session.updated",
        sessionId,
        actorGeneration,
        sequence,
        patch: title === undefined ? {} : { title },
      };
    }
    case "session.durableHeadChanged": {
      const data = object(event.data, "event.event.data", [
        "durableEntryId",
      ]);
      const durableHead = optionalString(
        data.durableEntryId,
        "event.event.data.durableEntryId",
      );
      return {
        type: "session.durableHeadChanged",
        sessionId,
        actorGeneration,
        sequence,
        durableHead,
      };
    }
    case "session.branchEntriesAppended": {
      const data = object(event.data, "event.event.data", ["entries"]);
      const entries = array(data.entries, "event.event.data.entries");
      if (entries.length === 0 || entries.length > 128) {
        throw new WireContractError(
          "event.event.data.entries",
          "must contain 1 to 128 entries",
        );
      }
      return {
        type: "session.branchEntriesAppended",
        sessionId,
        actorGeneration,
        sequence,
        entries: entries.map((entry, index) =>
          projectBranchEntry(
            entry,
            `event.event.data.entries[${index}]`,
          ),
        ),
      };
    }
    case "session.projectionReplaced": {
      const data = object(event.data, "event.event.data", [
        "durableEntryId",
      ]);
      return {
        type: "session.projectionReplaced",
        sessionId,
        actorGeneration,
        sequence,
        durableHead: optionalString(
          data.durableEntryId,
          "event.event.data.durableEntryId",
        ),
      };
    }
    case "item.started": {
      const data = object(event.data, "event.event.data", ["item"]);
      return resourceEventFromItem(
        data.item,
        sessionId,
        actorGeneration,
        sequence,
        timestampMs,
        false,
      );
    }
    case "item.committed": {
      const data = object(event.data, "event.event.data", ["item"]);
      return resourceEventFromItem(
        data.item,
        sessionId,
        actorGeneration,
        sequence,
        timestampMs,
        true,
      );
    }
    case "item.delta": {
      const data = object(event.data, "event.event.data", [
        "itemId",
        "delta",
      ]);
      const delta = taggedPayload(data.delta, "event.event.data.delta");
      const itemId = string(
        data.itemId,
        "event.event.data.itemId",
      );
      if (delta.type === "assistantText" || delta.type === "reasoningText") {
        const deltaData = object(
          delta.data,
          "event.event.data.delta.data",
          ["append"],
        );
        return {
          type: "item.delta",
          sessionId,
          actorGeneration,
          sequence,
          itemId,
          field: "content",
          delta: string(
            deltaData.append,
            "event.event.data.delta.data.append",
          ),
        };
      }
      if (delta.type === "toolProgress") {
        const deltaData = object(
          delta.data,
          "event.event.data.delta.data",
          ["text", "droppedBytes"],
        );
        number(
          deltaData.droppedBytes,
          "event.event.data.delta.data.droppedBytes",
        );
        return {
          type: "item.delta",
          sessionId,
          actorGeneration,
          sequence,
          itemId,
          field: "detail",
          delta: string(
            deltaData.text,
            "event.event.data.delta.data.text",
          ),
          replace: true,
        };
      }
      throw new WireContractError(
        "event.event.data.delta.type",
        `contains unsupported delta ${delta.type}`,
      );
    }
    case "item.retracted": {
      const data = object(event.data, "event.event.data", [
        "itemId",
        "providerAttempt",
        "reason",
      ]);
      number(
        data.providerAttempt,
        "event.event.data.providerAttempt",
      );
      string(data.reason, "event.event.data.reason");
      return {
        type: "item.retracted",
        sessionId,
        actorGeneration,
        sequence,
        itemId: string(data.itemId, "event.event.data.itemId"),
      };
    }
    case "request.changed": {
      const data = object(event.data, "event.event.data", ["request"]);
      const item = projectPendingRequest(
        data.request,
        "event.event.data.request",
        timestampMs,
      );
      return {
        type: "item.committed",
        sessionId,
        actorGeneration,
        sequence,
        item,
      };
    }
    case "source.upserted": {
      const data = object(event.data, "event.event.data", ["source"]);
      return {
        type: "session.resources",
        sessionId,
        actorGeneration,
        sequence,
        merge: true,
        sources: [
          projectSource(data.source, "event.event.data.source"),
        ],
      };
    }
    case "artifact.upserted": {
      const data = object(event.data, "event.event.data", ["artifact"]);
      return {
        type: "session.resources",
        sessionId,
        actorGeneration,
        sequence,
        merge: true,
        outputs: [
          projectArtifact(data.artifact, "event.event.data.artifact"),
        ],
      };
    }
    case "usage.updated": {
      const data = object(event.data, "event.event.data", ["usage"]);
      const usage = object(data.usage, "event.event.data.usage", [
        "inputTokens",
        "outputTokens",
        "contextTokens",
        "contextLimit",
      ]);
      number(usage.inputTokens, "event.event.data.usage.inputTokens");
      number(usage.outputTokens, "event.event.data.usage.outputTokens");
      const tokens = number(
        usage.contextTokens,
        "event.event.data.usage.contextTokens",
      );
      const limit =
        usage.contextLimit === undefined
          ? undefined
          : number(
              usage.contextLimit,
              "event.event.data.usage.contextLimit",
            );
      return {
        type: "session.updated",
        sessionId,
        actorGeneration,
        sequence,
        patch: {
          contextPercent:
            limit && limit > 0
              ? Math.min(100, Math.round((tokens / limit) * 100))
              : 0,
        },
      };
    }
    default:
      throw new WireContractError(
        "event.event.type",
        `contains unsupported event ${event.type}`,
      );
  }
}

export interface HostStreamProjection {
  hostSequence: number;
  event: HostEvent;
}

export function projectHostStreamEvent(
  value: unknown,
  context: Pick<SnapshotContext, "models"> = {},
): HostStreamProjection {
  const stream = object(value, "hostStreamEvent", [
    "protocol",
    "hostSequence",
    "event",
    "catalog",
  ]);
  protocol(stream.protocol, "hostStreamEvent.protocol");
  const hostSequence = number(
    stream.hostSequence,
    "hostStreamEvent.hostSequence",
  );
  if (stream.event !== undefined && stream.catalog === undefined) {
    return {
      hostSequence,
      event: projectEventEnvelope(stream.event, context),
    };
  }
  if (stream.catalog !== undefined && stream.event === undefined) {
    const catalog = object(stream.catalog, "hostStreamEvent.catalog", [
      "catalogCursor",
      "summary",
    ]);
    return {
      hostSequence,
      event: {
        type: "catalog.summary",
        catalogRevision: number(
          catalog.catalogCursor,
          "hostStreamEvent.catalog.catalogCursor",
        ),
        summary: projectSummary(
          catalog.summary,
          "hostStreamEvent.catalog.summary",
          context.models,
        ),
      },
    };
  }
  throw new WireContractError(
    "hostStreamEvent",
    "requires exactly one event or catalog change",
  );
}

export type ReplayProjection =
  | {
      type: "events";
      actorGeneration: number;
      sequence: number;
      events: SessionEvent[];
    }
  | {
      type: "gap";
      snapshot: SessionSnapshot;
    };

export function projectReplayResponse(
  value: unknown,
  context: SnapshotContext = {},
): ReplayProjection {
  const candidate = object(value, "replay");
  const type = enumeration(candidate.type, "replay.type", [
    "events",
    "gap",
  ] as const);
  if (type === "gap") {
    const replay = object(value, "replay", ["type", "gap", "snapshot"]);
    const gap = object(replay.gap, "replay.gap", [
      "requestedAfter",
      "earliestAvailable",
      "latestAvailable",
    ]);
    for (const key of [
      "requestedAfter",
      "earliestAvailable",
      "latestAvailable",
    ] as const) {
      const cursor = object(gap[key], `replay.gap.${key}`, [
        "actorGeneration",
        "sequence",
      ]);
      number(
        cursor.actorGeneration,
        `replay.gap.${key}.actorGeneration`,
      );
      number(cursor.sequence, `replay.gap.${key}.sequence`);
    }
    return {
      type,
      snapshot: projectSessionSnapshot(replay.snapshot, context),
    };
  }
  const replay = object(value, "replay", [
    "type",
    "after",
    "through",
    "events",
  ]);
  const after = object(replay.after, "replay.after", [
    "actorGeneration",
    "sequence",
  ]);
  const through = object(replay.through, "replay.through", [
    "actorGeneration",
    "sequence",
  ]);
  const actorGeneration = number(
    through.actorGeneration,
    "replay.through.actorGeneration",
  );
  if (
    number(after.actorGeneration, "replay.after.actorGeneration") !==
    actorGeneration
  ) {
    throw new WireContractError(
      "replay.through.actorGeneration",
      "must match the requested cursor",
    );
  }
  const events = array(replay.events, "replay.events").map((event) =>
    projectEventEnvelope(event, context),
  );
  return {
    type,
    actorGeneration,
    sequence: number(through.sequence, "replay.through.sequence"),
    events,
  };
}

function projectSanitizedError(value: unknown, path: string): string {
  const error = object(value, path, [
    "code",
    "message",
    "retryable",
    "currentGeneration",
  ]);
  enumeration(error.code, `${path}.code`, [
    "incompatibleProtocol",
    "invalidCommand",
    "commandIdConflict",
    "staleGeneration",
    "notFound",
    "alreadyResolved",
    "replayGap",
    "payloadTooLarge",
    "unauthorized",
    "invalidBoundary",
    "locked",
    "unavailable",
    "internal",
  ] as const);
  boolean(error.retryable, `${path}.retryable`);
  if (error.currentGeneration !== undefined) {
    number(error.currentGeneration, `${path}.currentGeneration`);
  }
  return string(error.message, `${path}.message`);
}

export function decodeWireCommandAck(value: unknown): CommandAck {
  const candidate = object(value, "commandAck");
  const hostAck = "hostId" in candidate;
  const ack = object(
    value,
    "commandAck",
    hostAck
      ? [
          "protocol",
          "hostId",
          "commandId",
          "acknowledgedAtMs",
          "catalogCursor",
          "disposition",
        ]
      : [
          "protocol",
          "sessionId",
          "commandId",
          "acknowledgedAtMs",
          "cursor",
          "disposition",
        ],
  );
  protocol(ack.protocol, "commandAck.protocol");
  if (hostAck) {
    string(ack.hostId, "commandAck.hostId");
    number(ack.catalogCursor, "commandAck.catalogCursor");
  } else {
    string(ack.sessionId, "commandAck.sessionId");
    const cursor = object(ack.cursor, "commandAck.cursor", [
      "actorGeneration",
      "sequence",
    ]);
    number(cursor.actorGeneration, "commandAck.cursor.actorGeneration");
    number(cursor.sequence, "commandAck.cursor.sequence");
  }
  number(ack.acknowledgedAtMs, "commandAck.acknowledgedAtMs");
  const commandId = string(ack.commandId, "commandAck.commandId");
  const rawDisposition = object(
    ack.disposition,
    "commandAck.disposition",
  );
  const status = enumeration(
    rawDisposition.status,
    "commandAck.disposition.status",
    ["accepted", "rejected"] as const,
  );
  if (status === "rejected") {
    const disposition = object(
      ack.disposition,
      "commandAck.disposition",
      ["status", "error"],
    );
    return {
      commandId,
      accepted: false,
      error: projectSanitizedError(
        disposition.error,
        "commandAck.disposition.error",
      ),
    };
  }
  const disposition = object(
    ack.disposition,
    "commandAck.disposition",
    hostAck ? ["status", "createdSessionId"] : ["status", "runId"],
  );
  if (hostAck) {
    string(
      disposition.createdSessionId,
      "commandAck.disposition.createdSessionId",
    );
  } else {
    optionalString(disposition.runId, "commandAck.disposition.runId");
  }
  return {
    commandId,
    accepted: true,
    createdSessionId: hostAck
      ? string(
          disposition.createdSessionId,
          "commandAck.disposition.createdSessionId",
        )
      : undefined,
  };
}

export interface WireCommandContext {
  hostId: string;
  deviceId: string;
  issuedAtMs: number;
  actorGenerationBySession: Readonly<Record<string, number>>;
  modelIdBySession: Readonly<Record<string, string>>;
  models: readonly ModelSummary[];
}

export type WireCommandEnvelope = JsonObject;

function commandModel(
  modelId: string,
  reasoning: ReasoningEffort,
  context: WireCommandContext,
  commandType: ClientCommand["type"],
) {
  const model = context.models.find((candidate) => candidate.id === modelId);
  if (!model) {
    throw new UnsupportedWireCommandError(
      commandType,
      `model ${modelId} is not in the host catalog`,
    );
  }
  if (!model.available) {
    throw new UnsupportedWireCommandError(
      commandType,
      `model ${modelId} is not currently available`,
    );
  }
  if (!model.reasoning.includes(reasoning)) {
    throw new UnsupportedWireCommandError(
      commandType,
      `reasoning ${reasoning} is not advertised by model ${modelId}`,
    );
  }
  return {
    provider: model.provider,
    model: model.id,
    reasoning,
  };
}

function sessionEnvelope(
  command: ClientCommand & { sessionId: string },
  wireCommand: JsonObject,
  context: WireCommandContext,
): WireCommandEnvelope {
  const generation = context.actorGenerationBySession[command.sessionId];
  if (!generation) {
    throw new UnsupportedWireCommandError(
      command.type,
      "the selected session actor generation is unknown",
    );
  }
  return {
    protocol: PROTOCOL_VERSION.major,
    hostId: context.hostId,
    deviceId: context.deviceId,
    sessionId: command.sessionId,
    commandId: command.id,
    issuedAtMs: context.issuedAtMs,
    expectedActorGeneration: generation,
    command: wireCommand,
  };
}

function encodeAttachments(attachments: AttachmentRef[]) {
  return attachments.map((attachment) => {
    if (!attachment.handle) {
      throw new UnsupportedWireCommandError(
        "session.submit",
        `${attachment.name} has not been ingested by the host`,
      );
    }
    return {
      handle: attachment.handle,
      displayName: attachment.name,
      mediaType: attachment.mediaType,
      byteLen: attachment.size,
    };
  });
}

export function encodeClientCommand(
  command: ClientCommand,
  context: WireCommandContext,
): WireCommandEnvelope {
  if (!context.hostId || !context.deviceId) {
    throw new UnsupportedWireCommandError(
      command.type,
      "host and authenticated device identity are required",
    );
  }
  if (command.type === "session.create") {
    return {
      protocol: PROTOCOL_VERSION.major,
      hostId: context.hostId,
      deviceId: context.deviceId,
      commandId: command.id,
      issuedAtMs: context.issuedAtMs,
      command: {
        type: "host.createSession",
        data: {
          projectId: command.projectId,
          authority: encodeAuthority(command.authority),
          model: commandModel(
            command.modelId,
            command.reasoning,
            context,
            command.type,
          ),
        },
      },
    };
  }
  if (
    command.type === "session.submit" ||
    command.type === "session.steer" ||
    command.type === "session.followUp"
  ) {
    const type =
      command.type === "session.submit"
        ? "session.submitPrompt"
        : command.type === "session.steer"
          ? "session.steer"
          : "session.followUp";
    return sessionEnvelope(
      command,
      {
        type,
        data: {
          input: {
            text: command.prompt,
            attachments: encodeAttachments(command.attachments),
          },
        },
      },
      context,
    );
  }
  if (command.type === "session.interrupt") {
    return sessionEnvelope(
      command,
      { type: "session.abort", data: {} },
      context,
    );
  }
  if (command.type === "approval.resolve") {
    if (command.decision === "allowed_session") {
      throw new UnsupportedWireCommandError(
        command.type,
        "the Rust contract exposes one-shot approval only",
      );
    }
    return sessionEnvelope(
      command,
      {
        type: "session.answerRequest",
        data: {
          requestId: command.requestId,
          answer: {
            type: "approval",
            data: { allowed: command.decision === "allowed_once" },
          },
        },
      },
      context,
    );
  }
  if (command.type === "userInput.resolve") {
    return sessionEnvelope(
      command,
      {
        type: "session.answerRequest",
        data: {
          requestId: command.requestId,
          answer:
            command.answer.type === "text"
              ? {
                  type: "text",
                  data: { text: command.answer.text },
                }
              : {
                  type: "choice",
                  data: { choice: command.answer.choice },
                },
        },
      },
      context,
    );
  }
  if (command.type === "session.configure") {
    const changes = [
      command.modelId !== undefined,
      command.reasoning !== undefined,
      command.authority !== undefined,
    ].filter(Boolean).length;
    if (changes !== 1) {
      throw new UnsupportedWireCommandError(
        command.type,
        "each wire command must change exactly one setting",
      );
    }
    if (command.modelId) {
      const model = context.models.find(
        (candidate) => candidate.id === command.modelId,
      );
      if (!model) {
        throw new UnsupportedWireCommandError(
          command.type,
          `model ${command.modelId} is not in the host catalog`,
        );
      }
      if (!model.available) {
        throw new UnsupportedWireCommandError(
          command.type,
          `model ${command.modelId} is not currently available`,
        );
      }
      return sessionEnvelope(
        command,
        {
          type: "session.changeModel",
          data: { provider: model.provider, model: model.id },
        },
        context,
      );
    }
    if (command.reasoning) {
      const modelId = context.modelIdBySession[command.sessionId];
      const model = context.models.find((candidate) => candidate.id === modelId);
      if (!model || !model.reasoning.includes(command.reasoning)) {
        throw new UnsupportedWireCommandError(
          command.type,
          `reasoning ${command.reasoning} is not advertised by the selected model`,
        );
      }
      return sessionEnvelope(
        command,
        {
          type: "session.changeReasoning",
          data: { reasoning: command.reasoning },
        },
        context,
      );
    }
    return sessionEnvelope(
      command,
      {
        type: "session.setAuthority",
        data: { authority: encodeAuthority(command.authority!) },
      },
      context,
    );
  }
  if (command.type === "session.rename") {
    return sessionEnvelope(
      command,
      {
        type: "session.rename",
        data: { title: command.title },
      },
      context,
    );
  }
  if (command.type === "session.pin") {
    return sessionEnvelope(
      command,
      {
        type: "session.pin",
        data: { pinned: command.pinned },
      },
      context,
    );
  }
  if (command.type === "session.archive") {
    return sessionEnvelope(
      command,
      {
        type: "session.archive",
        data: { archived: command.archived },
      },
      context,
    );
  }
  if (command.type === "session.checkout") {
    return sessionEnvelope(
      command,
      {
        type: "session.checkout",
        data: { entryId: command.entryId },
      },
      context,
    );
  }
  throw new UnsupportedWireCommandError(
    command.type,
    "the authoritative Rust contract has no matching command",
  );
}
