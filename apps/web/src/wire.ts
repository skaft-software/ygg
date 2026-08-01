import { PROTOCOL_VERSION } from "./protocol";
import type {
  ActionPresentation,
  ActionItem,
  ActivityPhaseSummary,
  AttachmentRef,
  AuthorityProfile,
  ClientCommand,
  CommandAck,
  CommandDiscovery,
  CommandSuggestion,
  ContextStatus,
  ContextTotals,
  ContextUsage,
  CompletionReview,
  DocumentReference,
  HostBootstrap,
  HostEvent,
  LifetimeUsage,
  ModelSummary,
  ModelUsage,
  OutputRef,
  PreviewRef,
  ProgressStep,
  ProjectCatalog,
  ProjectFileGitStatus,
  ProjectFileRead,
  ProjectFileSearchResult,
  ProjectFileTree,
  ProjectFileWrite,
  ProjectSummary,
  RepositoryContextSnapshot,
  ReasoningEffort,
  SessionBranchEntry,
  SessionBranchGraph,
  SessionEvent,
  SessionSnapshot,
  SessionStatus,
  SessionSummary,
  SkillSuggestion,
  SourceRef,
  ThemeColor,
  ThemeDto,
  ThemeOption,
  TranscriptSearchResult,
  TranscriptItem,
  TrustedFileCatalog,
  TrustedFileEntry,
  TrustedFileRead,
  TrustedFileSearchResult,
  UsageActivity,
  UsageSnapshot,
  UsageStats,
  UsageTotals,
} from "./protocol";

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

function integer(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw new WireContractError(path, "must be a safe integer");
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

function projectPathSegment(value: string, path: string): string {
  if (
    !value ||
    value === "." ||
    value === ".." ||
    [...value].some(
      (character) =>
        character <= "\u001f" ||
        character === "\u007f" ||
        character === "/" ||
        character === "\\" ||
        (character >= "\u200b" && character <= "\u200f") ||
        (character >= "\u202a" && character <= "\u202e") ||
        (character >= "\u2066" && character <= "\u2069") ||
        character === "\ufeff",
    )
  ) {
    throw new WireContractError(path, "must be a safe project-relative path");
  }
  return value;
}

function projectRelativePath(
  value: unknown,
  path: string,
  allowRoot = false,
): string {
  const decoded = boundedString(value, path, 2_048, allowRoot);
  if (!decoded && allowRoot) return decoded;
  for (const segment of decoded.split("/")) {
    projectPathSegment(segment, path);
  }
  return decoded;
}

function projectFileSha256(value: unknown, path: string): string {
  const decoded = boundedString(value, path, 64);
  if (!/^[a-f0-9]{64}$/u.test(decoded)) {
    throw new WireContractError(path, "must be a lowercase SHA-256 digest");
  }
  return decoded;
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

function projectInputPricing(
  value: unknown,
  path: string,
): NonNullable<ModelSummary["inputPricing"]> {
  const pricing = object(value, path, [
    "baseMicrodollarsPerMillionTokens",
    "tiers",
  ]);
  const rawTiers = array(pricing.tiers, `${path}.tiers`);
  if (rawTiers.length > 32) {
    throw new WireContractError(`${path}.tiers`, "has more than 32 tiers");
  }
  const tiers = rawTiers.map((value, index) => {
    const tierPath = `${path}.tiers[${index}]`;
    const tier = object(value, tierPath, [
      "minInputTokens",
      "microdollarsPerMillionTokens",
    ]);
    return {
      minInputTokens: number(tier.minInputTokens, `${tierPath}.minInputTokens`),
      microdollarsPerMillionTokens: number(
        tier.microdollarsPerMillionTokens,
        `${tierPath}.microdollarsPerMillionTokens`,
      ),
    };
  });
  if (
    tiers.some(
      (tier, index) =>
        index > 0 && tier.minInputTokens <= tiers[index - 1]!.minInputTokens,
    )
  ) {
    throw new WireContractError(
      `${path}.tiers`,
      "must have strictly ascending input thresholds",
    );
  }
  return {
    baseMicrodollarsPerMillionTokens: number(
      pricing.baseMicrodollarsPerMillionTokens,
      `${path}.baseMicrodollarsPerMillionTokens`,
    ),
    tiers,
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
    "inputPricing",
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
  const inputPricing =
    model.inputPricing === undefined
      ? undefined
      : projectInputPricing(model.inputPricing, `${path}.inputPricing`);
  return {
    id: boundedString(model.id, `${path}.id`, 256),
    name: boundedString(model.name, `${path}.name`, 256),
    provider: boundedString(model.provider, `${path}.provider`, 128),
    local: boolean(model.local, `${path}.local`),
    available: boolean(model.available, `${path}.available`),
    reasoning: efforts,
    defaultReasoning,
    inputPricing,
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

function projectConversationBranchProvenance(
  value: unknown,
  path: string,
  models?: readonly ModelSummary[],
) {
  const provenance = object(value, path, [
    "operation",
    "sourceSessionId",
    "sourceEntryId",
    "originatingUserEntryId",
    "modelOverride",
    "externalEffectsPreserved",
    "warning",
  ]);
  const externalEffectsPreserved = boolean(
    provenance.externalEffectsPreserved,
    `${path}.externalEffectsPreserved`,
  );
  if (!externalEffectsPreserved) {
    throw new WireContractError(
      `${path}.externalEffectsPreserved`,
      "must explicitly preserve external effects",
    );
  }
  return {
    operation: enumeration(provenance.operation, `${path}.operation`, [
      "editUserTurn",
      "retryResponse",
      "forkSession",
    ] as const),
    sourceSessionId: boundedString(
      provenance.sourceSessionId,
      `${path}.sourceSessionId`,
      256,
    ),
    sourceEntryId: boundedString(
      provenance.sourceEntryId,
      `${path}.sourceEntryId`,
      256,
    ),
    originatingUserEntryId: optionalString(
      provenance.originatingUserEntryId,
      `${path}.originatingUserEntryId`,
    ),
    modelOverride:
      provenance.modelOverride === undefined
        ? undefined
        : projectModelSelection(
            provenance.modelOverride,
            `${path}.modelOverride`,
            models,
          ),
    externalEffectsPreserved: true as const,
    warning: boundedString(provenance.warning, `${path}.warning`, 2_048),
  };
}

function projectPullRequest(
  value: unknown,
  path: string,
): NonNullable<SessionSummary["pullRequest"]> {
  const pullRequest = object(value, path, ["state"]);
  const state = enumeration(pullRequest.state, `${path}.state`, [
    "inProgress",
    "ready",
    "merged",
  ] as const);
  return { state: state === "inProgress" ? "in_progress" : state };
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
    "lifecycle",
    "retention",
    "forkedFrom",
    "provisional",
    "liveState",
    "attention",
    "pullRequest",
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
  const archived = boolean(summary.archived, `${path}.archived`);
  const lifecycle =
    summary.lifecycle === undefined
      ? archived
        ? "archived"
        : "active"
      : enumeration(summary.lifecycle, `${path}.lifecycle`, [
          "active",
          "archived",
          "trash",
        ] as const);
  if (archived !== (lifecycle !== "active")) {
    throw new WireContractError(
      `${path}.lifecycle`,
      "must agree with the archived compatibility field",
    );
  }
  const retention =
    summary.retention === undefined
      ? undefined
      : (() => {
          const value = object(summary.retention, `${path}.retention`, [
            "trashedAtMs",
            "purgeAfterMs",
            "permanentDeleteRequiresConfirmation",
          ]);
          const trashedAtMs = number(
            value.trashedAtMs,
            `${path}.retention.trashedAtMs`,
          );
          const purgeAfterMs = number(
            value.purgeAfterMs,
            `${path}.retention.purgeAfterMs`,
          );
          if (purgeAfterMs <= trashedAtMs) {
            throw new WireContractError(
              `${path}.retention.purgeAfterMs`,
              "must be after trashedAtMs",
            );
          }
          if (
            boolean(
              value.permanentDeleteRequiresConfirmation,
              `${path}.retention.permanentDeleteRequiresConfirmation`,
            ) !== true
          ) {
            throw new WireContractError(
              `${path}.retention.permanentDeleteRequiresConfirmation`,
              "must remain enabled",
            );
          }
          return {
            trashedAtMs,
            purgeAfterMs,
            permanentDeleteRequiresConfirmation: true as const,
          };
        })();
  if ((lifecycle === "trash") !== (retention !== undefined)) {
    throw new WireContractError(
      `${path}.retention`,
      "must be present exactly for trashed sessions",
    );
  }
  const forkedFrom =
    summary.forkedFrom === undefined
      ? undefined
      : projectConversationBranchProvenance(
          summary.forkedFrom,
          `${path}.forkedFrom`,
          models,
        );
  if (forkedFrom && forkedFrom.operation !== "forkSession") {
    throw new WireContractError(
      `${path}.forkedFrom.operation`,
      "must identify a new-session fork",
    );
  }
  return {
    id: string(summary.id, `${path}.id`),
    projectId:
      optionalString(summary.projectId, `${path}.projectId`) ?? "",
    title: string(summary.title, `${path}.title`),
    preview: summaryPreview(status, attention),
    status,
    updatedAt: iso(number(summary.modifiedAtMs, `${path}.modifiedAtMs`)),
    pinned: boolean(summary.pinned, `${path}.pinned`),
    archived,
    lifecycle,
    retention,
    forkedFrom,
    pullRequest:
      summary.pullRequest === undefined
        ? undefined
        : projectPullRequest(summary.pullRequest, `${path}.pullRequest`),
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

export function projectDocumentReference(
  value: unknown,
  path = "document",
): DocumentReference {
  const document = object(value, path, [
    "id",
    "displayName",
    "mediaType",
    "sourceByteCount",
    "extractedTextByteCount",
    "sha256",
    "fidelity",
    "pageCount",
    "createdAtMs",
  ]);
  return {
    id: boundedString(document.id, `${path}.id`, 128),
    displayName: boundedString(
      document.displayName,
      `${path}.displayName`,
      512,
    ),
    mediaType: enumeration(document.mediaType, `${path}.mediaType`, [
      "text/plain",
      "text/markdown",
      "application/pdf",
    ] as const),
    sourceByteCount: number(
      document.sourceByteCount,
      `${path}.sourceByteCount`,
    ),
    extractedTextByteCount: number(
      document.extractedTextByteCount,
      `${path}.extractedTextByteCount`,
    ),
    sha256: boundedString(document.sha256, `${path}.sha256`, 64),
    fidelity: enumeration(document.fidelity, `${path}.fidelity`, [
      "exactUtf8",
      "pdfTextOnlyPartial",
    ] as const),
    pageCount:
      document.pageCount === undefined
        ? undefined
        : number(document.pageCount, `${path}.pageCount`),
    createdAtMs: number(document.createdAtMs, `${path}.createdAtMs`),
  };
}

export function projectTrustedFileEntry(
  value: unknown,
  path = "trustedFile",
): TrustedFileEntry {
  const entry = object(value, path, [
    "id",
    "relativePath",
    "displayName",
    "kind",
    "byteLen",
  ]);
  return {
    id: boundedString(entry.id, `${path}.id`, 128),
    relativePath: boundedString(
      entry.relativePath,
      `${path}.relativePath`,
      2_048,
    ),
    displayName: boundedString(
      entry.displayName,
      `${path}.displayName`,
      512,
    ),
    kind: enumeration(entry.kind, `${path}.kind`, [
      "documentation",
      "source",
      "configuration",
      "text",
    ] as const),
    byteLen: number(entry.byteLen, `${path}.byteLen`),
  };
}

export function projectTrustedFileCatalog(
  value: unknown,
): TrustedFileCatalog {
  const catalog = object(value, "trustedFileCatalog", [
    "protocol",
    "summary",
    "files",
  ]);
  protocol(catalog.protocol, "trustedFileCatalog.protocol");
  const summary = object(catalog.summary, "trustedFileCatalog.summary", [
    "indexedFiles",
    "ignoredEntries",
    "truncated",
  ]);
  return {
    summary: {
      indexedFiles: number(
        summary.indexedFiles,
        "trustedFileCatalog.summary.indexedFiles",
      ),
      ignoredEntries: number(
        summary.ignoredEntries,
        "trustedFileCatalog.summary.ignoredEntries",
      ),
      truncated: boolean(
        summary.truncated,
        "trustedFileCatalog.summary.truncated",
      ),
    },
    files: array(catalog.files, "trustedFileCatalog.files").map(
      (entry, index) =>
        projectTrustedFileEntry(
          entry,
          `trustedFileCatalog.files[${index}]`,
        ),
    ),
  };
}

export function projectTrustedFileSearchResult(
  value: unknown,
): TrustedFileSearchResult {
  const result = object(value, "trustedFileSearch", [
    "hits",
    "truncated",
    "scannedBytes",
  ]);
  return {
    hits: array(result.hits, "trustedFileSearch.hits").map((value, index) => {
      const hit = object(value, `trustedFileSearch.hits[${index}]`, [
        "entry",
        "snippet",
        "line",
      ]);
      return {
        entry: projectTrustedFileEntry(
          hit.entry,
          `trustedFileSearch.hits[${index}].entry`,
        ),
        snippet: boundedString(
          hit.snippet,
          `trustedFileSearch.hits[${index}].snippet`,
          480,
        ),
        line:
          hit.line === undefined
            ? undefined
            : number(hit.line, `trustedFileSearch.hits[${index}].line`),
      };
    }),
    truncated: boolean(result.truncated, "trustedFileSearch.truncated"),
    scannedBytes: number(
      result.scannedBytes,
      "trustedFileSearch.scannedBytes",
    ),
  };
}

export function projectTrustedFileRead(value: unknown): TrustedFileRead {
  const read = object(value, "trustedFileRead", ["entry", "text", "sha256"]);
  return {
    entry: projectTrustedFileEntry(read.entry, "trustedFileRead.entry"),
    text: boundedString(read.text, "trustedFileRead.text", 1024 * 1024),
    sha256: boundedString(read.sha256, "trustedFileRead.sha256", 64),
  };
}


function projectFileGitStatus(
  value: unknown,
  path: string,
): ProjectFileGitStatus {
  const status = object(value, path, ["kind", "oldPath"]);
  const oldPath =
    status.oldPath === undefined
      ? undefined
      : projectRelativePath(status.oldPath, `${path}.oldPath`);
  return {
    kind: enumeration(status.kind, `${path}.kind`, [
      "modified",
      "added",
      "deleted",
      "renamed",
      "untracked",
    ] as const),
    oldPath,
  };
}

export function projectProjectFileTree(value: unknown): ProjectFileTree {
  const tree = object(value, "projectFileTree", [
    "path",
    "entries",
    "truncated",
    "gitStatusTruncated",
  ]);
  const entries = array(tree.entries, "projectFileTree.entries");
  if (entries.length > 1_000) {
    throw new WireContractError(
      "projectFileTree.entries",
      "must contain at most 1000 entries",
    );
  }
  return {
    path: projectRelativePath(tree.path, "projectFileTree.path", true),
    entries: entries.map((value, index) => {
      const entryPath = `projectFileTree.entries[${index}]`;
      const entry = object(value, entryPath, [
        "name",
        "kind",
        "size",
        "modifiedAtMs",
        "gitStatus",
      ]);
      const gitStatusValues =
        entry.gitStatus === undefined
          ? undefined
          : array(entry.gitStatus, `${entryPath}.gitStatus`);
      if (gitStatusValues && gitStatusValues.length > 5) {
        throw new WireContractError(
          `${entryPath}.gitStatus`,
          "must contain at most 5 statuses",
        );
      }
      const gitStatus = gitStatusValues?.map((status, statusIndex) =>
        projectFileGitStatus(
          status,
          `${entryPath}.gitStatus[${statusIndex}]`,
        ),
      );
      const parsedEntry = {
        name: projectPathSegment(
          boundedString(entry.name, `${entryPath}.name`, 2_048),
          `${entryPath}.name`,
        ),
        kind: enumeration(entry.kind, `${entryPath}.kind`, [
          "directory",
          "file",
        ] as const),
        size: number(entry.size, `${entryPath}.size`),
        modifiedAtMs:
          entry.modifiedAtMs === undefined
            ? undefined
            : number(entry.modifiedAtMs, `${entryPath}.modifiedAtMs`),
      };
      return gitStatus === undefined
        ? parsedEntry
        : { ...parsedEntry, gitStatus };
    }),
    truncated: boolean(tree.truncated, "projectFileTree.truncated"),
    gitStatusTruncated:
      tree.gitStatusTruncated === undefined
        ? false
        : boolean(tree.gitStatusTruncated, "projectFileTree.gitStatusTruncated"),
  };
}

export function projectProjectFileRead(value: unknown): ProjectFileRead {
  const read = object(value, "projectFileRead", [
    "path",
    "content",
    "startLine",
    "endLine",
    "lineCount",
    "truncated",
    "sha256",
  ]);
  const startLine = number(read.startLine, "projectFileRead.startLine");
  const endLine = number(read.endLine, "projectFileRead.endLine");
  const lineCount = number(read.lineCount, "projectFileRead.lineCount");
  if (startLine > endLine || endLine > lineCount) {
    throw new WireContractError(
      "projectFileRead",
      "has an invalid line range",
    );
  }
  return {
    path: projectRelativePath(read.path, "projectFileRead.path"),
    content: boundedString(
      read.content,
      "projectFileRead.content",
      1024 * 1024,
      true,
    ),
    startLine,
    endLine,
    lineCount,
    truncated: boolean(read.truncated, "projectFileRead.truncated"),
    sha256:
      read.sha256 === undefined
        ? undefined
        : projectFileSha256(read.sha256, "projectFileRead.sha256"),
  };
}

export function projectProjectFileSearchResult(
  value: unknown,
): ProjectFileSearchResult {
  const result = object(value, "projectFileSearch", [
    "hits",
    "truncated",
    "scannedBytes",
  ]);
  const hits = array(result.hits, "projectFileSearch.hits");
  if (hits.length > 100) {
    throw new WireContractError(
      "projectFileSearch.hits",
      "must contain at most 100 hits",
    );
  }
  return {
    hits: hits.map((value, index) => {
      const hitPath = `projectFileSearch.hits[${index}]`;
      const hit = object(value, hitPath, ["path", "line", "snippet"]);
      const line =
        hit.line === undefined ? undefined : number(hit.line, `${hitPath}.line`);
      if (line === 0) {
        throw new WireContractError(`${hitPath}.line`, "must be positive");
      }
      return {
        path: projectRelativePath(hit.path, `${hitPath}.path`),
        line,
        snippet: boundedString(hit.snippet, `${hitPath}.snippet`, 480, true),
      };
    }),
    truncated: boolean(result.truncated, "projectFileSearch.truncated"),
    scannedBytes: number(result.scannedBytes, "projectFileSearch.scannedBytes"),
  };
}

export function projectProjectFileWrite(value: unknown): ProjectFileWrite {
  const write = object(value, "projectFileWrite", [
    "path",
    "sha256",
    "modifiedAtMs",
  ]);
  return {
    path: projectRelativePath(write.path, "projectFileWrite.path"),
    sha256: projectFileSha256(write.sha256, "projectFileWrite.sha256"),
    modifiedAtMs:
      write.modifiedAtMs === undefined
        ? undefined
        : number(write.modifiedAtMs, "projectFileWrite.modifiedAtMs"),
  };
}

function commandDiscoveryIdentifier(
  value: unknown,
  path: string,
  maxLength: number,
): string {
  const identifier = boundedString(value, path, maxLength);
  if (/\s/.test(identifier)) {
    throw new WireContractError(path, "must not contain whitespace");
  }
  return identifier;
}

export function projectCommandDiscovery(value: unknown): CommandDiscovery {
  const discovery = object(value, "commandDiscovery", [
    "protocol",
    "commands",
    "skills",
  ]);
  protocol(discovery.protocol, "commandDiscovery.protocol");
  const commandValues = array(discovery.commands, "commandDiscovery.commands");
  const skillValues = array(discovery.skills, "commandDiscovery.skills");
  if (commandValues.length > 512 || skillValues.length > 512) {
    throw new WireContractError("commandDiscovery", "exceeds the discovery limit");
  }
  const commandNames = new Set<string>();
  const commands = commandValues.map((value, index): CommandSuggestion => {
    const path = `commandDiscovery.commands[${index}]`;
    const command = object(value, path, [
      "name",
      "usage",
      "description",
      "argumentHint",
      "acceptsArgument",
      "kind",
    ]);
    const name = commandDiscoveryIdentifier(command.name, `${path}.name`, 128);
    if (commandNames.has(name)) {
      throw new WireContractError(`${path}.name`, "is duplicated");
    }
    commandNames.add(name);
    const usage = boundedString(command.usage, `${path}.usage`, 512);
    if (!usage.startsWith("/")) {
      throw new WireContractError(`${path}.usage`, "must begin with a slash");
    }
    return {
      name,
      usage,
      description: boundedString(command.description, `${path}.description`, 2_048),
      argumentHint:
        command.argumentHint === undefined
          ? undefined
          : boundedString(command.argumentHint, `${path}.argumentHint`, 512),
      acceptsArgument: boolean(command.acceptsArgument, `${path}.acceptsArgument`),
      kind: enumeration(command.kind, `${path}.kind`, [
        "builtIn",
        "prompt",
        "extension",
      ] as const),
    };
  });
  const skillIds = new Set<string>();
  const skills = skillValues.map((value, index): SkillSuggestion => {
    const path = `commandDiscovery.skills[${index}]`;
    const skill = object(value, path, ["id", "name", "description", "active"]);
    const id = commandDiscoveryIdentifier(skill.id, `${path}.id`, 128);
    if (skillIds.has(id)) {
      throw new WireContractError(`${path}.id`, "is duplicated");
    }
    skillIds.add(id);
    return {
      id,
      name: boundedString(skill.name, `${path}.name`, 256),
      description: boundedString(skill.description, `${path}.description`, 2_048),
      active: boolean(skill.active, `${path}.active`),
    };
  });
  return { commands, skills };

}

export function projectTranscriptSearchResult(
  value: unknown,
): TranscriptSearchResult {
  const result = object(value, "transcriptSearch", ["hits", "truncated"]);
  return {
    hits: array(result.hits, "transcriptSearch.hits").map((value, index) => {
      const path = `transcriptSearch.hits[${index}]`;
      const hit = object(value, path, [
        "sessionId",
        "itemId",
        "kind",
        "sessionTitle",
        "snippet",
        "matchRanges",
        "titleMatchRanges",
        "timestampMs",
        "score",
      ]);
      const ranges = (value: unknown, rangePath: string) =>
        array(value, rangePath).map((value, rangeIndex) => {
          const range = object(value, `${rangePath}[${rangeIndex}]`, [
            "startChar",
            "endChar",
          ]);
          return {
            startChar: number(
              range.startChar,
              `${rangePath}[${rangeIndex}].startChar`,
            ),
            endChar: number(
              range.endChar,
              `${rangePath}[${rangeIndex}].endChar`,
            ),
          };
        });
      return {
        sessionId: boundedString(hit.sessionId, `${path}.sessionId`, 256),
        itemId: boundedString(hit.itemId, `${path}.itemId`, 512),
        kind: enumeration(hit.kind, `${path}.kind`, [
          "user",
          "assistant",
          "tool",
          "error",
          "attachment",
        ] as const),
        sessionTitle: boundedString(
          hit.sessionTitle,
          `${path}.sessionTitle`,
          512,
        ),
        snippet: boundedString(hit.snippet, `${path}.snippet`, 1_024),
        matchRanges: ranges(hit.matchRanges, `${path}.matchRanges`),
        titleMatchRanges: ranges(
          hit.titleMatchRanges,
          `${path}.titleMatchRanges`,
        ),
        timestampMs: number(hit.timestampMs, `${path}.timestampMs`),
        score: number(hit.score, `${path}.score`),
      };
    }),
    truncated: boolean(result.truncated, "transcriptSearch.truncated"),
  };
}

function itemState(value: unknown, path: string) {
  const lifecycle = enumeration(value, path, [
    "provisional",
    "committed",
  ] as const);
  return lifecycle === "provisional" ? ("streaming" as const) : ("committed" as const);
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

function stringArray(value: unknown, path: string): string[] {
  return array(value ?? [], path).map((entry, index) =>
    string(entry, `${path}[${index}]`),
  );
}

function projectToolActivity(
  value: unknown,
  path: string,
): ActionPresentation {
  const data = object(value, path, [
    "rawToolName",
    "kind",
    "phase",
    "status",
    "title",
    "summary",
    "target",
    "cwd",
    "commandPreview",
    "exitCode",
    "signal",
    "startedAtMs",
    "completedAtMs",
    "durationMs",
    "outputSummary",
    "outputHandle",
    "observedOutputBytes",
    "droppedOutputBytes",
    "changedPaths",
    "sourceIds",
    "artifactIds",
  ]);
  const kind = enumeration(data.kind, `${path}.kind`, [
    "read",
    "search",
    "edit",
    "write",
    "command",
    "web",
    "skill",
    "other",
  ] as const);
  const actionKind: ActionItem["actionKind"] =
    kind === "read"
      ? "file_read"
      : kind === "search"
        ? "file_search"
        : kind === "edit" || kind === "write"
          ? "file_write"
          : kind === "command"
            ? "command"
            : kind === "web"
              ? "web_search"
              : kind === "skill"
                ? "skill"
                : "analysis";
  const status = enumeration(data.status, `${path}.status`, [
    "running",
    "succeeded",
    "failed",
    "stopped",
  ] as const);
  const startedAtMs = number(data.startedAtMs, `${path}.startedAtMs`);
  const completedAtMs =
    data.completedAtMs === undefined
      ? undefined
      : number(data.completedAtMs, `${path}.completedAtMs`);
  const durationMs =
    data.durationMs === undefined
      ? undefined
      : number(data.durationMs, `${path}.durationMs`);
  const summary = optionalString(data.summary, `${path}.summary`);
  const outputSummary = optionalString(
    data.outputSummary,
    `${path}.outputSummary`,
  );
  return {
    actionKind,
    phase: enumeration(data.phase, `${path}.phase`, [
      "investigated",
      "changed",
      "verified",
      "produced",
      "other",
    ] as const),
    status,
    rawToolName: string(data.rawToolName, `${path}.rawToolName`),
    label: string(data.title, `${path}.title`),
    summary,
    target: optionalString(data.target, `${path}.target`),
    detail: outputSummary ?? summary,
    cwd: optionalString(data.cwd, `${path}.cwd`),
    commandPreview: optionalString(
      data.commandPreview,
      `${path}.commandPreview`,
    ),
    exitCode:
      data.exitCode === undefined
        ? undefined
        : integer(data.exitCode, `${path}.exitCode`),
    signal:
      data.signal === undefined
        ? undefined
        : integer(data.signal, `${path}.signal`),
    startedAt: iso(startedAtMs),
    completedAt:
      completedAtMs === undefined ? undefined : iso(completedAtMs),
    durationMs,
    outputSummary,
    outputHandle: optionalString(
      data.outputHandle,
      `${path}.outputHandle`,
    ),
    observedOutputBytes: number(
      data.observedOutputBytes ?? 0,
      `${path}.observedOutputBytes`,
    ),
    droppedOutputBytes: number(
      data.droppedOutputBytes ?? 0,
      `${path}.droppedOutputBytes`,
    ),
    changedPaths: stringArray(
      data.changedPaths,
      `${path}.changedPaths`,
    ),
    sourceIds: stringArray(data.sourceIds, `${path}.sourceIds`),
    outputIds: stringArray(data.artifactIds, `${path}.artifactIds`),
  };
}

interface ToolResultProjection {
  toolCallItemId: string;
  result: Extract<
    SessionEvent,
    { type: "item.activity_result" }
  >["result"];
}

function projectToolResult(
  value: unknown,
  path: string,
): ToolResultProjection {
  const data = object(value, path, [
    "toolCallItemId",
    "status",
    "summary",
    "outputSummary",
    "outputHandle",
    "exitCode",
    "signal",
    "completedAtMs",
    "durationMs",
    "observedOutputBytes",
    "droppedOutputBytes",
  ]);
  const status = enumeration(data.status, `${path}.status`, [
    "running",
    "succeeded",
    "failed",
    "stopped",
  ] as const);
  return {
    toolCallItemId: string(
      data.toolCallItemId,
      `${path}.toolCallItemId`,
    ),
    result: {
      status,
      summary: string(data.summary, `${path}.summary`),
      exitCode:
        data.exitCode === undefined
          ? undefined
          : integer(data.exitCode, `${path}.exitCode`),
      signal:
        data.signal === undefined
          ? undefined
          : integer(data.signal, `${path}.signal`),
      completedAt: iso(
        number(data.completedAtMs, `${path}.completedAtMs`),
      ),
      durationMs: number(data.durationMs, `${path}.durationMs`),
      outputSummary: optionalString(
        data.outputSummary,
        `${path}.outputSummary`,
      ),
      outputHandle: optionalString(
        data.outputHandle,
        `${path}.outputHandle`,
      ),
      observedOutputBytes: number(
        data.observedOutputBytes ?? 0,
        `${path}.observedOutputBytes`,
      ),
      droppedOutputBytes: number(
        data.droppedOutputBytes ?? 0,
        `${path}.droppedOutputBytes`,
      ),
    },
  };
}

function projectPhaseSummary(
  value: unknown,
  path: string,
): ActivityPhaseSummary {
  const phase = object(value, path, [
    "phase",
    "actionCount",
    "succeededCount",
    "failedCount",
    "stoppedCount",
  ]);
  return {
    phase: enumeration(phase.phase, `${path}.phase`, [
      "investigated",
      "changed",
      "verified",
      "produced",
      "other",
    ] as const),
    actionCount: number(phase.actionCount, `${path}.actionCount`),
    succeededCount: number(
      phase.succeededCount,
      `${path}.succeededCount`,
    ),
    failedCount: number(phase.failedCount, `${path}.failedCount`),
    stoppedCount: number(phase.stoppedCount, `${path}.stoppedCount`),
  };
}

function projectReportedTestCounts(
  value: unknown,
  path: string,
): import("./protocol").ReportedTestCounts {
  const counts = object(value, path, [
    "total",
    "passed",
    "failed",
    "skipped",
    "errors",
  ]);
  const optionalCount = (field: keyof typeof counts) =>
    counts[field] === undefined
      ? undefined
      : number(counts[field], `${path}.${field}`);
  return {
    total: optionalCount("total"),
    passed: optionalCount("passed"),
    failed: optionalCount("failed"),
    skipped: optionalCount("skipped"),
    errors: optionalCount("errors"),
  };
}

function projectStructuredTestResults(
  value: unknown,
  path: string,
): import("./protocol").StructuredTestResults {
  const result = object(value, path, [
    "originItemId",
    "framework",
    "parser",
    "command",
    "verification",
    "reported",
    "reportedSuites",
    "summaryCount",
    "suites",
    "coverage",
  ]);
  const command = object(result.command, `${path}.command`, [
    "status",
    "exitCode",
    "signal",
  ]);
  const coverage = object(result.coverage, `${path}.coverage`, [
    "inputTruncated",
    "recordsTruncated",
    "unsupportedSummaryFields",
    "summaries",
    "cases",
  ]);
  return {
    originItemId: string(result.originItemId, `${path}.originItemId`),
    framework: enumeration(result.framework, `${path}.framework`, [
      "cargoLibtest",
      "vitest",
      "jest",
      "pytest",
      "goTest",
    ] as const),
    parser: enumeration(result.parser, `${path}.parser`, [
      "cargoLibtestTextV1",
      "vitestTextV1",
      "jestTextV1",
      "pytestTextV1",
      "goTestTextV1",
    ] as const),
    command: {
      status: enumeration(command.status, `${path}.command.status`, [
        "succeeded",
        "failed",
        "stopped",
      ] as const),
      exitCode:
        command.exitCode === undefined
          ? undefined
          : integer(command.exitCode, `${path}.command.exitCode`),
      signal:
        command.signal === undefined
          ? undefined
          : integer(command.signal, `${path}.command.signal`),
    },
    verification: enumeration(
      result.verification,
      `${path}.verification`,
      ["passed", "failed", "stopped", "inconclusive"] as const,
    ),
    reported: projectReportedTestCounts(
      result.reported,
      `${path}.reported`,
    ),
    reportedSuites: projectReportedTestCounts(
      result.reportedSuites,
      `${path}.reportedSuites`,
    ),
    summaryCount: number(result.summaryCount, `${path}.summaryCount`),
    suites: array(result.suites, `${path}.suites`).map((value, index) => {
      const suitePath = `${path}.suites[${index}]`;
      const suite = object(value, suitePath, [
        "name",
        "status",
        "reported",
        "cases",
      ]);
      return {
        name: string(suite.name, `${suitePath}.name`),
        status:
          suite.status === undefined
            ? undefined
            : enumeration(suite.status, `${suitePath}.status`, [
                "passed",
                "failed",
                "skipped",
                "error",
              ] as const),
        reported: projectReportedTestCounts(
          suite.reported,
          `${suitePath}.reported`,
        ),
        cases: array(suite.cases, `${suitePath}.cases`).map(
          (value, caseIndex) => {
            const casePath = `${suitePath}.cases[${caseIndex}]`;
            const testCase = object(value, casePath, ["name", "status"]);
            return {
              name: string(testCase.name, `${casePath}.name`),
              status: enumeration(testCase.status, `${casePath}.status`, [
                "passed",
                "failed",
                "skipped",
                "error",
              ] as const),
            };
          },
        ),
      };
    }),
    coverage: {
      inputTruncated: boolean(
        coverage.inputTruncated,
        `${path}.coverage.inputTruncated`,
      ),
      recordsTruncated: boolean(
        coverage.recordsTruncated,
        `${path}.coverage.recordsTruncated`,
      ),
      unsupportedSummaryFields: boolean(
        coverage.unsupportedSummaryFields,
        `${path}.coverage.unsupportedSummaryFields`,
      ),
      summaries: enumeration(
        coverage.summaries,
        `${path}.coverage.summaries`,
        ["none", "partial", "complete"] as const,
      ),
      cases: enumeration(coverage.cases, `${path}.coverage.cases`, [
        "none",
        "partial",
        "complete",
      ] as const),
    },
  };
}

function projectCompletionReview(
  value: unknown,
  path: string,
): CompletionReview {
  const review = object(value, path, [
    "summary",
    "durationMs",
    "actionCount",
    "phases",
    "changedFileItemIds",
    "verificationActionItemIds",
    "failedActionItemIds",
    "warningActionItemIds",
    "sourceIds",
    "outputIds",
    "testResults",
    "evidenceCoverage",
    "openQuestions",
  ]);
  return {
    summary: string(review.summary, `${path}.summary`),
    durationMs: number(review.durationMs, `${path}.durationMs`),
    actionCount: number(review.actionCount, `${path}.actionCount`),
    phases: array(review.phases ?? [], `${path}.phases`).map(
      (phase, index) =>
        projectPhaseSummary(phase, `${path}.phases[${index}]`),
    ),
    changedFileItemIds: stringArray(
      review.changedFileItemIds,
      `${path}.changedFileItemIds`,
    ),
    verificationActionItemIds: stringArray(
      review.verificationActionItemIds,
      `${path}.verificationActionItemIds`,
    ),
    failedActionItemIds: stringArray(
      review.failedActionItemIds,
      `${path}.failedActionItemIds`,
    ),
    warningActionItemIds: stringArray(
      review.warningActionItemIds,
      `${path}.warningActionItemIds`,
    ),
    sourceIds: stringArray(review.sourceIds, `${path}.sourceIds`),
    outputIds: stringArray(review.outputIds, `${path}.outputIds`),
    testResults: array(
      review.testResults ?? [],
      `${path}.testResults`,
    ).map((result, index) =>
      projectStructuredTestResults(
        result,
        `${path}.testResults[${index}]`,
      ),
    ),
    evidenceCoverage: enumeration(
      review.evidenceCoverage,
      `${path}.evidenceCoverage`,
      ["none", "partial", "complete"] as const,
    ),
    openQuestions: stringArray(
      review.openQuestions,
      `${path}.openQuestions`,
    ),
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
  const runId = optionalString(item.runId, `${path}.runId`);
  const id = string(item.id, `${path}.id`);
  const turnId =
    optionalString(item.turnId, `${path}.turnId`) ??
    optionalString(item.runId, `${path}.runId`) ??
    id;
  const providerAttempt =
    item.providerAttempt === undefined
      ? undefined
      : number(item.providerAttempt, `${path}.providerAttempt`);
  const durableEntryId = optionalString(
    item.durableEntryId,
    `${path}.durableEntryId`,
  );
  const state = itemState(item.lifecycle, `${path}.lifecycle`);
  const payload = taggedPayload(item.payload, `${path}.payload`);
  const base = {
    id,
    runId,
    turnId,
    providerAttempt,
    durableEntryId,
    state,
    createdAt: iso(context.timestampMs),
  };

  switch (payload.type) {
    case "userMessage": {
      const data = object(payload.data, `${path}.payload.data`, [
        "text",
        "attachments",
        "documents",
        "projectFiles",
        "delivery",
        "branchProvenance",
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
        documents: array(
          data.documents ?? [],
          `${path}.payload.data.documents`,
        ).map((document, index) =>
          projectDocumentReference(
            document,
            `${path}.payload.data.documents[${index}]`,
          ),
        ),
        projectFiles: array(
          data.projectFiles ?? [],
          `${path}.payload.data.projectFiles`,
        ).map((entry, index) =>
          projectTrustedFileEntry(
            entry,
            `${path}.payload.data.projectFiles[${index}]`,
          ),
        ),
        branchProvenance:
          data.branchProvenance === undefined
            ? undefined
            : projectConversationBranchProvenance(
                data.branchProvenance,
                `${path}.payload.data.branchProvenance`,
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
      const activity = projectToolActivity(
        payload.data,
        `${path}.payload.data`,
      );
      return {
        ...base,
        kind: "action",
        ...activity,
        state:
          activity.status === "running"
            ? "streaming"
            : activity.status === "failed"
              ? "failed"
              : activity.status === "stopped"
                ? "stopped"
                : state,
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
        "originItemId",
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
        phase: "changed",
        status: "succeeded",
        rawToolName: "fileChange",
        label: "Changed file",
        originItemId: optionalString(
          data.originItemId,
          `${path}.payload.data.originItemId`,
        ),
        target: string(
          data.displayPath,
          `${path}.payload.data.displayPath`,
        ),
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: [
          string(
            data.displayPath,
            `${path}.payload.data.displayPath`,
          ),
        ],
        sourceIds: [],
        outputIds: [],
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
        phase: "other",
        status: "succeeded",
        rawToolName: "compaction",
        label: "Compacted session context",
        detail: string(data.reason, `${path}.payload.data.reason`),
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
      };
    }
    case "runOutcome": {
      const data = object(payload.data, `${path}.payload.data`, [
        "outcome",
        "message",
        "review",
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
      const review = projectCompletionReview(
        data.review,
        `${path}.payload.data.review`,
      );
      return {
        ...base,
        kind: "run_outcome",
        outcome: outcome === "completed" ? "done" : outcome,
        durationMs: review.durationMs,
        summary:
          message ??
          review.summary ??
          (outcome === "completed" ? "Run completed" : `Run ${outcome}`),
        review,
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

const contextCategories = [
  "system",
  "projectInstructions",
  "conversation",
  "toolResults",
  "attachments",
  "documents",
  "projectFiles",
  "compactionSummaries",
  "other",
] as const;

function projectUsageSnapshot(value: unknown, path: string): UsageSnapshot {
  const usage = object(value, path, [
    "inputTokens",
    "outputTokens",
    "contextTokens",
    "contextLimit",
  ]);
  return {
    inputTokens: number(usage.inputTokens, `${path}.inputTokens`),
    outputTokens: number(usage.outputTokens, `${path}.outputTokens`),
    contextTokens: number(usage.contextTokens, `${path}.contextTokens`),
    contextLimit:
      usage.contextLimit === undefined
        ? undefined
        : number(usage.contextLimit, `${path}.contextLimit`),
  };
}

function projectContextTotals(value: unknown, path: string): ContextTotals {
  const wire = object(value, path, ["categories", "totalTokens"]);
  const rawCategories = array(wire.categories, `${path}.categories`);
  if (rawCategories.length > 16) {
    throw new WireContractError(
      `${path}.categories`,
      "must contain at most 16 categories",
    );
  }
  const seen = new Set<string>();
  const categories = rawCategories.map((value, index) => {
    const categoryPath = `${path}.categories[${index}]`;
    const entry = object(value, categoryPath, ["category", "tokens"]);
    const category = enumeration(
      entry.category,
      `${categoryPath}.category`,
      contextCategories,
    );
    if (seen.has(category)) {
      throw new WireContractError(
        `${path}.categories`,
        `contains duplicate category ${category}`,
      );
    }
    seen.add(category);
    return {
      category,
      tokens: number(entry.tokens, `${categoryPath}.tokens`),
    };
  });
  const totalTokens = number(wire.totalTokens, `${path}.totalTokens`);
  const categorizedTokens = categories.reduce((total, category) => {
    const next = total + category.tokens;
    if (!Number.isSafeInteger(next)) {
      throw new WireContractError(path, "category sum exceeds the safe integer range");
    }
    return next;
  }, 0);
  if (categorizedTokens !== totalTokens) {
    throw new WireContractError(
      path,
      "category tokens must reconcile exactly with totalTokens",
    );
  }
  return { categories, totalTokens };
}

function sameContextTotals(left: ContextTotals, right: ContextTotals): boolean {
  return (
    left.totalTokens === right.totalTokens &&
    left.categories.length === right.categories.length &&
    left.categories.every(
      (entry, index) =>
        entry.category === right.categories[index]?.category &&
        entry.tokens === right.categories[index]?.tokens,
    )
  );
}

function projectContextStatus(value: unknown, path: string): ContextStatus {
  const wire = object(value, path, [
    "current",
    "updatedAtMs",
    "activeCompaction",
    "lastCompaction",
  ]);
  const current = projectContextTotals(wire.current, `${path}.current`);
  const updatedAtMs = number(wire.updatedAtMs, `${path}.updatedAtMs`);
  const activeCompaction =
    wire.activeCompaction === undefined || wire.activeCompaction === null
      ? undefined
      : (() => {
          const activePath = `${path}.activeCompaction`;
          const active = object(wire.activeCompaction, activePath, [
            "id",
            "reason",
            "before",
            "startedAtMs",
          ]);
          const projected = {
            id: boundedString(active.id, `${activePath}.id`, 128),
            reason: enumeration(active.reason, `${activePath}.reason`, [
              "threshold",
              "overflow",
            ] as const),
            before: projectContextTotals(active.before, `${activePath}.before`),
            startedAtMs: number(active.startedAtMs, `${activePath}.startedAtMs`),
          };
          if (
            !sameContextTotals(projected.before, current) ||
            projected.startedAtMs < updatedAtMs
          ) {
            throw new WireContractError(
              activePath,
              "contains contradictory active-compaction facts",
            );
          }
          return projected;
        })();
  const lastCompaction =
    wire.lastCompaction === undefined || wire.lastCompaction === null
      ? undefined
      : (() => {
          const completedPath = `${path}.lastCompaction`;
          const completed = object(wire.lastCompaction, completedPath, [
            "id",
            "reason",
            "before",
            "after",
            "reclaimedTokens",
            "succeeded",
            "startedAtMs",
            "finishedAtMs",
          ]);
          const projected = {
            id: boundedString(completed.id, `${completedPath}.id`, 128),
            reason: enumeration(completed.reason, `${completedPath}.reason`, [
              "threshold",
              "overflow",
            ] as const),
            before: projectContextTotals(
              completed.before,
              `${completedPath}.before`,
            ),
            after: projectContextTotals(completed.after, `${completedPath}.after`),
            reclaimedTokens: number(
              completed.reclaimedTokens,
              `${completedPath}.reclaimedTokens`,
            ),
            succeeded: boolean(completed.succeeded, `${completedPath}.succeeded`),
            startedAtMs: number(
              completed.startedAtMs,
              `${completedPath}.startedAtMs`,
            ),
            finishedAtMs: number(
              completed.finishedAtMs,
              `${completedPath}.finishedAtMs`,
            ),
          };
          const reconciled =
            projected.before.totalTokens >= projected.after.totalTokens &&
            projected.before.totalTokens - projected.after.totalTokens ===
              projected.reclaimedTokens;
          const failedAttemptIsUnchanged =
            projected.succeeded ||
            (sameContextTotals(projected.before, projected.after) &&
              projected.reclaimedTokens === 0);
          if (
            projected.finishedAtMs < projected.startedAtMs ||
            !reconciled ||
            !failedAttemptIsUnchanged
          ) {
            throw new WireContractError(
              completedPath,
              "contains contradictory completed-compaction facts",
            );
          }
          if (
            activeCompaction === undefined &&
            (updatedAtMs < projected.finishedAtMs ||
              (updatedAtMs === projected.finishedAtMs &&
                !sameContextTotals(current, projected.after)))
          ) {
            throw new WireContractError(
              completedPath,
              "does not reconcile with the current context state",
            );
          }
          return projected;
        })();
  return { current, updatedAtMs, activeCompaction, lastCompaction };
}

function projectAgentRunTelemetry(
  value: unknown,
  path: string,
): NonNullable<ContextUsage["run"]> {
  const wire = object(value, path, [
    "phase",
    "terminalState",
    "responsesStarted",
    "responsesFinished",
    "responsesDiscarded",
    "responseActive",
    "toolCallsStarted",
    "toolCallsFinished",
    "toolExecutionsStarted",
    "toolExecutionsFinished",
    "compactionsStarted",
    "compactionsCompleted",
    "compactionsFailed",
  ]);
  const run: NonNullable<ContextUsage["run"]> = {
    phase: enumeration(wire.phase, `${path}.phase`, [
      "preparing",
      "responding",
      "retrying",
      "compacting",
      "executingTool",
      "finished",
    ] as const),
    terminalState:
      wire.terminalState === undefined || wire.terminalState === null
        ? undefined
        : enumeration(wire.terminalState, `${path}.terminalState`, [
            "completed",
            "aborted",
            "failed",
            "maxTurns",
            "dropped",
          ] as const),
    responsesStarted: number(wire.responsesStarted, `${path}.responsesStarted`),
    responsesFinished: number(
      wire.responsesFinished,
      `${path}.responsesFinished`,
    ),
    responsesDiscarded: number(
      wire.responsesDiscarded,
      `${path}.responsesDiscarded`,
    ),
    responseActive: boolean(wire.responseActive, `${path}.responseActive`),
    toolCallsStarted: number(
      wire.toolCallsStarted,
      `${path}.toolCallsStarted`,
    ),
    toolCallsFinished: number(
      wire.toolCallsFinished,
      `${path}.toolCallsFinished`,
    ),
    toolExecutionsStarted: number(
      wire.toolExecutionsStarted,
      `${path}.toolExecutionsStarted`,
    ),
    toolExecutionsFinished: number(
      wire.toolExecutionsFinished,
      `${path}.toolExecutionsFinished`,
    ),
    compactionsStarted: number(
      wire.compactionsStarted,
      `${path}.compactionsStarted`,
    ),
    compactionsCompleted: number(
      wire.compactionsCompleted,
      `${path}.compactionsCompleted`,
    ),
    compactionsFailed: number(
      wire.compactionsFailed,
      `${path}.compactionsFailed`,
    ),
  };
  if ((run.phase === "finished") !== (run.terminalState !== undefined)) {
    throw new WireContractError(
      `${path}.terminalState`,
      "must be present exactly for a finished run",
    );
  }
  const settledResponses = run.responsesFinished + run.responsesDiscarded;
  const accountedResponses = settledResponses + (run.responseActive ? 1 : 0);
  if (
    !Number.isSafeInteger(accountedResponses) ||
    accountedResponses !== run.responsesStarted ||
    (run.phase === "responding") !== run.responseActive ||
    run.toolCallsFinished > run.toolCallsStarted ||
    run.toolExecutionsFinished > run.toolExecutionsStarted ||
    run.compactionsCompleted + run.compactionsFailed > run.compactionsStarted
  ) {
    throw new WireContractError(path, "contains contradictory lifecycle counters");
  }
  return run;
}

export function projectContextUsage(value: unknown, path = "context"): ContextUsage {
  const wire = object(value, path, ["usage", "compactions", "status", "run"]);
  const usage = projectUsageSnapshot(wire.usage, `${path}.usage`);
  const compactions = number(wire.compactions, `${path}.compactions`);
  if (compactions > 0xffff_ffff) {
    throw new WireContractError(`${path}.compactions`, "must fit in an unsigned 32-bit integer");
  }
  const status =
    wire.status === undefined
      ? { current: { categories: [], totalTokens: 0 }, updatedAtMs: 0 }
      : projectContextStatus(wire.status, `${path}.status`);
  const run =
    wire.run === undefined || wire.run === null
      ? undefined
      : projectAgentRunTelemetry(wire.run, `${path}.run`);
  if (usage.contextTokens !== status.current.totalTokens) {
    throw new WireContractError(
      `${path}.usage.contextTokens`,
      "must equal the reconciled context total",
    );
  }
  if (run && compactions !== run.compactionsCompleted) {
    throw new WireContractError(
      `${path}.compactions`,
      "must equal the successful run-compaction count",
    );
  }
  return { usage, compactions, status, run };
}

function contextPercent(usage: UsageSnapshot): number {
  return usage.contextLimit && usage.contextLimit > 0
    ? Math.min(
        100,
        Math.round((usage.contextTokens / usage.contextLimit) * 100),
      )
    : 0;
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
  const contextUsage = projectContextUsage(
    snapshot.context,
    "sessionSnapshot.context",
  );
  const contextTokens = contextUsage.usage.contextTokens;
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
          const resultState =
            result.result.status === "failed"
              ? "failed"
              : result.result.status === "stopped"
                ? "stopped"
                : result.result.status === "running"
                  ? "streaming"
                  : itemState(
                      wireItem.lifecycle,
                      `${path}.lifecycle`,
                    );
          items[targetIndex] = {
            ...target,
            ...result.result,
            detail:
              result.result.outputSummary ?? result.result.summary,
            state: resultState,
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
  const title = context.summary?.title ?? "Session";

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
    context: contextUsage,
    contextTokens,
    contextPercent: contextPercent(contextUsage.usage),
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

function projectProjectSummary(value: unknown, path: string): ProjectSummary {
  const entry = object(value, path, [
    "id",
    "name",
    "trusted",
    "archived",
    "available",
    "isDefault",
    "sessionCount",
    "liveSessionCount",
  ]);
  return {
    id: string(entry.id, `${path}.id`),
    name: string(entry.name, `${path}.name`),
    trusted: boolean(entry.trusted, `${path}.trusted`),
    archived: boolean(entry.archived, `${path}.archived`),
    available: boolean(entry.available, `${path}.available`),
    isDefault: boolean(entry.isDefault, `${path}.isDefault`),
    sessionCount: number(entry.sessionCount, `${path}.sessionCount`),
    liveSessionCount: number(
      entry.liveSessionCount,
      `${path}.liveSessionCount`,
    ),
  };
}

export function projectProjectCatalog(value: unknown): ProjectCatalog {
  const catalog = object(value, "projectCatalog", [
    "protocol",
    "host",
    "catalogCursor",
    "lifecycleMutationsSupported",
    "importSupported",
    "projects",
  ]);
  protocol(catalog.protocol, "projectCatalog.protocol");
  const host = object(catalog.host, "projectCatalog.host", ["id", "name"]);
  return {
    host: {
      id: string(host.id, "projectCatalog.host.id"),
      name: string(host.name, "projectCatalog.host.name"),
    },
    catalogRevision: number(
      catalog.catalogCursor,
      "projectCatalog.catalogCursor",
    ),
    lifecycleMutationsSupported: boolean(
      catalog.lifecycleMutationsSupported,
      "projectCatalog.lifecycleMutationsSupported",
    ),
    importSupported: boolean(
      catalog.importSupported,
      "projectCatalog.importSupported",
    ),
    projects: array(catalog.projects, "projectCatalog.projects").map(
      (project, index) =>
        projectProjectSummary(
          project,
          `projectCatalog.projects[${index}]`,
        ),
    ),
  };
}

function projectUsageTotals(value: JsonObject, path: string): UsageTotals {
  return {
    promptTokens: number(value.prompt_tokens, `${path}.prompt_tokens`),
    completionTokens: number(
      value.completion_tokens,
      `${path}.completion_tokens`,
    ),
    cacheReadTokens: number(
      value.cache_read_tokens,
      `${path}.cache_read_tokens`,
    ),
    cacheWriteTokens: number(
      value.cache_write_tokens,
      `${path}.cache_write_tokens`,
    ),
    cacheWriteOneHourTokens: number(
      value.cache_write_1h_tokens,
      `${path}.cache_write_1h_tokens`,
    ),
    reasoningTokens: number(
      value.reasoning_tokens,
      `${path}.reasoning_tokens`,
    ),
    totalTokens: number(value.total_tokens, `${path}.total_tokens`),
    requestCount: number(value.request_count, `${path}.request_count`),
  };
}

const usageTotalKeys = [
  "prompt_tokens",
  "completion_tokens",
  "cache_read_tokens",
  "cache_write_tokens",
  "cache_write_1h_tokens",
  "reasoning_tokens",
  "total_tokens",
  "request_count",
] as const;

const MAX_USAGE_MODEL_ROWS = 256;

function projectModelUsage(value: unknown, path: string): ModelUsage {
  const model = object(value, path, ["provider", "model", ...usageTotalKeys]);
  return {
    provider: boundedString(model.provider, `${path}.provider`, 128),
    model: boundedString(model.model, `${path}.model`, 256),
    ...projectUsageTotals(model, path),
  };
}

function projectModelBreakdown(
  value: unknown,
  path: string,
): ModelUsage[] {
  const rows = array(value, path);
  if (rows.length > MAX_USAGE_MODEL_ROWS) {
    throw new WireContractError(
      path,
      `must contain at most ${MAX_USAGE_MODEL_ROWS} models`,
    );
  }
  const identities = new Set<string>();
  let previousTotal = Number.POSITIVE_INFINITY;
  return rows.map((value, index) => {
    const rowPath = `${path}[${index}]`;
    const row = projectModelUsage(value, rowPath);
    const identity = `${row.provider}\0${row.model}`;
    if (identities.has(identity)) {
      throw new WireContractError(rowPath, "must identify a unique model");
    }
    if (row.totalTokens > previousTotal) {
      throw new WireContractError(path, "must be ordered by total tokens");
    }
    identities.add(identity);
    previousTotal = row.totalTokens;
    return row;
  });
}

export function projectUsageStats(value: unknown): UsageStats {
  const path = "usageStats";
  const stats = object(value, path, [
    "period",
    ...usageTotalKeys,
    "models",
    "models_truncated",
  ]);
  return {
    period: enumeration(stats.period, `${path}.period`, [
      "daily",
      "weekly",
    ] as const),
    ...projectUsageTotals(stats, path),
    models: projectModelBreakdown(stats.models, `${path}.models`),
    modelsTruncated: boolean(
      stats.models_truncated,
      `${path}.models_truncated`,
    ),
  };
}

export function projectLifetimeUsage(value: unknown): LifetimeUsage {
  const path = "lifetimeUsage";
  const lifetime = object(value, path, [
    ...usageTotalKeys,
    "models",
    "models_truncated",
    "first_request_at_ms",
    "last_request_at_ms",
  ]);
  return {
    ...projectUsageTotals(lifetime, path),
    models: projectModelBreakdown(lifetime.models, `${path}.models`),
    modelsTruncated: boolean(
      lifetime.models_truncated,
      `${path}.models_truncated`,
    ),
    firstRequestAtMs:
      lifetime.first_request_at_ms === null
        ? undefined
        : number(
            lifetime.first_request_at_ms,
            `${path}.first_request_at_ms`,
          ),
    lastRequestAtMs:
      lifetime.last_request_at_ms === null
        ? undefined
        : number(
            lifetime.last_request_at_ms,
            `${path}.last_request_at_ms`,
          ),
  };
}

export function projectUsageActivity(value: unknown): UsageActivity {
  const path = "usageActivity";
  const activity = object(value, path, [
    "days",
    "current_streak",
    "longest_streak",
  ]);
  const rawDays = array(activity.days, `${path}.days`);
  if (rawDays.length > 53 * 7) {
    throw new WireContractError(`${path}.days`, "must contain at most 53 weeks");
  }
  let previousDate = "";
  const days = rawDays.map((value, index) => {
    const dayPath = `${path}.days[${index}]`;
    const day = object(value, dayPath, ["date", "tokens", "request_count"]);
    const date = string(day.date, `${dayPath}.date`);
    const timestamp = Date.parse(`${date}T00:00:00.000Z`);
    if (
      !/^\d{4}-\d{2}-\d{2}$/u.test(date) ||
      !Number.isFinite(timestamp) ||
      new Date(timestamp).toISOString().slice(0, 10) !== date
    ) {
      throw new WireContractError(`${dayPath}.date`, "must be a UTC date");
    }
    if (date <= previousDate) {
      throw new WireContractError(
        `${dayPath}.date`,
        "must be unique and ordered oldest first",
      );
    }
    previousDate = date;
    return {
      date,
      tokens: number(day.tokens, `${dayPath}.tokens`),
      requestCount: number(
        day.request_count,
        `${dayPath}.request_count`,
      ),
    };
  });
  return {
    days,
    currentStreak: number(
      activity.current_streak,
      `${path}.current_streak`,
    ),
    longestStreak: number(
      activity.longest_streak,
      `${path}.longest_streak`,
    ),
  };
}

function projectContextRefresh(
  value: unknown,
  path: string,
): RepositoryContextSnapshot["repository"]["refresh"] {
  const refresh = object(value, path, [
    "state",
    "refreshedAtUnixMs",
    "durationMs",
    "truncated",
  ]);
  return {
    state: enumeration(refresh.state, `${path}.state`, [
      "current",
      "partial",
      "notApplicable",
      "unavailable",
      "timedOut",
    ] as const),
    refreshedAtUnixMs: number(
      refresh.refreshedAtUnixMs,
      `${path}.refreshedAtUnixMs`,
    ),
    durationMs: number(refresh.durationMs, `${path}.durationMs`),
    truncated: boolean(refresh.truncated, `${path}.truncated`),
  };
}

function projectInstructionOrigin(
  value: unknown,
  path: string,
): import("./protocol").InstructionOrigin {
  const origin = object(value, path, ["relativePath", "scope"]);
  const relativePath = boundedString(
    origin.relativePath,
    `${path}.relativePath`,
    2_048,
  );
  const scope = boundedString(origin.scope, `${path}.scope`, 2_048);
  for (const [field, candidate] of [
    ["relativePath", relativePath],
    ["scope", scope],
  ] as const) {
    if (
      candidate.startsWith("/") ||
      candidate.startsWith("\\") ||
      candidate.includes("\\") ||
      /^[a-z]:/iu.test(candidate) ||
      candidate.split("/").some((segment) => segment === "..")
    ) {
      throw new WireContractError(
        `${path}.${field}`,
        "must be a project-relative display path",
      );
    }
  }
  return { relativePath, scope };
}

export function projectRepositoryContext(
  value: unknown,
  path = "repositoryContext",
): RepositoryContextSnapshot {
  const snapshot = object(value, path, [
    "projectId",
    "trust",
    "repository",
    "instructions",
  ]);
  const repository = object(snapshot.repository, `${path}.repository`, [
    "source",
    "refresh",
    "worktree",
    "head",
    "branchState",
    "branch",
    "dirty",
    "ahead",
    "behind",
  ]);
  const instructions = object(snapshot.instructions, `${path}.instructions`, [
    "source",
    "refresh",
    "files",
    "errors",
    "omittedErrors",
    "loadedBytes",
  ]);
  return {
    projectId: boundedString(snapshot.projectId, `${path}.projectId`, 128),
    trust: enumeration(snapshot.trust, `${path}.trust`, ["verified"] as const),
    repository: {
      source: enumeration(
        repository.source,
        `${path}.repository.source`,
        ["gitStatusPorcelainV2"] as const,
      ),
      refresh: projectContextRefresh(
        repository.refresh,
        `${path}.repository.refresh`,
      ),
      worktree: enumeration(
        repository.worktree,
        `${path}.repository.worktree`,
        ["present", "notRepository", "unknown"] as const,
      ),
      head: optionalString(repository.head, `${path}.repository.head`),
      branchState: enumeration(
        repository.branchState,
        `${path}.repository.branchState`,
        ["named", "detached", "unborn", "unknown"] as const,
      ),
      branch: optionalString(repository.branch, `${path}.repository.branch`),
      dirty:
        repository.dirty === undefined
          ? undefined
          : boolean(repository.dirty, `${path}.repository.dirty`),
      ahead:
        repository.ahead === undefined
          ? undefined
          : number(repository.ahead, `${path}.repository.ahead`),
      behind:
        repository.behind === undefined
          ? undefined
          : number(repository.behind, `${path}.repository.behind`),
    },
    instructions: {
      source: enumeration(
        instructions.source,
        `${path}.instructions.source`,
        ["projectAgentsMdV1"] as const,
      ),
      refresh: projectContextRefresh(
        instructions.refresh,
        `${path}.instructions.refresh`,
      ),
      files: array(
        instructions.files,
        `${path}.instructions.files`,
      ).map((value, index) => {
        const filePath = `${path}.instructions.files[${index}]`;
        const file = object(value, filePath, [
          "origin",
          "precedence",
          "byteLen",
          "sha256",
          "summary",
          "visibleContent",
          "contentTruncated",
        ]);
        return {
          origin: projectInstructionOrigin(file.origin, `${filePath}.origin`),
          precedence: number(file.precedence, `${filePath}.precedence`),
          byteLen: number(file.byteLen, `${filePath}.byteLen`),
          sha256: boundedString(file.sha256, `${filePath}.sha256`, 64),
          summary: boundedString(
            file.summary,
            `${filePath}.summary`,
            240,
            true,
          ),
          visibleContent: boundedString(
            file.visibleContent,
            `${filePath}.visibleContent`,
            128 * 1_024,
            true,
          ),
          contentTruncated: boolean(
            file.contentTruncated,
            `${filePath}.contentTruncated`,
          ),
        };
      }),
      errors: array(
        instructions.errors,
        `${path}.instructions.errors`,
      ).map((value, index) => {
        const errorPath = `${path}.instructions.errors[${index}]`;
        const error = object(value, errorPath, ["origin", "code"]);
        return {
          origin:
            error.origin === undefined
              ? undefined
              : projectInstructionOrigin(
                  error.origin,
                  `${errorPath}.origin`,
                ),
          code: enumeration(error.code, `${errorPath}.code`, [
            "directoryUnavailable",
            "unsupportedName",
            "symlinkRejected",
            "notRegularFile",
            "hardLinkRejected",
            "fileTooLarge",
            "aggregateLimitReached",
            "changedDuringRead",
            "invalidUtf8",
            "binaryContent",
            "discoveryLimitReached",
          ] as const),
        };
      }),
      omittedErrors: number(
        instructions.omittedErrors,
        `${path}.instructions.omittedErrors`,
      ),
      loadedBytes: number(
        instructions.loadedBytes,
        `${path}.instructions.loadedBytes`,
      ),
    },
  };
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
      "documents",
      "trustedProjectFiles",
      "projectFileBrowser",
      "projectFileWrite",
      "transcriptSearch",
      "previews",
      "connectedDevices",
      "lanClients",
      "terminal",
      "childAgents",
      "sessionMetadata",
      "sessionBranches",
      "conversationBranching",
      "sessionTrash",
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
  const terminal = boolean(
    capabilities.terminal,
    "hostBootstrap.capabilities.terminal",
  );
  boolean(capabilities.childAgents, "hostBootstrap.capabilities.childAgents");
  const sessionMetadata = boolean(
    capabilities.sessionMetadata,
    "hostBootstrap.capabilities.sessionMetadata",
  );
  const sessionBranches = boolean(
    capabilities.sessionBranches,
    "hostBootstrap.capabilities.sessionBranches",
  );
  const conversationBranching =
    capabilities.conversationBranching === undefined
      ? false
      : boolean(
          capabilities.conversationBranching,
          "hostBootstrap.capabilities.conversationBranching",
        );
  const sessionTrash =
    capabilities.sessionTrash === undefined
      ? false
      : boolean(
          capabilities.sessionTrash,
          "hostBootstrap.capabilities.sessionTrash",
        );
  const sessionExport = boolean(
    capabilities.sessionExport,
    "hostBootstrap.capabilities.sessionExport",
  );
  const attachments = boolean(
    capabilities.attachments,
    "hostBootstrap.capabilities.attachments",
  );
  const documents =
    capabilities.documents === undefined
      ? false
      : boolean(
          capabilities.documents,
          "hostBootstrap.capabilities.documents",
        );
  const trustedProjectFiles =
    capabilities.trustedProjectFiles === undefined
      ? false
      : boolean(
          capabilities.trustedProjectFiles,
          "hostBootstrap.capabilities.trustedProjectFiles",
        );
  const projectFileBrowser =
    capabilities.projectFileBrowser === undefined
      ? false
      : boolean(
          capabilities.projectFileBrowser,
          "hostBootstrap.capabilities.projectFileBrowser",
        );
  const projectFileWrite =
    capabilities.projectFileWrite === undefined
      ? false
      : boolean(
          capabilities.projectFileWrite,
          "hostBootstrap.capabilities.projectFileWrite",
        );
  if (projectFileWrite && !projectFileBrowser) {
    throw new WireContractError(
      "hostBootstrap.capabilities.projectFileWrite",
      "requires projectFileBrowser",
    );
  }
  const transcriptSearch =
    capabilities.transcriptSearch === undefined
      ? false
      : boolean(
          capabilities.transcriptSearch,
          "hostBootstrap.capabilities.transcriptSearch",
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
    (project, index) =>
      projectProjectSummary(
        project,
        `hostBootstrap.projects[${index}]`,
      ),
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
      terminal,
      // These describe UI paths that require more than a host capability bit.
      // Keep pairing disabled until an authenticated pairing lifecycle exists.
      attachmentIngest: attachments && attachmentPolicy !== undefined,
      pairDevices: false,
      sessionMetadata,
      sessionBranches,
      conversationBranching,
      sessionTrash,
      sessionExport,
      documents,
      trustedProjectFiles,
      projectFileBrowser,
      projectFileWrite,
      transcriptSearch,
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
      type: "item.activity_result",
      sessionId,
      actorGeneration,
      sequence,
      itemId: result.toolCallItemId,
      resultItemId: itemId,
      result: result.result,
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
      if (delta.type === "toolActivity") {
        const deltaData = object(
          delta.data,
          "event.event.data.delta.data",
          ["activity"],
        );
        return {
          type: "item.activity",
          sessionId,
          actorGeneration,
          sequence,
          itemId,
          activity: projectToolActivity(
            deltaData.activity,
            "event.event.data.delta.data.activity",
          ),
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
    case "context.updated": {
      const data = object(event.data, "event.event.data", ["context"]);
      return {
        type: "context.updated",
        sessionId,
        actorGeneration,
        sequence,
        context: projectContextUsage(
          data.context,
          "event.event.data.context",
        ),
      };
    }
    case "usage.updated": {
      const data = object(event.data, "event.event.data", ["usage"]);
      return {
        type: "usage.updated",
        sessionId,
        actorGeneration,
        sequence,
        observedAtMs: timestampMs,
        usage: projectUsageSnapshot(data.usage, "event.event.data.usage"),
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

function projectSanitizedError(
  value: unknown,
  path: string,
): {
  code: CommandAck["errorCode"];
  message: string;
  retryable: boolean;
  currentGeneration?: number;
} {
  const error = object(value, path, [
    "code",
    "message",
    "retryable",
    "currentGeneration",
  ]);
  const code = enumeration(error.code, `${path}.code`, [
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
  const retryable = boolean(error.retryable, `${path}.retryable`);
  const currentGeneration =
    error.currentGeneration === undefined
      ? undefined
      : number(error.currentGeneration, `${path}.currentGeneration`);
  return {
    code,
    message: string(error.message, `${path}.message`),
    retryable,
    currentGeneration,
  };
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
    const error = projectSanitizedError(
      disposition.error,
      "commandAck.disposition.error",
    );
    return {
      commandId,
      accepted: false,
      error: error.message,
      errorCode: error.code,
      retryable: error.retryable,
      currentGeneration: error.currentGeneration,
    };
  }
  const disposition = object(
    ack.disposition,
    "commandAck.disposition",
    hostAck
      ? ["status", "createdSessionId", "project", "catalogChanged"]
      : ["status", "runId", "createdSessionId"],
  );
  if (!hostAck) {
    optionalString(disposition.runId, "commandAck.disposition.runId");
  }
  const createdSessionId = optionalString(
    disposition.createdSessionId,
    "commandAck.disposition.createdSessionId",
  );
  const project =
    hostAck && disposition.project !== undefined
      ? projectProjectSummary(
          disposition.project,
          "commandAck.disposition.project",
        )
      : undefined;
  const catalogChanged =
    hostAck && disposition.catalogChanged !== undefined
      ? boolean(
          disposition.catalogChanged,
          "commandAck.disposition.catalogChanged",
        )
      : undefined;
  if (
    hostAck &&
    Number(createdSessionId !== undefined) +
      Number(project !== undefined) +
      Number(catalogChanged === true) !==
      1
  ) {
    throw new WireContractError(
      "commandAck.disposition",
      "must contain exactly one accepted host result",
    );
  }
  return {
    commandId,
    accepted: true,
    createdSessionId,
    project,
    catalogChanged,
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
    command.type === "session.setLifecycle" ||
    command.type === "session.deletePermanently"
  ) {
    return {
      protocol: PROTOCOL_VERSION.major,
      hostId: context.hostId,
      deviceId: context.deviceId,
      commandId: command.id,
      issuedAtMs: context.issuedAtMs,
      command:
        command.type === "session.setLifecycle"
          ? {
              type: "session.setLifecycle",
              data: {
                sessionId: command.sessionId,
                lifecycle: command.lifecycle,
              },
            }
          : {
              type: "session.deletePermanently",
              data: {
                sessionId: command.sessionId,
                confirmation: command.confirmation,
              },
            },
    };
  }
  if (
    command.type === "project.import" ||
    command.type === "project.rename" ||
    command.type === "project.setDefault" ||
    command.type === "project.clearDefault" ||
    command.type === "project.setTrust" ||
    command.type === "project.archive"
  ) {
    const wireCommand =
      command.type === "project.import"
        ? {
            type: "project.import",
            data: {
              candidateId: command.candidateId,
              displayName: command.displayName,
            },
          }
        : command.type === "project.rename"
          ? {
              type: "project.rename",
              data: {
                projectId: command.projectId,
                displayName: command.displayName,
              },
            }
          : command.type === "project.setDefault"
            ? {
                type: "project.setDefault",
                data: { projectId: command.projectId },
              }
            : command.type === "project.clearDefault"
              ? { type: "project.clearDefault" }
              : command.type === "project.setTrust"
                ? {
                    type: "project.setTrust",
                    data: {
                      projectId: command.projectId,
                      trusted: command.trusted,
                    },
                  }
                : {
                    type: "project.archive",
                    data: { projectId: command.projectId },
                  };
    return {
      protocol: PROTOCOL_VERSION.major,
      hostId: context.hostId,
      deviceId: context.deviceId,
      commandId: command.id,
      issuedAtMs: context.issuedAtMs,
      command: wireCommand,
    };
  }
  if (command.type === "session.invokeSlashCommand") {
    const invocation = command.invocation.trim();
    if (
      !invocation.startsWith("/") ||
      invocation.slice(1).trim().length === 0
    ) {
      throw new UnsupportedWireCommandError(
        command.type,
        "a slash-prefixed invocation is required",
      );
    }
    return sessionEnvelope(
      command,
      {
        type: "session.invokeSlashCommand",
        data: { invocation: { invocation } },
      },
      context,
    );
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
            ...(command.documentIds?.length
              ? { documentIds: command.documentIds }
              : {}),
            ...(command.projectFileIds?.length
              ? { projectFileIds: command.projectFileIds }
              : {}),
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
  if (command.type === "session.editUserTurn") {
    return sessionEnvelope(
      command,
      {
        type: "session.editUserTurn",
        data: {
          sourceUserEntryId: command.sourceUserEntryId,
          input: {
            text: command.prompt,
            attachments: encodeAttachments(command.attachments),
            ...(command.documentIds?.length
              ? { documentIds: command.documentIds }
              : {}),
            ...(command.projectFileIds?.length
              ? { projectFileIds: command.projectFileIds }
              : {}),
          },
        },
      },
      context,
    );
  }
  if (command.type === "session.retryResponse") {
    if ((command.modelId === undefined) !== (command.reasoning === undefined)) {
      throw new UnsupportedWireCommandError(
        command.type,
        "an alternate model and reasoning selection must be supplied together",
      );
    }
    return sessionEnvelope(
      command,
      {
        type: "session.retryResponse",
        data: {
          sourceAssistantEntryId: command.sourceAssistantEntryId,
          ...(command.modelId && command.reasoning
            ? {
                model: commandModel(
                  command.modelId,
                  command.reasoning,
                  context,
                  command.type,
                ),
              }
            : {}),
        },
      },
      context,
    );
  }
  if (command.type === "session.forkConversation") {
    return sessionEnvelope(
      command,
      {
        type: "session.forkConversation",
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
