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
  pathLabel: string;
  trusted: boolean;
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

interface ItemBase {
  id: string;
  turnId: string;
  createdAt: string;
  state: ItemState;
}

export interface UserMessageItem extends ItemBase {
  kind: "user_message";
  content: string;
  attachments?: AttachmentRef[];
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
  | "file_write"
  | "web_search"
  | "preview"
  | "analysis";

export interface ActionItem extends ItemBase {
  kind: "action";
  actionKind: ActionKind;
  label: string;
  target?: string;
  detail?: string;
  durationMs?: number;
  additions?: number;
  deletions?: number;
  sourceIds?: string[];
  outputIds?: string[];
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
  kind: "file" | "web" | "attachment" | "documentation";
  title: string;
  subtitle: string;
  consultedAt: string;
  iconLabel: string;
  excerpt?: string;
}

export interface OutputRef {
  id: string;
  kind: "file" | "image" | "site" | "document";
  title: string;
  subtitle: string;
  mimeType: string;
  updatedAt: string;
  previewId?: string;
  content?: string;
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
      type: "item.tool_result";
      sessionId: string;
      actorGeneration?: number;
      sequence: number;
      itemId: string;
      resultItemId: string;
      detail: string;
      state: ItemState;
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
      type: "session.submit";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
    }
  | {
      id: string;
      type: "session.steer";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
    }
  | {
      id: string;
      type: "session.followUp";
      sessionId: string;
      prompt: string;
      attachments: AttachmentRef[];
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

export interface CommandAck {
  commandId: string;
  accepted: boolean;
  error?: string;
  createdSessionId?: string;
}

export interface ConnectedDevice {
  id: string;
  name: string;
  platform: "macOS" | "iOS" | "Android" | "Linux";
  status: "this_device" | "connected" | "offline";
  lastSeen: string;
  connection: "local" | "lan";
}
