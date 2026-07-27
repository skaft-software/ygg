export const MAX_CHILD_AGENTS = 512;
export const MAX_MCP_SERVERS = 128;
export const MAX_CATALOG_ENTRIES = 256;
export const MAX_ENTRY_CONTRIBUTIONS = 64;
export const MAX_LSP_SERVERS = 256;
export const MAX_CONTEXT_CATEGORIES = 16;
export const MAX_POLICY_RULES = 128;
export const MAX_DIAGNOSTICS_PER_SEVERITY = 1_000_000;

export type ChildAgentState =
  | "queued"
  | "running"
  | "waiting"
  | "succeeded"
  | "failed"
  | "cancelled";

export interface ChildAgentStatus {
  id: string;
  parentId?: string;
  objective: string;
  state: ChildAgentState;
  queuedAtMs: number;
  startedAtMs?: number;
  updatedAtMs: number;
  finishedAtMs?: number;
  outcome?: string;
}

export type McpServerState =
  | "configured"
  | "starting"
  | "ready"
  | "failed"
  | "stopped";

export interface McpServerStatus {
  id: string;
  label: string;
  state: McpServerState;
  restartCount: number;
  configuredAtMs: number;
  updatedAtMs: number;
  failure?: string;
}

export type TrustedCatalogKind = "skill" | "extension";
export type ContributionKind =
  | "skill"
  | "tool"
  | "command"
  | "mcpServer"
  | "theme"
  | "languageServer";

export interface CatalogContribution {
  kind: ContributionKind;
  id: string;
  label: string;
}

export interface TrustedCatalogEntry {
  id: string;
  label: string;
  kind: TrustedCatalogKind;
  enabled: boolean;
  contributions: CatalogContribution[];
}

export type CatalogReloadStatus =
  | { state: "idle" }
  | {
      state: "running";
      reloadId: string;
      retainedGeneration: number;
      startedAtMs: number;
    }
  | {
      state: "succeeded";
      reloadId: string;
      generation: number;
      startedAtMs: number;
      finishedAtMs: number;
    }
  | {
      state: "failed";
      reloadId: string;
      retainedGeneration: number;
      startedAtMs: number;
      finishedAtMs: number;
      failure: string;
    };

export interface TrustedCatalogStatus {
  generation: number;
  updatedAtMs: number;
  reload: CatalogReloadStatus;
  entries: TrustedCatalogEntry[];
}

export type LspServerState =
  | "configured"
  | "starting"
  | "ready"
  | "failed"
  | "stopped";

export interface DiagnosticCounts {
  errors: number;
  warnings: number;
  information: number;
  hints: number;
}

export interface LspServerStatus {
  projectId: string;
  languageId: string;
  state: LspServerState;
  restartCount: number;
  configuredAtMs: number;
  updatedAtMs: number;
  diagnosticRevision: number;
  diagnostics: DiagnosticCounts;
  failure?: string;
}

export type ContextCategory =
  | "system"
  | "projectInstructions"
  | "conversation"
  | "toolResults"
  | "attachments"
  | "compactionSummaries"
  | "other";

export interface ContextCategoryTotal {
  category: ContextCategory;
  tokens: number;
}

export interface ContextTotals {
  categories: ContextCategoryTotal[];
  totalTokens: number;
}

export interface ActiveCompaction {
  id: string;
  before: ContextTotals;
  startedAtMs: number;
}

export interface CompletedCompaction {
  id: string;
  before: ContextTotals;
  after: ContextTotals;
  reclaimedTokens: number;
  startedAtMs: number;
  finishedAtMs: number;
}

export interface ContextStatus {
  current: ContextTotals;
  updatedAtMs: number;
  activeCompaction?: ActiveCompaction;
  lastCompaction?: CompletedCompaction;
}

export type RuleDefault = "allow" | "deny";

export interface RuleSet<T extends string> {
  default: RuleDefault;
  allow: T[];
  deny: T[];
}

export type UnavailableConsequence =
  | "featureBlocked"
  | "hostBehaviorUnknown";
export type FilesystemAccess =
  | "none"
  | "trustedProjectRead"
  | "trustedProjectReadWrite";

export interface UnavailablePolicy {
  status: "unavailable";
  reason: string;
  consequence: UnavailableConsequence;
}

export type FilesystemPolicy =
  | { status: "enforced"; access: FilesystemAccess }
  | UnavailablePolicy;
export type ToolPolicy =
  | { status: "enforced"; rules: RuleSet<string> }
  | UnavailablePolicy;
export type CommandPolicy =
  | { status: "enforced"; rules: RuleSet<string> }
  | UnavailablePolicy;

export type DomainConsequence =
  | { mode: "blocked" }
  | { mode: "domainRules"; domains: RuleSet<string> };
export type RemoteReadPolicy =
  | { status: "enforced"; consequence: DomainConsequence }
  | UnavailablePolicy;
export type ProcessNetworkPolicy =
  | { status: "enforced"; consequence: DomainConsequence }
  | UnavailablePolicy;

export type ApprovalOperation =
  | "filesystemWrite"
  | "tool"
  | "command"
  | "remoteRead"
  | "processNetwork"
  | "secretAccess";
export type ApprovalConsequence =
  | { mode: "never" }
  | { mode: "requiredFor"; operations: ApprovalOperation[] };
export type ApprovalPolicy =
  | { status: "enforced"; consequence: ApprovalConsequence }
  | UnavailablePolicy;

export type SecretsConsequence =
  | { mode: "blocked" }
  | { mode: "namedGrants"; grants: string[] };
export type SecretsPolicy =
  | { status: "enforced"; consequence: SecretsConsequence }
  | UnavailablePolicy;

export interface RuntimePolicyStatus {
  revision: number;
  observedAtMs: number;
  filesystem: FilesystemPolicy;
  tools: ToolPolicy;
  commands: CommandPolicy;
  remoteRead: RemoteReadPolicy;
  processNetwork: ProcessNetworkPolicy;
  approvals: ApprovalPolicy;
  secrets: SecretsPolicy;
}

export interface RuntimeSnapshot {
  childAgents: ChildAgentStatus[];
  mcpServers: McpServerStatus[];
  catalog: TrustedCatalogStatus;
  lspServers: LspServerStatus[];
  context: ContextStatus;
  policy?: RuntimePolicyStatus;
}

type WireObject = Record<string, unknown>;

function invalid(path: string, detail: string): never {
  throw new Error(`${path}: ${detail}`);
}

function objectValue(
  value: unknown,
  path: string,
  required: readonly string[],
  optional: readonly string[] = [],
): WireObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return invalid(path, "expected an object");
  }
  const wire = value as WireObject;
  const accepted = new Set([...required, ...optional]);
  for (const key of Object.keys(wire)) {
    if (!accepted.has(key)) invalid(`${path}.${key}`, "unknown field");
  }
  for (const key of required) {
    if (!Object.hasOwn(wire, key)) invalid(`${path}.${key}`, "missing field");
  }
  return wire;
}

function discriminantValue(
  value: unknown,
  path: string,
  key: string,
): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return invalid(path, "expected an object");
  }
  const wire = value as WireObject;
  if (!Object.hasOwn(wire, key)) invalid(`${path}.${key}`, "missing field");
  return wire[key];
}

function optionalValue<T>(
  wire: WireObject,
  key: string,
  project: (value: unknown, path: string) => T,
  path: string,
): T | undefined {
  if (!Object.hasOwn(wire, key) || wire[key] === null) return undefined;
  return project(wire[key], `${path}.${key}`);
}

function arrayValue<T>(
  value: unknown,
  path: string,
  maximum: number,
  project: (entry: unknown, path: string) => T,
): T[] {
  if (!Array.isArray(value)) return invalid(path, "expected an array");
  if (value.length > maximum) {
    return invalid(path, `exceeds the ${maximum}-item limit`);
  }
  return value.map((entry, index) => project(entry, `${path}[${index}]`));
}

function enumValue<T extends string>(
  value: unknown,
  path: string,
  accepted: readonly T[],
): T {
  if (typeof value !== "string" || !accepted.includes(value as T)) {
    return invalid(path, `expected one of ${accepted.join(", ")}`);
  }
  return value as T;
}

function booleanValue(value: unknown, path: string): boolean {
  if (typeof value !== "boolean") return invalid(path, "expected a boolean");
  return value;
}

function unsignedInteger(
  value: unknown,
  path: string,
  maximum = Number.MAX_SAFE_INTEGER,
): number {
  if (
    typeof value !== "number" ||
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value > maximum
  ) {
    return invalid(path, `expected an unsigned integer no greater than ${maximum}`);
  }
  return value;
}

function utf8Length(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function containsAbsoluteHostPath(value: string): boolean {
  if (value.includes("file://") || value.includes("\\\\")) return true;
  return value.split(/\s/u).some((word) => {
    const punctuation = new Set([
      '"',
      "'",
      "`",
      "(",
      ")",
      "[",
      "]",
      "{",
      "}",
      ",",
      ";",
    ]);
    let start = 0;
    let end = word.length;
    while (start < end && punctuation.has(word[start] ?? "")) start += 1;
    while (end > start && punctuation.has(word[end - 1] ?? "")) end -= 1;
    const candidate = word.slice(start, end);
    return (
      candidate.startsWith("/") ||
      candidate.startsWith("~/") ||
      /^[A-Za-z]:/u.test(candidate)
    );
  });
}

function publicText(value: unknown, path: string, maximum: number): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    utf8Length(value) > maximum
  ) {
    return invalid(path, "expected non-empty bounded public text");
  }
  if (/\p{Cc}/u.test(value)) {
    return invalid(path, "public text contains control characters");
  }
  if (containsAbsoluteHostPath(value)) {
    return invalid(path, "public text contains an absolute host path");
  }
  return value;
}

function runtimeId(value: unknown, path: string): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    utf8Length(value) > 128 ||
    !/^[A-Za-z0-9._:-]+$/u.test(value)
  ) {
    return invalid(path, "expected a bounded opaque runtime id");
  }
  return value;
}

function commandName(value: unknown, path: string): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    utf8Length(value) > 192 ||
    !/^[A-Za-z0-9._+-]+$/u.test(value)
  ) {
    return invalid(path, "expected an executable name without arguments or paths");
  }
  return value;
}

function domainName(value: unknown, path: string): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > 253 ||
    value !== value.toLowerCase() ||
    !/^[a-z0-9.-]+$/u.test(value) ||
    value.startsWith(".") ||
    value.endsWith(".") ||
    value.includes("..") ||
    value
      .split(".")
      .some(
        (label) =>
          label.length === 0 ||
          label.startsWith("-") ||
          label.endsWith("-"),
      )
  ) {
    return invalid(path, "expected an exact lowercase hostname");
  }
  return value;
}

function uniqueStrings(values: readonly string[], path: string): void {
  if (new Set(values).size !== values.length) {
    invalid(path, "contains duplicate values");
  }
}

function childAgent(value: unknown, path: string): ChildAgentStatus {
  const wire = objectValue(
    value,
    path,
    ["id", "objective", "state", "queuedAtMs", "updatedAtMs"],
    ["parentId", "startedAtMs", "finishedAtMs", "outcome"],
  );
  const status: ChildAgentStatus = {
    id: runtimeId(wire.id, `${path}.id`),
    objective: publicText(wire.objective, `${path}.objective`, 4 * 1024),
    state: enumValue(wire.state, `${path}.state`, [
      "queued",
      "running",
      "waiting",
      "succeeded",
      "failed",
      "cancelled",
    ]),
    queuedAtMs: unsignedInteger(wire.queuedAtMs, `${path}.queuedAtMs`),
    updatedAtMs: unsignedInteger(wire.updatedAtMs, `${path}.updatedAtMs`),
  };
  status.parentId = optionalValue(wire, "parentId", runtimeId, path);
  status.startedAtMs = optionalValue(
    wire,
    "startedAtMs",
    unsignedInteger,
    path,
  );
  status.finishedAtMs = optionalValue(
    wire,
    "finishedAtMs",
    unsignedInteger,
    path,
  );
  status.outcome = optionalValue(
    wire,
    "outcome",
    (entry, entryPath) => publicText(entry, entryPath, 2 * 1024),
    path,
  );

  if (
    status.updatedAtMs < status.queuedAtMs ||
    (status.startedAtMs !== undefined &&
      (status.startedAtMs < status.queuedAtMs ||
        status.startedAtMs > status.updatedAtMs)) ||
    (status.finishedAtMs !== undefined &&
      (status.finishedAtMs < status.queuedAtMs ||
        status.finishedAtMs !== status.updatedAtMs))
  ) {
    invalid(path, "contains contradictory child-agent timing");
  }
  const terminal = ["succeeded", "failed", "cancelled"].includes(status.state);
  if (
    terminal !==
      (status.finishedAtMs !== undefined && status.outcome !== undefined) ||
    ((status.state === "running" || status.state === "waiting") &&
      status.startedAtMs === undefined) ||
    (status.state === "queued" && status.startedAtMs !== undefined)
  ) {
    invalid(path, "contains contradictory child-agent lifecycle facts");
  }
  return status;
}

function mcpServer(value: unknown, path: string): McpServerStatus {
  const wire = objectValue(
    value,
    path,
    [
      "id",
      "label",
      "state",
      "restartCount",
      "configuredAtMs",
      "updatedAtMs",
    ],
    ["failure"],
  );
  const status: McpServerStatus = {
    id: runtimeId(wire.id, `${path}.id`),
    label: publicText(wire.label, `${path}.label`, 192),
    state: enumValue(wire.state, `${path}.state`, [
      "configured",
      "starting",
      "ready",
      "failed",
      "stopped",
    ]),
    restartCount: unsignedInteger(wire.restartCount, `${path}.restartCount`),
    configuredAtMs: unsignedInteger(
      wire.configuredAtMs,
      `${path}.configuredAtMs`,
    ),
    updatedAtMs: unsignedInteger(wire.updatedAtMs, `${path}.updatedAtMs`),
  };
  status.failure = optionalValue(
    wire,
    "failure",
    (entry, entryPath) => publicText(entry, entryPath, 2 * 1024),
    path,
  );
  if (
    status.updatedAtMs < status.configuredAtMs ||
    (status.state === "failed") !== (status.failure !== undefined)
  ) {
    invalid(path, "contains contradictory MCP status");
  }
  return status;
}

function catalogContribution(
  value: unknown,
  path: string,
): CatalogContribution {
  const wire = objectValue(value, path, ["kind", "id", "label"]);
  return {
    kind: enumValue(wire.kind, `${path}.kind`, [
      "skill",
      "tool",
      "command",
      "mcpServer",
      "theme",
      "languageServer",
    ]),
    id: runtimeId(wire.id, `${path}.id`),
    label: publicText(wire.label, `${path}.label`, 192),
  };
}

function catalogEntry(value: unknown, path: string): TrustedCatalogEntry {
  const wire = objectValue(value, path, [
    "id",
    "label",
    "kind",
    "enabled",
    "contributions",
  ]);
  const entry: TrustedCatalogEntry = {
    id: runtimeId(wire.id, `${path}.id`),
    label: publicText(wire.label, `${path}.label`, 192),
    kind: enumValue(wire.kind, `${path}.kind`, ["skill", "extension"]),
    enabled: booleanValue(wire.enabled, `${path}.enabled`),
    contributions: arrayValue(
      wire.contributions,
      `${path}.contributions`,
      MAX_ENTRY_CONTRIBUTIONS,
      catalogContribution,
    ),
  };
  const localKeys = entry.contributions.map(
    (contribution) => `${contribution.kind}\u0000${contribution.id}`,
  );
  uniqueStrings(localKeys, `${path}.contributions`);
  return entry;
}

function catalogReload(value: unknown, path: string): CatalogReloadStatus {
  const discriminant = discriminantValue(value, path, "state");
  switch (discriminant) {
    case "idle":
      objectValue(value, path, ["state"]);
      return { state: "idle" };
    case "running": {
      const wire = objectValue(value, path, [
        "state",
        "reloadId",
        "retainedGeneration",
        "startedAtMs",
      ]);
      return {
        state: "running",
        reloadId: runtimeId(wire.reloadId, `${path}.reloadId`),
        retainedGeneration: unsignedInteger(
          wire.retainedGeneration,
          `${path}.retainedGeneration`,
        ),
        startedAtMs: unsignedInteger(wire.startedAtMs, `${path}.startedAtMs`),
      };
    }
    case "succeeded": {
      const wire = objectValue(value, path, [
        "state",
        "reloadId",
        "generation",
        "startedAtMs",
        "finishedAtMs",
      ]);
      return {
        state: "succeeded",
        reloadId: runtimeId(wire.reloadId, `${path}.reloadId`),
        generation: unsignedInteger(wire.generation, `${path}.generation`),
        startedAtMs: unsignedInteger(wire.startedAtMs, `${path}.startedAtMs`),
        finishedAtMs: unsignedInteger(
          wire.finishedAtMs,
          `${path}.finishedAtMs`,
        ),
      };
    }
    case "failed": {
      const wire = objectValue(value, path, [
        "state",
        "reloadId",
        "retainedGeneration",
        "startedAtMs",
        "finishedAtMs",
        "failure",
      ]);
      return {
        state: "failed",
        reloadId: runtimeId(wire.reloadId, `${path}.reloadId`),
        retainedGeneration: unsignedInteger(
          wire.retainedGeneration,
          `${path}.retainedGeneration`,
        ),
        startedAtMs: unsignedInteger(wire.startedAtMs, `${path}.startedAtMs`),
        finishedAtMs: unsignedInteger(
          wire.finishedAtMs,
          `${path}.finishedAtMs`,
        ),
        failure: publicText(wire.failure, `${path}.failure`, 2 * 1024),
      };
    }
    default:
      return invalid(`${path}.state`, "unknown catalog reload state");
  }
}

function trustedCatalog(value: unknown, path: string): TrustedCatalogStatus {
  const wire = objectValue(value, path, [
    "generation",
    "updatedAtMs",
    "reload",
    "entries",
  ]);
  const status: TrustedCatalogStatus = {
    generation: unsignedInteger(wire.generation, `${path}.generation`),
    updatedAtMs: unsignedInteger(wire.updatedAtMs, `${path}.updatedAtMs`),
    reload: catalogReload(wire.reload, `${path}.reload`),
    entries: arrayValue(
      wire.entries,
      `${path}.entries`,
      MAX_CATALOG_ENTRIES,
      catalogEntry,
    ),
  };
  uniqueStrings(
    status.entries.map((entry) => entry.id),
    `${path}.entries`,
  );
  const contributionKeys = status.entries.flatMap((entry) =>
    entry.contributions.map(
      (contribution) => `${contribution.kind}\u0000${contribution.id}`,
    ),
  );
  uniqueStrings(contributionKeys, `${path}.entries.contributions`);

  const reload = status.reload;
  if (
    (reload.state === "idle" && status.generation !== 0) ||
    (reload.state === "running" &&
      (reload.retainedGeneration !== status.generation ||
        reload.startedAtMs < status.updatedAtMs)) ||
    (reload.state === "succeeded" &&
      (reload.generation > status.generation ||
        reload.finishedAtMs > status.updatedAtMs ||
        reload.finishedAtMs < reload.startedAtMs)) ||
    (reload.state === "failed" &&
      (reload.retainedGeneration > status.generation ||
        reload.finishedAtMs < reload.startedAtMs))
  ) {
    invalid(path, "contains contradictory catalog reload facts");
  }
  return status;
}

function diagnosticCounts(value: unknown, path: string): DiagnosticCounts {
  const wire = objectValue(value, path, [
    "errors",
    "warnings",
    "information",
    "hints",
  ]);
  return {
    errors: unsignedInteger(
      wire.errors,
      `${path}.errors`,
      MAX_DIAGNOSTICS_PER_SEVERITY,
    ),
    warnings: unsignedInteger(
      wire.warnings,
      `${path}.warnings`,
      MAX_DIAGNOSTICS_PER_SEVERITY,
    ),
    information: unsignedInteger(
      wire.information,
      `${path}.information`,
      MAX_DIAGNOSTICS_PER_SEVERITY,
    ),
    hints: unsignedInteger(
      wire.hints,
      `${path}.hints`,
      MAX_DIAGNOSTICS_PER_SEVERITY,
    ),
  };
}

function lspServer(value: unknown, path: string): LspServerStatus {
  const wire = objectValue(
    value,
    path,
    [
      "projectId",
      "languageId",
      "state",
      "restartCount",
      "configuredAtMs",
      "updatedAtMs",
      "diagnosticRevision",
      "diagnostics",
    ],
    ["failure"],
  );
  const status: LspServerStatus = {
    projectId: runtimeId(wire.projectId, `${path}.projectId`),
    languageId: runtimeId(wire.languageId, `${path}.languageId`),
    state: enumValue(wire.state, `${path}.state`, [
      "configured",
      "starting",
      "ready",
      "failed",
      "stopped",
    ]),
    restartCount: unsignedInteger(wire.restartCount, `${path}.restartCount`),
    configuredAtMs: unsignedInteger(
      wire.configuredAtMs,
      `${path}.configuredAtMs`,
    ),
    updatedAtMs: unsignedInteger(wire.updatedAtMs, `${path}.updatedAtMs`),
    diagnosticRevision: unsignedInteger(
      wire.diagnosticRevision,
      `${path}.diagnosticRevision`,
    ),
    diagnostics: diagnosticCounts(wire.diagnostics, `${path}.diagnostics`),
  };
  status.failure = optionalValue(
    wire,
    "failure",
    (entry, entryPath) => publicText(entry, entryPath, 2 * 1024),
    path,
  );
  const diagnosticsTotal = Object.values(status.diagnostics).reduce(
    (sum, count) => sum + count,
    0,
  );
  if (
    status.updatedAtMs < status.configuredAtMs ||
    (status.state === "failed") !== (status.failure !== undefined) ||
    (status.state !== "ready" && diagnosticsTotal !== 0)
  ) {
    invalid(path, "contains contradictory language-server status");
  }
  return status;
}

function contextTotals(value: unknown, path: string): ContextTotals {
  const wire = objectValue(value, path, ["categories", "totalTokens"]);
  const categories = arrayValue(
    wire.categories,
    `${path}.categories`,
    MAX_CONTEXT_CATEGORIES,
    (entry, entryPath): ContextCategoryTotal => {
      const categoryWire = objectValue(entry, entryPath, [
        "category",
        "tokens",
      ]);
      return {
        category: enumValue(categoryWire.category, `${entryPath}.category`, [
          "system",
          "projectInstructions",
          "conversation",
          "toolResults",
          "attachments",
          "compactionSummaries",
          "other",
        ]),
        tokens: unsignedInteger(
          categoryWire.tokens,
          `${entryPath}.tokens`,
        ),
      };
    },
  );
  uniqueStrings(
    categories.map((entry) => entry.category),
    `${path}.categories`,
  );
  const totalTokens = unsignedInteger(wire.totalTokens, `${path}.totalTokens`);
  const sum = categories.reduce((total, category) => {
    const next = total + category.tokens;
    if (!Number.isSafeInteger(next)) {
      invalid(path, "category sum exceeds the safe integer range");
    }
    return next;
  }, 0);
  if (sum !== totalTokens) {
    invalid(path, "category tokens do not reconcile with totalTokens");
  }
  return { categories, totalTokens };
}

function sameTotals(left: ContextTotals, right: ContextTotals): boolean {
  return (
    left.totalTokens === right.totalTokens &&
    left.categories.length === right.categories.length &&
    left.categories.every(
      (category, index) =>
        category.category === right.categories[index]?.category &&
        category.tokens === right.categories[index]?.tokens,
    )
  );
}

function activeCompaction(value: unknown, path: string): ActiveCompaction {
  const wire = objectValue(value, path, ["id", "before", "startedAtMs"]);
  return {
    id: runtimeId(wire.id, `${path}.id`),
    before: contextTotals(wire.before, `${path}.before`),
    startedAtMs: unsignedInteger(wire.startedAtMs, `${path}.startedAtMs`),
  };
}

function completedCompaction(value: unknown, path: string): CompletedCompaction {
  const wire = objectValue(value, path, [
    "id",
    "before",
    "after",
    "reclaimedTokens",
    "startedAtMs",
    "finishedAtMs",
  ]);
  const completed: CompletedCompaction = {
    id: runtimeId(wire.id, `${path}.id`),
    before: contextTotals(wire.before, `${path}.before`),
    after: contextTotals(wire.after, `${path}.after`),
    reclaimedTokens: unsignedInteger(
      wire.reclaimedTokens,
      `${path}.reclaimedTokens`,
    ),
    startedAtMs: unsignedInteger(wire.startedAtMs, `${path}.startedAtMs`),
    finishedAtMs: unsignedInteger(wire.finishedAtMs, `${path}.finishedAtMs`),
  };
  if (
    completed.finishedAtMs < completed.startedAtMs ||
    completed.before.totalTokens - completed.after.totalTokens !==
      completed.reclaimedTokens
  ) {
    invalid(path, "contains contradictory completed-compaction facts");
  }
  return completed;
}

function contextStatus(value: unknown, path: string): ContextStatus {
  const wire = objectValue(
    value,
    path,
    ["current", "updatedAtMs"],
    ["activeCompaction", "lastCompaction"],
  );
  const status: ContextStatus = {
    current: contextTotals(wire.current, `${path}.current`),
    updatedAtMs: unsignedInteger(wire.updatedAtMs, `${path}.updatedAtMs`),
  };
  status.activeCompaction = optionalValue(
    wire,
    "activeCompaction",
    activeCompaction,
    path,
  );
  status.lastCompaction = optionalValue(
    wire,
    "lastCompaction",
    completedCompaction,
    path,
  );
  if (
    status.activeCompaction &&
    (!sameTotals(status.activeCompaction.before, status.current) ||
      status.activeCompaction.startedAtMs < status.updatedAtMs)
  ) {
    invalid(path, "contains contradictory active-compaction facts");
  }
  if (
    status.lastCompaction &&
    !status.activeCompaction &&
    (!sameTotals(status.current, status.lastCompaction.after) ||
      status.updatedAtMs < status.lastCompaction.finishedAtMs)
  ) {
    invalid(path, "contains contradictory completed-compaction state");
  }
  return status;
}

function ruleSet(
  value: unknown,
  path: string,
  projectEntry: (value: unknown, path: string) => string,
): RuleSet<string> {
  const wire = objectValue(value, path, ["default", "allow", "deny"]);
  const rules: RuleSet<string> = {
    default: enumValue(wire.default, `${path}.default`, ["allow", "deny"]),
    allow: arrayValue(
      wire.allow,
      `${path}.allow`,
      MAX_POLICY_RULES,
      projectEntry,
    ),
    deny: arrayValue(
      wire.deny,
      `${path}.deny`,
      MAX_POLICY_RULES,
      projectEntry,
    ),
  };
  uniqueStrings(rules.allow, `${path}.allow`);
  uniqueStrings(rules.deny, `${path}.deny`);
  if (rules.allow.some((entry) => rules.deny.includes(entry))) {
    invalid(path, "allow and deny rules overlap");
  }
  return rules;
}

function unavailablePolicy(
  value: unknown,
  path: string,
): UnavailablePolicy {
  const wire = objectValue(value, path, [
    "status",
    "reason",
    "consequence",
  ]);
  if (wire.status !== "unavailable") {
    invalid(`${path}.status`, "expected unavailable");
  }
  return {
    status: "unavailable",
    reason: publicText(wire.reason, `${path}.reason`, 2 * 1024),
    consequence: enumValue(wire.consequence, `${path}.consequence`, [
      "featureBlocked",
      "hostBehaviorUnknown",
    ]),
  };
}

function filesystemPolicy(value: unknown, path: string): FilesystemPolicy {
  const status = discriminantValue(value, path, "status");
  if (status === "unavailable") return unavailablePolicy(value, path);
  if (status !== "enforced") {
    return invalid(`${path}.status`, "unknown filesystem policy status");
  }
  const wire = objectValue(value, path, ["status", "access"]);
  return {
    status,
    access: enumValue(wire.access, `${path}.access`, [
      "none",
      "trustedProjectRead",
      "trustedProjectReadWrite",
    ]),
  };
}

function exactRulePolicy(
  value: unknown,
  path: string,
  projectEntry: (entry: unknown, entryPath: string) => string,
): ToolPolicy | CommandPolicy {
  const status = discriminantValue(value, path, "status");
  if (status === "unavailable") return unavailablePolicy(value, path);
  if (status !== "enforced") {
    return invalid(`${path}.status`, "unknown rule policy status");
  }
  const wire = objectValue(value, path, ["status", "rules"]);
  return {
    status,
    rules: ruleSet(wire.rules, `${path}.rules`, projectEntry),
  };
}

function domainConsequence(
  value: unknown,
  path: string,
): DomainConsequence {
  const mode = discriminantValue(value, path, "mode");
  if (mode === "blocked") {
    objectValue(value, path, ["mode"]);
    return { mode };
  }
  if (mode !== "domainRules") {
    return invalid(`${path}.mode`, "unknown network consequence mode");
  }
  const wire = objectValue(value, path, ["mode", "domains"]);
  return {
    mode,
    domains: ruleSet(wire.domains, `${path}.domains`, domainName),
  };
}

function domainPolicy(
  value: unknown,
  path: string,
): RemoteReadPolicy | ProcessNetworkPolicy {
  const status = discriminantValue(value, path, "status");
  if (status === "unavailable") return unavailablePolicy(value, path);
  if (status !== "enforced") {
    return invalid(`${path}.status`, "unknown network policy status");
  }
  const wire = objectValue(value, path, ["status", "consequence"]);
  return {
    status,
    consequence: domainConsequence(
      wire.consequence,
      `${path}.consequence`,
    ),
  };
}

function approvalPolicy(value: unknown, path: string): ApprovalPolicy {
  const status = discriminantValue(value, path, "status");
  if (status === "unavailable") return unavailablePolicy(value, path);
  if (status !== "enforced") {
    return invalid(`${path}.status`, "unknown approval policy status");
  }
  const wire = objectValue(value, path, ["status", "consequence"]);
  const consequencePath = `${path}.consequence`;
  const mode = discriminantValue(
    wire.consequence,
    consequencePath,
    "mode",
  );
  if (mode === "never") {
    objectValue(wire.consequence, consequencePath, ["mode"]);
    return { status, consequence: { mode } };
  }
  if (mode !== "requiredFor") {
    return invalid(`${consequencePath}.mode`, "unknown approval consequence");
  }
  const consequenceWire = objectValue(wire.consequence, consequencePath, [
    "mode",
    "operations",
  ]);
  const operations = arrayValue<ApprovalOperation>(
    consequenceWire.operations,
    `${consequencePath}.operations`,
    8,
    (entry, entryPath) =>
      enumValue(entry, entryPath, [
        "filesystemWrite",
        "tool",
        "command",
        "remoteRead",
        "processNetwork",
        "secretAccess",
      ]),
  );
  if (operations.length === 0) {
    invalid(`${consequencePath}.operations`, "must not be empty");
  }
  uniqueStrings(operations, `${consequencePath}.operations`);
  return { status, consequence: { mode, operations } };
}

function secretsPolicy(value: unknown, path: string): SecretsPolicy {
  const status = discriminantValue(value, path, "status");
  if (status === "unavailable") return unavailablePolicy(value, path);
  if (status !== "enforced") {
    return invalid(`${path}.status`, "unknown secrets policy status");
  }
  const wire = objectValue(value, path, ["status", "consequence"]);
  const consequencePath = `${path}.consequence`;
  const mode = discriminantValue(
    wire.consequence,
    consequencePath,
    "mode",
  );
  if (mode === "blocked") {
    objectValue(wire.consequence, consequencePath, ["mode"]);
    return { status, consequence: { mode } };
  }
  if (mode !== "namedGrants") {
    return invalid(`${consequencePath}.mode`, "unknown secrets consequence");
  }
  const consequenceWire = objectValue(wire.consequence, consequencePath, [
    "mode",
    "grants",
  ]);
  const grants = arrayValue(
    consequenceWire.grants,
    `${consequencePath}.grants`,
    MAX_POLICY_RULES,
    runtimeId,
  );
  if (grants.length === 0) {
    invalid(`${consequencePath}.grants`, "must not be empty");
  }
  uniqueStrings(grants, `${consequencePath}.grants`);
  return { status, consequence: { mode, grants } };
}

function runtimePolicy(value: unknown, path: string): RuntimePolicyStatus {
  const wire = objectValue(value, path, [
    "revision",
    "observedAtMs",
    "filesystem",
    "tools",
    "commands",
    "remoteRead",
    "processNetwork",
    "approvals",
    "secrets",
  ]);
  return {
    revision: unsignedInteger(wire.revision, `${path}.revision`),
    observedAtMs: unsignedInteger(wire.observedAtMs, `${path}.observedAtMs`),
    filesystem: filesystemPolicy(wire.filesystem, `${path}.filesystem`),
    tools: exactRulePolicy(
      wire.tools,
      `${path}.tools`,
      runtimeId,
    ) as ToolPolicy,
    commands: exactRulePolicy(
      wire.commands,
      `${path}.commands`,
      commandName,
    ) as CommandPolicy,
    remoteRead: domainPolicy(
      wire.remoteRead,
      `${path}.remoteRead`,
    ) as RemoteReadPolicy,
    processNetwork: domainPolicy(
      wire.processNetwork,
      `${path}.processNetwork`,
    ) as ProcessNetworkPolicy,
    approvals: approvalPolicy(wire.approvals, `${path}.approvals`),
    secrets: secretsPolicy(wire.secrets, `${path}.secrets`),
  };
}

function validateChildGraph(
  children: readonly ChildAgentStatus[],
  path: string,
): void {
  uniqueStrings(
    children.map((child) => child.id),
    path,
  );
  const byId = new Map(children.map((child) => [child.id, child]));
  for (const child of children) {
    if (!child.parentId) continue;
    if (child.parentId === child.id || !byId.has(child.parentId)) {
      invalid(path, "contains an invalid child-agent parent");
    }
    const visited = new Set([child.id]);
    let cursor: string | undefined = child.parentId;
    while (cursor) {
      if (visited.has(cursor)) {
        invalid(path, "contains a child-agent parent cycle");
      }
      visited.add(cursor);
      cursor = byId.get(cursor)?.parentId;
    }
  }
}

/**
 * Strictly projects the path-free RuntimeSnapshot JSON emitted by ygg-serve.
 *
 * Every object denies unknown fields. Collection limits and the Rust DTO's
 * cross-field invariants are checked again before UI code receives a value.
 */
export function projectRuntimeSnapshot(value: unknown): RuntimeSnapshot {
  const path = "runtimeSnapshot";
  const wire = objectValue(
    value,
    path,
    ["childAgents", "mcpServers", "catalog", "lspServers", "context"],
    ["policy"],
  );
  const snapshot: RuntimeSnapshot = {
    childAgents: arrayValue(
      wire.childAgents,
      `${path}.childAgents`,
      MAX_CHILD_AGENTS,
      childAgent,
    ),
    mcpServers: arrayValue(
      wire.mcpServers,
      `${path}.mcpServers`,
      MAX_MCP_SERVERS,
      mcpServer,
    ),
    catalog: trustedCatalog(wire.catalog, `${path}.catalog`),
    lspServers: arrayValue(
      wire.lspServers,
      `${path}.lspServers`,
      MAX_LSP_SERVERS,
      lspServer,
    ),
    context: contextStatus(wire.context, `${path}.context`),
  };
  snapshot.policy = optionalValue(wire, "policy", runtimePolicy, path);

  validateChildGraph(snapshot.childAgents, `${path}.childAgents`);
  uniqueStrings(
    snapshot.mcpServers.map((server) => server.id),
    `${path}.mcpServers`,
  );
  uniqueStrings(
    snapshot.lspServers.map(
      (server) => `${server.projectId}\u0000${server.languageId}`,
    ),
    `${path}.lspServers`,
  );
  return snapshot;
}
