export const PROTOCOL_VERSION = {
  major: 1,
  minor: 0,
} as const;

export type SessionStatus =
  | "idle"
  | "working"
  | "needs_attention"
  | "done"
  | "failed"
  | "stopped"
  | "disconnected";

export type ItemState = "streaming" | "committed" | "failed" | "stopped";

// Reasoning choices are provider-defined catalog values. Keep the UI model
// open while validating every selection against its model's bounded catalog.
export type ReasoningEffort = string;

export type AuthorityProfile = "readOnly" | "workspace" | "fullAccess";

export type ThemeColor =
  | { kind: "default" }
  | { kind: "rgb"; red: number; green: number; blue: number }
  | { kind: "ansi"; index: number };

export interface ThemeRoleStyle {
  foreground?: string;
  background?: string;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  strikethrough: boolean;
}

export interface ThemeDto {
  name: string;
  source: "bundled" | "global" | "project" | "explicit";
  revision: number;
  scheme: "light" | "dark" | "unknown";
  density: "compact" | "comfortable" | "airy";
  motion: "full" | "reduced" | "none";
  typography: {
    body_family: string;
    mono_family: string;
    body_size: number;
    display_ratio_milli: number;
  };
  colors: Record<string, ThemeColor>;
  roles: Record<string, ThemeRoleStyle>;
}

export interface ThemeOption {
  id: string;
  theme: ThemeDto;
}

export interface ProjectSummary {
  id: string;
  name: string;
  trusted: boolean;
  archived: boolean;
  available: boolean;
  isDefault: boolean;
  sessionCount: number;
  liveSessionCount: number;
}

export interface ProjectCatalog {
  host: {
    id: string;
    name: string;
  };
  catalogRevision: number;
  lifecycleMutationsSupported: boolean;
  importSupported: boolean;
  projects: ProjectSummary[];
}

export type ContextRefreshState =
  | "current"
  | "partial"
  | "notApplicable"
  | "unavailable"
  | "timedOut";

export interface ContextRefreshStatus {
  state: ContextRefreshState;
  refreshedAtUnixMs: number;
  durationMs: number;
  truncated: boolean;
}

export interface InstructionOrigin {
  relativePath: string;
  scope: string;
}

export type InstructionLoadErrorCode =
  | "directoryUnavailable"
  | "unsupportedName"
  | "symlinkRejected"
  | "notRegularFile"
  | "hardLinkRejected"
  | "fileTooLarge"
  | "aggregateLimitReached"
  | "changedDuringRead"
  | "invalidUtf8"
  | "binaryContent"
  | "discoveryLimitReached";

export interface RepositoryContextSnapshot {
  projectId: string;
  trust: "verified";
  repository: {
    source: "gitStatusPorcelainV2";
    refresh: ContextRefreshStatus;
    worktree: "present" | "notRepository" | "unknown";
    head?: string;
    branchState: "named" | "detached" | "unborn" | "unknown";
    branch?: string;
    dirty?: boolean;
    ahead?: number;
    behind?: number;
  };
  instructions: {
    source: "projectAgentsMdV1";
    refresh: ContextRefreshStatus;
    files: Array<{
      origin: InstructionOrigin;
      precedence: number;
      byteLen: number;
      sha256: string;
      summary: string;
      visibleContent: string;
      contentTruncated: boolean;
    }>;
    errors: Array<{
      origin?: InstructionOrigin;
      code: InstructionLoadErrorCode;
    }>;
    omittedErrors: number;
    loadedBytes: number;
  };
}

export interface ModelSummary {
  id: string;
  name: string;
  provider: string;
  local: boolean;
  available: boolean;
  reasoning: ReasoningEffort[];
  defaultReasoning?: ReasoningEffort;
  inputModalities: Array<"text" | "image" | "audio" | "document">;
}

export interface SessionSummary {
  id: string;
  projectId: string;
  title: string;
  preview: string;
  status: SessionStatus;
  updatedAt: string;
  pinned: boolean;
  archived: boolean;
  lifecycle: "active" | "archived" | "trash";
  retention?: {
    trashedAtMs: number;
    purgeAfterMs: number;
    permanentDeleteRequiresConfirmation: true;
  };
  forkedFrom?: ConversationBranchProvenance;
  unread: boolean;
  modelId: string;
  attentionCount: number;
}

export interface HostBootstrap {
  protocolVersion: typeof PROTOCOL_VERSION;
  host: {
    id: string;
    name: string;
    connection: "local" | "lan";
  };
  catalogRevision: number;
  selectedSessionId: string;
  projects: ProjectSummary[];
  sessions: SessionSummary[];
  models: ModelSummary[];
  authorityProfiles: AuthorityProfile[];
  themes: ThemeOption[];
  selectedThemeId: string;
  devices: ConnectedDevice[];
  capabilities: {
    attachments: boolean;
    attachmentPolicy?: AttachmentPolicy;
    previews: boolean;
    resources: boolean;
    connectedDevices: boolean;
    lanClients: boolean;
    attachmentIngest: boolean;
    pairDevices: boolean;
    sessionMetadata: boolean;
    sessionBranches: boolean;
    conversationBranching: boolean;
    sessionTrash: boolean;
    sessionExport: boolean;
    documents: boolean;
    trustedProjectFiles: boolean;
    transcriptSearch: boolean;
    themeSelection: boolean;
    steer: boolean;
    followUp: boolean;
  };
}

export interface AttachmentPolicy {
  acceptedMediaTypes: string[];
  maxCount: number;
  maxFileBytes: number;
  maxTotalBytes: number;
}

export interface DocumentReference {
  id: string;
  displayName: string;
  mediaType: "text/plain" | "text/markdown" | "application/pdf";
  sourceByteCount: number;
  extractedTextByteCount: number;
  sha256: string;
  fidelity: "exactUtf8" | "pdfTextOnlyPartial";
  pageCount?: number;
  createdAtMs: number;
}

export type TrustedFileKind =
  | "documentation"
  | "source"
  | "configuration"
  | "text";

export interface TrustedFileEntry {
  id: string;
  relativePath: string;
  displayName: string;
  kind: TrustedFileKind;
  byteLen: number;
}

export interface TrustedFileIndexSummary {
  indexedFiles: number;
  ignoredEntries: number;
  truncated: boolean;
}

export interface TrustedFileCatalog {
  summary: TrustedFileIndexSummary;
  files: TrustedFileEntry[];
}

export interface TrustedFileSearchHit {
  entry: TrustedFileEntry;
  snippet: string;
  line?: number;
}

export interface TrustedFileSearchResult {
  hits: TrustedFileSearchHit[];
  truncated: boolean;
  scannedBytes: number;
}

export interface TrustedFileRead {
  entry: TrustedFileEntry;
  text: string;
  sha256: string;
}

export type TranscriptSearchKind =
  | "user"
  | "assistant"
  | "tool"
  | "error"
  | "attachment";

export interface TranscriptSearchRequest {
  query: string;
  filter: {
    sessionId?: string;
    kinds?: TranscriptSearchKind[];
  };
  limit: number;
}

export interface SearchMatchRange {
  startChar: number;
  endChar: number;
}

export interface TranscriptSearchHit {
  sessionId: string;
  itemId: string;
  kind: TranscriptSearchKind;
  sessionTitle: string;
  snippet: string;
  matchRanges: SearchMatchRange[];
  titleMatchRanges: SearchMatchRange[];
  timestampMs: number;
  score: number;
}

export interface TranscriptSearchResult {
  hits: TranscriptSearchHit[];
  truncated: boolean;
}

interface ItemBase {
  id: string;
  runId?: string;
  turnId: string;
  providerAttempt?: number;
  durableEntryId?: string;
  createdAt: string;
  state: ItemState;
}

export interface UserMessageItem extends ItemBase {
  kind: "user_message";
  content: string;
  attachments?: AttachmentRef[];
  documents?: DocumentReference[];
  projectFiles?: TrustedFileEntry[];
  delivery?: "submit" | "steer" | "followUp";
  branchProvenance?: ConversationBranchProvenance;
}

export interface AssistantMessageItem extends ItemBase {
  kind: "assistant_message";
  content: string;
}

export interface ReasoningItem extends ItemBase {
  kind: "reasoning";
  content: string;
  summary: string;
}

export type ActionKind =
  | "command"
  | "file_read"
  | "file_search"
  | "file_write"
  | "web_search"
  | "skill"
  | "preview"
  | "analysis";

export type ActivityPhase =
  | "investigated"
  | "changed"
  | "verified"
  | "produced"
  | "other";

export type ActionStatus =
  | "running"
  | "succeeded"
  | "failed"
  | "stopped";

export interface ActionPresentation {
  actionKind: ActionKind;
  phase: ActivityPhase;
  status: ActionStatus;
  rawToolName: string;
  label: string;
  summary?: string;
  target?: string;
  detail?: string;
  cwd?: string;
  commandPreview?: string;
  exitCode?: number;
  signal?: number;
  startedAt?: string;
  completedAt?: string;
  durationMs?: number;
  outputSummary?: string;
  outputHandle?: string;
  observedOutputBytes: number;
  droppedOutputBytes: number;
  changedPaths: string[];
  sourceIds: string[];
  outputIds: string[];
}

export interface ActionItem extends ItemBase, ActionPresentation {
  kind: "action";
  originItemId?: string;
  additions?: number;
  deletions?: number;
  diffHandle?: string;
  resultHandle?: string;
}

export interface ApprovalItem extends ItemBase {
  kind: "approval";
  requestId: string;
  title: string;
  description: string;
  scopeLabel: string;
  resolved?: "allowed_once" | "allowed_session" | "denied";
}

export interface UserInputRequestItem extends ItemBase {
  kind: "user_input_request";
  requestId: string;
  prompt: string;
  choices: string[];
  resolved?: "answered" | "denied";
}

export interface RunOutcomeItem extends ItemBase {
  kind: "run_outcome";
  outcome: "done" | "failed" | "stopped";
  durationMs: number;
  summary: string;
  review: CompletionReview;
}

export interface ActivityPhaseSummary {
  phase: ActivityPhase;
  actionCount: number;
  succeededCount: number;
  failedCount: number;
  stoppedCount: number;
}

export type TestFramework =
  | "cargoLibtest"
  | "vitest"
  | "jest"
  | "pytest"
  | "goTest";

export type TestStatus = "passed" | "failed" | "skipped" | "error";

export interface ReportedTestCounts {
  total?: number;
  passed?: number;
  failed?: number;
  skipped?: number;
  errors?: number;
}

export interface StructuredTestCase {
  name: string;
  status: TestStatus;
}

export interface StructuredTestSuite {
  name: string;
  status?: TestStatus;
  reported: ReportedTestCounts;
  cases: StructuredTestCase[];
}

export interface StructuredTestResults {
  originItemId: string;
  framework: TestFramework;
  parser:
    | "cargoLibtestTextV1"
    | "vitestTextV1"
    | "jestTextV1"
    | "pytestTextV1"
    | "goTestTextV1";
  command: {
    status: "succeeded" | "failed" | "stopped";
    exitCode?: number;
    signal?: number;
  };
  verification: "passed" | "failed" | "stopped" | "inconclusive";
  reported: ReportedTestCounts;
  reportedSuites: ReportedTestCounts;
  summaryCount: number;
  suites: StructuredTestSuite[];
  coverage: {
    inputTruncated: boolean;
    recordsTruncated: boolean;
    unsupportedSummaryFields: boolean;
    summaries: "none" | "partial" | "complete";
    cases: "none" | "partial" | "complete";
  };
}

export interface CompletionReview {
  summary: string;
  durationMs: number;
  actionCount: number;
  phases: ActivityPhaseSummary[];
  changedFileItemIds: string[];
  verificationActionItemIds: string[];
  failedActionItemIds: string[];
  warningActionItemIds: string[];
  sourceIds: string[];
  outputIds: string[];
  testResults: StructuredTestResults[];
  evidenceCoverage: "none" | "partial" | "complete";
  openQuestions: string[];
}

export type TranscriptItem =
  | UserMessageItem
  | AssistantMessageItem
  | ReasoningItem
  | ActionItem
  | ApprovalItem
  | UserInputRequestItem
  | RunOutcomeItem;

export interface ProgressStep {
  id: string;
  content: string;
  activeForm: string;
  status: "pending" | "in_progress" | "completed";
}

export interface SourceRef {
  id: string;
  handle?: string;
  originItemId?: string;
  kind: "file" | "web" | "attachment" | "documentation";
  title: string;
  subtitle: string;
  consultedAt: string;
  iconLabel: string;
  excerpt?: string;
  available?: boolean;
}

export interface OutputRef {
  id: string;
  handle?: string;
  originItemId?: string;
  kind: "file" | "image" | "site" | "document";
  title: string;
  subtitle: string;
  mimeType: string;
  updatedAt: string;
  previewId?: string;
  content?: string;
  available?: boolean;
}

export interface PreviewRef {
  id: string;
  title: string;
  kind: "web" | "document" | "image" | "code";
  status: "starting" | "live" | "stopped";
  urlLabel?: string;
  fixtureId?: string;
  outputId?: string;
}

export interface AttachmentRef {
  id: string;
  handle?: string;
  name: string;
  mediaType: string;
  size: number;
}

export type SessionBranchEntryKind =
  | "userMessage"
  | "assistantMessage"
  | "compaction"
  | "internal";

export interface SessionBranchEntry {
  entryId: string;
  parentEntryId?: string;
  kind: SessionBranchEntryKind;
  checkoutable: boolean;
  label: string;
}

export interface SessionBranchGraph {
  head?: string;
  entries: SessionBranchEntry[];
  truncated: boolean;
}

export type ConversationBranchOperation =
  | "editUserTurn"
  | "retryResponse"
  | "forkSession";

export interface BranchModelSelection {
  provider: string;
  model: string;
  reasoning: ReasoningEffort;
}

export interface ConversationBranchProvenance {
  operation: ConversationBranchOperation;
  sourceSessionId: string;
  sourceEntryId: string;
  originatingUserEntryId?: string;
  modelOverride?: BranchModelSelection;
  externalEffectsPreserved: true;
  warning: string;
}

export interface SessionSnapshot {
  sessionId: string;
  actorGeneration: number;
  sequence: number;
  title: string;
  status: SessionStatus;
  activeRunId?: string;
  projectId: string;
  modelId: string;
  reasoning: ReasoningEffort;
  authority: AuthorityProfile;
  contextPercent: number;
  startedAt: string;
  branches: SessionBranchGraph;
  items: TranscriptItem[];
  progress: ProgressStep[];
  sources: SourceRef[];
  outputs: OutputRef[];
  previews: PreviewRef[];
}

export type SessionEvent =
  | {
      type: "session.snapshot";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      snapshot: SessionSnapshot;
    }
  | {
      type: "session.updated";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      patch: Partial<
        Pick<
          SessionSnapshot,
          | "title"
          | "status"
          | "activeRunId"
          | "modelId"
          | "reasoning"
          | "authority"
          | "contextPercent"
        >
      >;
    }
  | {
      type: "item.started";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      item: TranscriptItem;
    }
  | {
      type: "item.delta";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      itemId: string;
      field: "content" | "detail";
      delta: string;
      replace?: boolean;
    }
  | {
      type: "item.committed";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      item: TranscriptItem;
    }
  | {
      type: "item.retracted";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      itemId: string;
    }
  | {
      type: "item.activity";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      itemId: string;
      activity: ActionPresentation;
    }
  | {
      type: "item.activity_result";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      itemId: string;
      resultItemId: string;
      result: Pick<
        ActionPresentation,
        | "status"
        | "summary"
        | "exitCode"
        | "signal"
        | "completedAt"
        | "durationMs"
        | "outputSummary"
        | "outputHandle"
        | "observedOutputBytes"
        | "droppedOutputBytes"
      >;
    }
  | {
      type: "session.branchEntriesAppended";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      entries: SessionBranchEntry[];
    }
  | {
      type: "session.durableHeadChanged";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      durableHead?: string;
    }
  | {
      type: "session.projectionReplaced";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      durableHead?: string;
    }
  | {
      type: "session.resources";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      merge?: boolean;
      progress?: ProgressStep[];
      sources?: SourceRef[];
      outputs?: OutputRef[];
      previews?: PreviewRef[];
    };

export type HostEvent =
  | SessionEvent
  | {
      type: "catalog.summary";
      catalogRevision: number;
      summary: SessionSummary;
    };

export type ClientCommand =
  | {
      id: string;
      type: "session.create";
      projectId: string;
      modelId: string;
      reasoning: ReasoningEffort;
      authority: AuthorityProfile;
    }
  | {
      id: string;
      type: "project.import";
      candidateId: string;
      displayName?: string;
    }
  | {
      id: string;
      type: "project.rename";
      projectId: string;
      displayName: string;
    }
  | {
      id: string;
      type: "project.setDefault";
      projectId: string;
    }
  | {
      id: string;
      type: "project.clearDefault";
    }
  | {
      id: string;
      type: "project.setTrust";
      projectId: string;
      trusted: boolean;
    }
  | {
      id: string;
      type: "project.archive";
      projectId: string;
    }
  | {
      id: string;
      type: "session.submit";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
      documentIds?: string[];
      projectFileIds?: string[];
    }
  | {
      id: string;
      type: "session.steer";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
      documentIds?: string[];
      projectFileIds?: string[];
    }
  | {
      id: string;
      type: "session.followUp";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
      documentIds?: string[];
      projectFileIds?: string[];
    }
  | {
      id: string;
      type: "session.interrupt";
      sessionId: string;
    }
  | {
      id: string;
      type: "session.configure";
      sessionId: string;
      modelId?: string;
      reasoning?: ReasoningEffort;
      authority?: AuthorityProfile;
    }
  | {
      id: string;
      type: "session.rename";
      sessionId: string;
      title: string;
    }
  | {
      id: string;
      type: "session.pin";
      sessionId: string;
      pinned: boolean;
    }
  | {
      id: string;
      type: "session.archive";
      sessionId: string;
      archived: boolean;
    }
  | {
      id: string;
      type: "session.checkout";
      sessionId: string;
      entryId: string;
    }
  | {
      id: string;
      type: "session.editUserTurn";
      sessionId: string;
      sourceUserEntryId: string;
      prompt: string;
      attachments: AttachmentRef[];
      documentIds?: string[];
      projectFileIds?: string[];
    }
  | {
      id: string;
      type: "session.retryResponse";
      sessionId: string;
      sourceAssistantEntryId: string;
      modelId?: string;
      reasoning?: ReasoningEffort;
    }
  | {
      id: string;
      type: "session.forkConversation";
      sessionId: string;
      entryId: string;
    }
  | {
      id: string;
      type: "session.setLifecycle";
      sessionId: string;
      lifecycle: "active" | "archived" | "trash";
    }
  | {
      id: string;
      type: "session.deletePermanently";
      sessionId: string;
      confirmation: {
        sessionId: string;
        trashedAtMs: number;
        phrase: string;
      };
    }
  | {
      id: string;
      type: "approval.resolve";
      sessionId: string;
      requestId: string;
      decision: "allowed_once" | "allowed_session" | "denied";
    }
  | {
      id: string;
      type: "userInput.resolve";
      sessionId: string;
      requestId: string;
      answer:
        | { type: "text"; text: string }
        | { type: "choice"; choice: string };
    }
  | {
      id: string;
      type: "theme.select";
      themeId: string;
    };

export type CommandErrorCode =
  | "incompatibleProtocol"
  | "invalidCommand"
  | "commandIdConflict"
  | "staleGeneration"
  | "notFound"
  | "alreadyResolved"
  | "replayGap"
  | "payloadTooLarge"
  | "unauthorized"
  | "invalidBoundary"
  | "locked"
  | "unavailable"
  | "internal";

export interface CommandAck {
  commandId: string;
  accepted: boolean;
  error?: string;
  errorCode?: CommandErrorCode;
  retryable?: boolean;
  currentGeneration?: number;
  createdSessionId?: string;
  project?: ProjectSummary;
  catalogChanged?: boolean;
}

export interface ConnectedDevice {
  id: string;
  name: string;
  platform: "macOS" | "iOS" | "Android" | "Linux";
  status: "this_device" | "connected" | "offline";
  lastSeen: string;
  connection: "local" | "lan";
}
