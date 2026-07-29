import {
  fixtureBootstrap,
  fixtureSessions,
} from "./fixtures";
import type {
  AttachmentRef,
  ClientCommand,
  CommandAck,
  DocumentReference,
  HostBootstrap,
  HostEvent,
  LifetimeUsage,
  ModelSummary,
  ProjectCatalog,
  RepositoryContextSnapshot,
  SessionEvent,
  SessionSnapshot,
  SessionSummary,
  TranscriptSearchRequest,
  TranscriptSearchResult,
  TrustedFileCatalog,
  TrustedFileRead,
  TrustedFileSearchResult,
  UsageActivity,
  UsagePeriod,
  UsageStats,
} from "./protocol";
import {
  primeSessionItemIndex,
  reduceSessionEvent,
} from "./reducer";
import {
  decodeWireCommandAck,
  encodeClientCommand,
  projectHostBootstrap,
  projectProjectCatalog,
  projectRepositoryContext,
  projectLifetimeUsage,
  projectUsageActivity,
  projectUsageStats,
  projectHostStreamEvent,
  projectDocumentReference,
  projectReplayResponse,
  projectSessionSnapshot,
  projectTranscriptSearchResult,
  projectTrustedFileCatalog,
  projectTrustedFileRead,
  projectTrustedFileSearchResult,
} from "./wire";

type EventListener = (event: HostEvent) => void;
export type TransportConnectionState =
  | "connecting"
  | "connected"
  | "reconnecting";
type ConnectionListener = (state: TransportConnectionState) => void;

export interface YggTransport {
  getProjectCatalog(): Promise<ProjectCatalog>;
  getRepositoryContext(projectId: string): Promise<RepositoryContextSnapshot>;
  getUsageStats(period: UsagePeriod): Promise<UsageStats>;
  getUsageLifetime(): Promise<LifetimeUsage>;
  getUsageActivity(): Promise<UsageActivity>;
  connect(selectedSessionId?: string): Promise<HostBootstrap>;
  getSession(sessionId: string, signal?: AbortSignal): Promise<SessionSnapshot>;
  send(command: ClientCommand): Promise<CommandAck>;
  ingestAttachment(file: File): Promise<AttachmentRef>;
  ingestDocument(sessionId: string, file: File): Promise<DocumentReference>;
  listDocuments(sessionId: string): Promise<DocumentReference[]>;
  getTrustedFiles(projectId: string): Promise<TrustedFileCatalog>;
  searchTrustedFiles(
    projectId: string,
    query: string,
  ): Promise<TrustedFileSearchResult>;
  readTrustedFile(
    projectId: string,
    entryId: string,
  ): Promise<TrustedFileRead>;
  searchTranscripts(
    request: TranscriptSearchRequest,
  ): Promise<TranscriptSearchResult>;
  attachmentContentUrl(handle: string): string;
  resourceContentUrl(sessionId: string, handle: string): string;
  subscribe(listener: EventListener): () => void;
  subscribeConnection?(listener: ConnectionListener): () => void;
  close(): void;
}

const clone = <T,>(value: T): T => structuredClone(value);

export class FixtureTransport implements YggTransport {
  private bootstrap = clone(fixtureBootstrap);
  private sessions = clone(fixtureSessions);
  private listeners = new Set<EventListener>();
  private timers = new Set<number>();
  private createdCount = 0;
  private attachmentFiles = new Map<string, File>();
  private attachmentUrls = new Map<string, string>();
  private documents = new Map<string, DocumentReference[]>();

  async getProjectCatalog(): Promise<ProjectCatalog> {
    return {
      host: {
        id: this.bootstrap.host.id,
        name: this.bootstrap.host.name,
      },
      catalogRevision: this.bootstrap.catalogRevision,
      lifecycleMutationsSupported: true,
      importSupported: false,
      projects: clone(this.bootstrap.projects),
    };
  }

  async getRepositoryContext(
    projectId: string,
  ): Promise<RepositoryContextSnapshot> {
    const known = this.bootstrap.projects.some(
      (project) =>
        project.id === projectId &&
        project.trusted &&
        project.available &&
        !project.archived,
    );
    if (!known) throw new Error("Explicit project trust is required.");
    const refreshedAtUnixMs = Date.now();
    return {
      projectId,
      trust: "verified",
      repository: {
        source: "gitStatusPorcelainV2",
        refresh: {
          state: "current",
          refreshedAtUnixMs,
          durationMs: 12,
          truncated: false,
        },
        worktree: "present",
        head: "aa56661f9b1f14933b57848d08144e8604e2e9cb",
        branchState: "named",
        branch: "explore/ygg-serve-web-v2",
        dirty: true,
        ahead: 0,
        behind: 0,
      },
      instructions: {
        source: "projectAgentsMdV1",
        refresh: {
          state: "current",
          refreshedAtUnixMs,
          durationMs: 3,
          truncated: false,
        },
        files: [],
        errors: [],
        omittedErrors: 0,
        loadedBytes: 0,
      },
    };
  }

  async getUsageStats(period: UsagePeriod): Promise<UsageStats> {
    const multiplier = period === "daily" ? 1 : 5;
    return {
      period,
      promptTokens: 82_000 * multiplier,
      completionTokens: 38_000 * multiplier,
      cacheReadTokens: 54_000 * multiplier,
      cacheWriteTokens: 12_000 * multiplier,
      cacheWriteOneHourTokens: 2_000 * multiplier,
      reasoningTokens: 9_000 * multiplier,
      totalTokens: 186_000 * multiplier,
      requestCount: 7 * multiplier,
    };
  }

  async getUsageLifetime(): Promise<LifetimeUsage> {
    const now = Date.now();
    return {
      promptTokens: 8_200_000,
      completionTokens: 3_800_000,
      cacheReadTokens: 5_400_000,
      cacheWriteTokens: 1_200_000,
      cacheWriteOneHourTokens: 200_000,
      reasoningTokens: 900_000,
      totalTokens: 18_600_000,
      requestCount: 700,
      firstRequestAtMs: now - 120 * 86_400_000,
      lastRequestAtMs: now,
    };
  }

  async getUsageActivity(): Promise<UsageActivity> {
    const today = new Date();
    const todayUtc = Date.UTC(
      today.getUTCFullYear(),
      today.getUTCMonth(),
      today.getUTCDate(),
    );
    const days = Array.from({ length: 120 }, (_, index) => {
      const offset = 119 - index;
      if ((offset * 11) % 17 > 11 && offset > 2) return null;
      return {
        date: new Date(todayUtc - offset * 86_400_000)
          .toISOString()
          .slice(0, 10),
        tokens: 18_000 + ((offset * 37_913) % 240_000),
        requestCount: 1 + ((offset * 7) % 12),
      };
    }).filter((day): day is UsageActivity["days"][number] => day !== null);
    return {
      days,
      currentStreak: 3,
      longestStreak: 12,
    };
  }

  async connect(selectedSessionId?: string): Promise<HostBootstrap> {
    if (selectedSessionId) {
      if (!this.sessions[selectedSessionId]) {
        throw new Error(`Unknown fixture session ${selectedSessionId}`);
      }
      this.bootstrap.selectedSessionId = selectedSessionId;
    }
    return clone(this.bootstrap);
  }

  async getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
    const snapshot = this.sessions[sessionId];
    if (!snapshot) {
      throw new Error(`Unknown fixture session ${sessionId}`);
    }
    return clone(snapshot);
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  subscribeConnection(listener: ConnectionListener): () => void {
    listener("connected");
    return () => {};
  }

  close(): void {
    for (const timer of this.timers) {
      window.clearTimeout(timer);
    }
    this.timers.clear();
    this.listeners.clear();
    for (const url of this.attachmentUrls.values()) URL.revokeObjectURL(url);
    this.attachmentFiles.clear();
    this.attachmentUrls.clear();
    this.documents.clear();
  }

  async ingestAttachment(file: File): Promise<AttachmentRef> {
    const identity = `${file.name}\0${file.type}\0${file.size}`;
    let hash = 2_166_136_261;
    for (let index = 0; index < identity.length; index += 1) {
      hash ^= identity.charCodeAt(index);
      hash = Math.imul(hash, 16_777_619);
    }
    const handle = `fixture-${(hash >>> 0).toString(16).padStart(8, "0")}`;
    this.attachmentFiles.set(handle, file);
    return {
      id: handle,
      handle,
      name: file.name,
      mediaType: file.type || "application/octet-stream",
      size: file.size,
    };
  }

  async ingestDocument(
    sessionId: string,
    file: File,
  ): Promise<DocumentReference> {
    const identity = `${sessionId}\0${file.name}\0${file.type}\0${file.size}`;
    let hash = 2_166_136_261;
    for (let index = 0; index < identity.length; index += 1) {
      hash ^= identity.charCodeAt(index);
      hash = Math.imul(hash, 16_777_619);
    }
    const mediaType =
      file.type === "application/pdf"
        ? "application/pdf"
        : file.type === "text/markdown" ||
            /\.(?:md|markdown)$/iu.test(file.name)
          ? "text/markdown"
          : "text/plain";
    const reference: DocumentReference = {
      id: `doc_${(hash >>> 0).toString(16).padStart(32, "0")}`,
      displayName: file.name,
      mediaType,
      sourceByteCount: file.size,
      extractedTextByteCount: file.size,
      sha256: (hash >>> 0).toString(16).padStart(64, "0"),
      fidelity:
        mediaType === "application/pdf" ? "pdfTextOnlyPartial" : "exactUtf8",
      pageCount: mediaType === "application/pdf" ? 1 : undefined,
      createdAtMs: Date.now(),
    };
    const current = this.documents.get(sessionId) ?? [];
    this.documents.set(sessionId, [...current, reference]);
    return clone(reference);
  }

  async listDocuments(sessionId: string): Promise<DocumentReference[]> {
    return clone(this.documents.get(sessionId) ?? []);
  }

  async getTrustedFiles(projectId: string): Promise<TrustedFileCatalog> {
    if (!projectId.trim()) {
      throw new Error("A fixture project id is required.");
    }
    const files = [
      {
        id: "file_11111111111111111111111111111111",
        relativePath: "README.md",
        displayName: "README.md",
        kind: "documentation" as const,
        byteLen: 1_024,
      },
      {
        id: "file_22222222222222222222222222222222",
        relativePath: "src/lib.rs",
        displayName: "lib.rs",
        kind: "source" as const,
        byteLen: 2_048,
      },
    ];
    return {
      summary: {
        indexedFiles: files.length,
        ignoredEntries: 3,
        truncated: false,
      },
      files,
    };
  }

  async searchTrustedFiles(
    projectId: string,
    query: string,
  ): Promise<TrustedFileSearchResult> {
    const catalog = await this.getTrustedFiles(projectId);
    const needle = query.toLocaleLowerCase();
    return {
      hits: catalog.files
        .filter((entry) =>
          entry.relativePath.toLocaleLowerCase().includes(needle),
        )
        .map((entry) => ({
          entry,
          snippet: `Fixture match in ${entry.relativePath}`,
        })),
      truncated: false,
      scannedBytes: catalog.files.reduce(
        (total, entry) => total + entry.byteLen,
        0,
      ),
    };
  }

  async readTrustedFile(
    projectId: string,
    entryId: string,
  ): Promise<TrustedFileRead> {
    const catalog = await this.getTrustedFiles(projectId);
    const entry = catalog.files.find((candidate) => candidate.id === entryId);
    if (!entry) throw new Error("Project file is not available.");
    return {
      entry,
      text: `Fixture snapshot for ${entry.relativePath}\n`,
      sha256: entry.id.slice(5).padEnd(64, "0"),
    };
  }

  async searchTranscripts(
    request: TranscriptSearchRequest,
  ): Promise<TranscriptSearchResult> {
    const query = request.query.trim().toLocaleLowerCase();
    const hits = Object.values(this.sessions).flatMap((session) =>
      session.items.flatMap((item) => {
        const projected =
          item.kind === "user_message"
            ? { kind: "user" as const, text: item.content }
            : item.kind === "assistant_message"
              ? { kind: "assistant" as const, text: item.content }
              : item.kind === "action"
                ? {
                    kind:
                      item.status === "failed"
                        ? ("error" as const)
                        : ("tool" as const),
                    text: [item.label, item.summary, item.target]
                      .filter(Boolean)
                      .join("\n"),
                  }
                : item.kind === "run_outcome" && item.outcome === "failed"
                  ? { kind: "error" as const, text: item.summary }
                  : undefined;
        if (
          !projected ||
          !projected.text.toLocaleLowerCase().includes(query) ||
          (request.filter.sessionId &&
            request.filter.sessionId !== session.sessionId) ||
          (request.filter.kinds?.length &&
            !request.filter.kinds.includes(projected.kind))
        ) {
          return [];
        }
        const start = projected.text.toLocaleLowerCase().indexOf(query);
        return [
          {
            sessionId: session.sessionId,
            itemId: item.id,
            kind: projected.kind,
            sessionTitle:
              this.bootstrap.sessions.find(
                (summary) => summary.id === session.sessionId,
              )?.title ?? "Session",
            snippet: projected.text,
            matchRanges: [
              { startChar: start, endChar: start + [...query].length },
            ],
            titleMatchRanges: [],
            timestampMs: Date.parse(item.createdAt),
            score: 100,
          },
        ];
      }),
    );
    return {
      hits: hits.slice(0, request.limit),
      truncated: hits.length > request.limit,
    };
  }

  attachmentContentUrl(handle: string): string {
    const existing = this.attachmentUrls.get(handle);
    if (existing) return existing;
    const file = this.attachmentFiles.get(handle);
    if (!file) return "";
    const url = URL.createObjectURL(file);
    this.attachmentUrls.set(handle, url);
    return url;
  }

  resourceContentUrl(sessionId: string, handle: string): string {
    return `/api/v1/sessions/${encodeURIComponent(sessionId)}/resources/${encodeURIComponent(handle)}`;
  }

  private emit(event: SessionEvent): void {
    const current = this.sessions[event.sessionId];
    if (current) {
      if (event.type === "session.snapshot") {
        this.sessions[event.sessionId] = clone(event.snapshot);
      } else if (event.type === "session.updated") {
        this.sessions[event.sessionId] = {
          ...current,
          ...event.patch,
          sequence: event.sequence,
        };
      } else if (event.type === "item.started") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: [...current.items, clone(event.item)],
        };
      } else if (event.type === "item.delta") {
        primeSessionItemIndex(current);
        this.sessions[event.sessionId] = reduceSessionEvent(current, event);
      } else if (event.type === "item.committed") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.map((item) =>
            item.id === event.item.id ? clone(event.item) : item,
          ),
        };
      } else if (event.type === "item.retracted") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.filter((item) => item.id !== event.itemId),
        };
      } else if (event.type === "item.activity") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.map((item) =>
            item.id === event.itemId && item.kind === "action"
              ? {
                  ...item,
                  ...event.activity,
                  detail:
                    event.activity.outputSummary ??
                    event.activity.summary,
                  state:
                    event.activity.status === "running"
                      ? "streaming"
                      : event.activity.status === "failed"
                        ? "failed"
                        : event.activity.status === "stopped"
                          ? "stopped"
                          : "committed",
                }
              : item,
          ),
        };
      } else if (event.type === "item.activity_result") {
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          items: current.items.map((item) =>
            item.id === event.itemId && item.kind === "action"
              ? {
                  ...item,
                  ...event.result,
                  detail:
                    event.result.outputSummary ?? event.result.summary,
                  state:
                    event.result.status === "running"
                      ? "streaming"
                      : event.result.status === "failed"
                        ? "failed"
                        : event.result.status === "stopped"
                          ? "stopped"
                          : "committed",
                }
              : item,
          ),
        };
      } else if (event.type === "session.resources") {
        const mergeById = <T extends { id: string }>(
          currentItems: T[],
          incomingItems: T[] | undefined,
        ): T[] => {
          if (!incomingItems) return currentItems;
          if (!event.merge) return clone(incomingItems);
          const merged = new Map(
            currentItems.map((item) => [item.id, item]),
          );
          for (const item of incomingItems) merged.set(item.id, clone(item));
          return [...merged.values()];
        };
        this.sessions[event.sessionId] = {
          ...current,
          sequence: event.sequence,
          progress: mergeById(current.progress, event.progress),
          sources: mergeById(current.sources, event.sources),
          outputs: mergeById(current.outputs, event.outputs),
          previews: mergeById(current.previews, event.previews),
        };
      }
    }

    for (const listener of this.listeners) {
      listener(clone(event));
    }
  }

  private later(delay: number, callback: () => void): void {
    const timer = window.setTimeout(() => {
      this.timers.delete(timer);
      callback();
    }, delay);
    this.timers.add(timer);
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    if (command.type === "theme.select") {
      if (!this.bootstrap.themes.some((theme) => theme.id === command.themeId)) {
        return {
          commandId: command.id,
          accepted: false,
          error: "Theme is not available.",
        };
      }
      this.bootstrap.selectedThemeId = command.themeId;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "project.import") {
      return {
        commandId: command.id,
        accepted: false,
        error: "This host has no active folder-selection grant.",
      };
    }

    if (
      command.type === "project.rename" ||
      command.type === "project.setDefault" ||
      command.type === "project.setTrust" ||
      command.type === "project.archive"
    ) {
      const project = this.bootstrap.projects.find(
        (candidate) => candidate.id === command.projectId,
      );
      if (!project) {
        return {
          commandId: command.id,
          accepted: false,
          error: "Project is not available.",
        };
      }
      if (command.type === "project.rename") {
        project.name = command.displayName.trim();
      } else if (command.type === "project.setDefault") {
        for (const candidate of this.bootstrap.projects) {
          candidate.isDefault = candidate.id === project.id;
        }
      } else if (command.type === "project.setTrust") {
        project.trusted = command.trusted;
      } else {
        project.archived = true;
        project.trusted = false;
        project.isDefault = false;
        project.liveSessionCount = 0;
      }
      this.bootstrap.catalogRevision += 1;
      return {
        commandId: command.id,
        accepted: true,
        project: clone(project),
      };
    }

    if (command.type === "project.clearDefault") {
      for (const project of this.bootstrap.projects) {
        project.isDefault = false;
      }
      this.bootstrap.catalogRevision += 1;
      return {
        commandId: command.id,
        accepted: true,
        catalogChanged: true,
      };
    }

    if (command.type === "session.create") {
      this.createdCount += 1;
      const sessionId = `session-created-${this.createdCount}`;
      const now = new Date().toISOString();
      const snapshot: SessionSnapshot = {
        sessionId,
        actorGeneration: 1,
        sequence: 1,
        title: "New session",
        status: "idle",
        projectId: command.projectId,
        modelId: command.modelId,
        reasoning: command.reasoning,
        authority: command.authority,
        contextTokens: 0,
        contextPercent: 0,
        startedAt: now,
        branches: { entries: [], truncated: false },
        items: [],
        progress: [],
        sources: [],
        outputs: [],
        previews: [],
      };
      this.sessions[sessionId] = snapshot;
      this.bootstrap.sessions.unshift({
        id: sessionId,
        projectId: command.projectId,
        title: "New session",
        preview: "Ready when you are",
        status: "idle",
        updatedAt: now,
        pinned: false,
        archived: false,
        lifecycle: "active",
        unread: false,
        modelId: command.modelId,
        attentionCount: 0,
      });
      return {
        commandId: command.id,
        accepted: true,
        createdSessionId: sessionId,
      };
    }

    if (command.type === "session.setLifecycle") {
      const summary = this.bootstrap.sessions.find(
        (candidate) => candidate.id === command.sessionId,
      );
      if (!summary) {
        return {
          commandId: command.id,
          accepted: false,
          error: "Session is not available.",
        };
      }
      summary.lifecycle = command.lifecycle;
      summary.archived = command.lifecycle !== "active";
      summary.retention =
        command.lifecycle === "trash"
          ? {
              trashedAtMs: Date.now(),
              purgeAfterMs: Date.now() + 30 * 24 * 60 * 60 * 1_000,
              permanentDeleteRequiresConfirmation: true,
            }
          : undefined;
      this.bootstrap.catalogRevision += 1;
      return {
        commandId: command.id,
        accepted: true,
        catalogChanged: true,
      };
    }

    if (command.type === "session.deletePermanently") {
      const summaryIndex = this.bootstrap.sessions.findIndex(
        (candidate) => candidate.id === command.sessionId,
      );
      const summary = this.bootstrap.sessions[summaryIndex];
      const expectedPhrase = `permanently delete ${command.sessionId}`;
      if (
        !summary ||
        summary.lifecycle !== "trash" ||
        !summary.retention ||
        summary.retention.trashedAtMs !==
          command.confirmation.trashedAtMs ||
        command.confirmation.sessionId !== command.sessionId ||
        command.confirmation.phrase !== expectedPhrase
      ) {
        return {
          commandId: command.id,
          accepted: false,
          error: "Permanent-delete confirmation is stale or incomplete.",
        };
      }
      this.bootstrap.sessions.splice(summaryIndex, 1);
      delete this.sessions[command.sessionId];
      this.bootstrap.catalogRevision += 1;
      return {
        commandId: command.id,
        accepted: true,
        catalogChanged: true,
      };
    }

    const snapshot = this.sessions[command.sessionId];
    if (!snapshot) {
      return {
        commandId: command.id,
        accepted: false,
        error: "Session is not available.",
      };
    }

    if (
      command.type === "session.submit" ||
      command.type === "session.steer" ||
      command.type === "session.followUp"
    ) {
      const turnId = `turn-${snapshot.sequence + 1}`;
      let sequence = snapshot.sequence + 1;
      const now = new Date().toISOString();
      if (
        command.sessionId === "session-performance" &&
        command.prompt === "Stream 60 fixture deltas"
      ) {
        const assistantId = "performance-turn-223-answer";
        this.later(0, () => {
          for (let deltaIndex = 1; deltaIndex <= 60; deltaIndex += 1) {
            this.emit({
              type: "item.delta",
              sessionId: command.sessionId,
              sequence: sequence++,
              itemId: assistantId,
              field: "content",
              delta: ` [stream ${deltaIndex}/60]`,
            });
          }
        });
        return { commandId: command.id, accepted: true };
      }

      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: sequence++,
        patch: {
          status: "working",
          title:
            snapshot.items.length === 0
              ? command.prompt.slice(0, 52) ||
                command.attachments[0]?.name ||
                "New session"
              : snapshot.title,
        },
      });
      this.emit({
        type: "item.started",
        sessionId: command.sessionId,
        sequence: sequence++,
        item: {
          id: `${turnId}-user`,
          turnId,
          kind: "user_message",
          content: command.prompt,
          attachments: command.attachments,
          state: "committed",
          createdAt: now,
        },
      });

      const reasoningId = `${turnId}-reasoning`;
      this.later(260, () => {
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: reasoningId,
            turnId,
            kind: "reasoning",
            summary: "Understanding the request",
            content: "",
            state: "streaming",
            createdAt: new Date().toISOString(),
          },
        });
      });
      this.later(580, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: reasoningId,
          field: "content",
          delta:
            "I’m grounding the request in the current project and checking the most direct path.",
        });
      });
      this.later(1_150, () => {
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: reasoningId,
            turnId,
            kind: "reasoning",
            summary: "Request understood",
            content:
              "I grounded the request in the current project and selected the smallest complete path.",
            state: "committed",
            createdAt: now,
          },
        });
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: `${turnId}-action`,
            turnId,
            kind: "action",
            actionKind: "analysis",
            phase: "investigated",
            status: "succeeded",
            rawToolName: "fixture_context",
            label: "Inspected the project context",
            summary: "Found the relevant session and project state.",
            detail: "Found the relevant session and project state.",
            observedOutputBytes: 0,
            droppedOutputBytes: 0,
            changedPaths: [],
            sourceIds: [],
            outputIds: [],
            state: "committed",
            createdAt: new Date().toISOString(),
          },
        });
      });

      const assistantId = `${turnId}-assistant`;
      this.later(1_650, () => {
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: assistantId,
            turnId,
            kind: "assistant_message",
            content: "",
            state: "streaming",
            createdAt: new Date().toISOString(),
          },
        });
      });
      this.later(1_900, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: assistantId,
          field: "content",
          delta: "I’ve got it. ",
        });
      });
      this.later(2_180, () => {
        this.emit({
          type: "item.delta",
          sessionId: command.sessionId,
          sequence: sequence++,
          itemId: assistantId,
          field: "content",
          delta:
            "The session is connected, the request is grounded, and I’m ready to continue with the real ygg runtime.",
        });
      });
      this.later(2_650, () => {
        const content =
          "I’ve got it. The session is connected, the request is grounded, and I’m ready to continue with the real ygg runtime.";
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: assistantId,
            turnId,
            kind: "assistant_message",
            content,
            state: "committed",
            createdAt: now,
          },
        });
        this.emit({
          type: "session.updated",
          sessionId: command.sessionId,
          sequence: sequence++,
          patch: { status: "done", contextTokens: 8_000, contextPercent: 4 },
        });
        this.emit({
          type: "item.started",
          sessionId: command.sessionId,
          sequence: sequence++,
          item: {
            id: `${turnId}-outcome`,
            turnId,
            kind: "run_outcome",
            outcome: "done",
            durationMs: 2_650,
            summary: "Request completed",
            review: {
              summary: "Request completed",
              durationMs: 2_650,
              actionCount: 1,
              phases: [
                {
                  phase: "investigated",
                  actionCount: 1,
                  succeededCount: 1,
                  failedCount: 0,
                  stoppedCount: 0,
                },
              ],
              changedFileItemIds: [],
              verificationActionItemIds: [],
              failedActionItemIds: [],
              warningActionItemIds: [],
              sourceIds: [],
              outputIds: [],
              testResults: [],
              evidenceCoverage: "none",
              openQuestions: [],
            },
            state: "committed",
            createdAt: new Date().toISOString(),
          },
        });
      });

      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.interrupt") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: { status: "stopped" },
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.configure") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: {
          ...(command.modelId ? { modelId: command.modelId } : {}),
          ...(command.reasoning ? { reasoning: command.reasoning } : {}),
          ...(command.authority ? { authority: command.authority } : {}),
        },
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.rename") {
      this.emit({
        type: "session.updated",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        patch: { title: command.title },
      });
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.title = command.title;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.pin") {
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.pinned = command.pinned;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.archive") {
      const summary = this.bootstrap.sessions.find(
        (session) => session.id === command.sessionId,
      );
      if (summary) summary.archived = command.archived;
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.checkout") {
      if (
        snapshot.activeRunId !== undefined ||
        !["idle", "done", "failed", "stopped"].includes(snapshot.status)
      ) {
        return {
          commandId: command.id,
          accepted: false,
          error:
            "A session branch can only be checked out after current work finishes.",
        };
      }
      const target = snapshot.branches.entries.find(
        (entry) =>
          entry.entryId === command.entryId && entry.checkoutable,
      );
      if (!target) {
        return {
          commandId: command.id,
          accepted: false,
          error: "That session checkpoint is not available for checkout.",
        };
      }
      const sequence = snapshot.sequence + 1;
      let items = snapshot.items;
      let progress = snapshot.progress;
      let sources = snapshot.sources;
      let outputs = snapshot.outputs;
      let previews = snapshot.previews;
      if (command.sessionId === "session-done") {
        const rootItem = snapshot.items.find((item) => item.id === "done-user");
        if (target.entryId === "entry-release-question") {
          items = rootItem ? [rootItem] : [];
          progress = [];
          sources = [];
          outputs = [];
          previews = [];
        } else if (target.entryId === "entry-release-draft") {
          items = [
            ...(rootItem ? [rootItem] : []),
            {
              id: "done-draft",
              turnId: "done-turn",
              kind: "assistant_message",
              content:
                "The initial release assessment is ready, but the focused checks and review artifact have not been completed yet.",
              state: "committed",
              createdAt: snapshot.startedAt,
            },
          ];
          progress = [];
          sources = [];
          outputs = [];
          previews = [];
        }
      }
      this.sessions[command.sessionId] = {
        ...snapshot,
        sequence,
        status: "idle",
        branches: { ...snapshot.branches, head: target.entryId },
        items,
        progress,
        sources,
        outputs,
        previews,
      };
      this.emit({
        type: "session.projectionReplaced",
        sessionId: command.sessionId,
        actorGeneration: snapshot.actorGeneration,
        sequence,
        durableHead: target.entryId,
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.editUserTurn") {
      const source = snapshot.items.find(
        (item) =>
          item.kind === "user_message" &&
          item.durableEntryId === command.sourceUserEntryId,
      );
      if (!source) {
        return {
          commandId: command.id,
          accepted: false,
          error: "That user checkpoint is not available.",
          errorCode: "notFound",
          retryable: false,
        };
      }
      this.emit({
        type: "item.committed",
        sessionId: command.sessionId,
        sequence: snapshot.sequence + 1,
        item: {
          id: `fixture-edit-${snapshot.sequence + 1}`,
          turnId: `fixture-edit-turn-${snapshot.sequence + 1}`,
          durableEntryId: `fixture-entry-edit-${snapshot.sequence + 1}`,
          kind: "user_message",
          content: command.prompt,
          state: "committed",
          createdAt: new Date().toISOString(),
          branchProvenance: {
            operation: "editUserTurn",
            sourceSessionId: command.sessionId,
            sourceEntryId: command.sourceUserEntryId,
            externalEffectsPreserved: true,
            warning:
              "External side effects from the earlier transcript are preserved.",
          },
        },
      });
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.retryResponse") {
      const source = snapshot.items.find(
        (item) =>
          item.kind === "assistant_message" &&
          item.durableEntryId === command.sourceAssistantEntryId,
      );
      if (!source) {
        return {
          commandId: command.id,
          accepted: false,
          error: "That assistant checkpoint is not available.",
          errorCode: "notFound",
          retryable: false,
        };
      }
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "session.forkConversation") {
      const sourceIndex = snapshot.items.findIndex(
        (item) => item.durableEntryId === command.entryId,
      );
      if (sourceIndex < 0) {
        return {
          commandId: command.id,
          accepted: false,
          error: "That conversation checkpoint is not available.",
          errorCode: "notFound",
          retryable: false,
        };
      }
      this.createdCount += 1;
      const sessionId = `session-forked-${this.createdCount}`;
      const now = new Date().toISOString();
      this.sessions[sessionId] = {
        ...clone(snapshot),
        sessionId,
        sequence: 1,
        title: `${snapshot.title} fork`,
        status: "idle",
        activeRunId: undefined,
        startedAt: now,
        branches: { ...clone(snapshot.branches), head: command.entryId },
        items: clone(snapshot.items.slice(0, sourceIndex + 1)),
        progress: [],
      };
      this.bootstrap.sessions.unshift({
        id: sessionId,
        projectId: snapshot.projectId,
        title: `${snapshot.title} fork`,
        preview: "Forked conversation",
        status: "idle",
        updatedAt: now,
        pinned: false,
        archived: false,
        lifecycle: "active",
        forkedFrom: {
          operation: "forkSession",
          sourceSessionId: command.sessionId,
          sourceEntryId: command.entryId,
          externalEffectsPreserved: true,
          warning:
            "External side effects from the source session are preserved.",
        },
        unread: false,
        modelId: snapshot.modelId,
        attentionCount: 0,
      });
      return {
        commandId: command.id,
        accepted: true,
        createdSessionId: sessionId,
      };
    }

    if (command.type === "approval.resolve") {
      const item = snapshot.items.find(
        (candidate) =>
          candidate.kind === "approval" &&
          candidate.requestId === command.requestId,
      );
      if (item?.kind === "approval") {
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 1,
          item: {
            ...item,
            resolved: command.decision,
            state: "committed",
          },
        });
        this.emit({
          type: "session.updated",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 2,
          patch: {
            status: command.decision === "denied" ? "stopped" : "working",
          },
        });
      }
      return { commandId: command.id, accepted: true };
    }

    if (command.type === "userInput.resolve") {
      const item = snapshot.items.find(
        (candidate) =>
          candidate.kind === "user_input_request" &&
          candidate.requestId === command.requestId,
      );
      if (item?.kind === "user_input_request") {
        this.emit({
          type: "item.committed",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 1,
          item: {
            ...item,
            resolved: "answered",
            state: "committed",
          },
        });
        this.emit({
          type: "session.updated",
          sessionId: command.sessionId,
          sequence: snapshot.sequence + 2,
          patch: { status: "working" },
        });
      }
      return { commandId: command.id, accepted: true };
    }

    throw new Error("Unsupported fixture command.");
  }
}

export class HttpTransport implements YggTransport {
  private listeners = new Set<EventListener>();
  private connectionListeners = new Set<ConnectionListener>();
  private connectionState: TransportConnectionState = "connecting";
  private socket: WebSocket | null = null;
  private reconnectTimer: number | null = null;
  private reconnectAttempt = 0;
  private closedByClient = false;
  private replaying = false;
  private bufferedEvents: Array<{
    hostSequence: number;
    event: HostEvent;
  }> = [];
  private hostId: string | null = null;
  private catalogRevision = 0;
  private catalogAnchorSessionId: string | null = null;
  private models: ModelSummary[] = [];
  private summaries = new Map<string, SessionSummary>();
  private actorGenerationBySession: Record<string, number> = {};
  private modelIdBySession: Record<string, string> = {};
  private cursorBySession = new Map<
    string,
    { actorGeneration: number; sequence: number }
  >();
  private replacementBarrierBySession = new Map<
    string,
    { actorGeneration: number; sequence: number }
  >();
  private selectedSessionCache: SessionSnapshot | null = null;
  private encodedCommands = new Map<
    string,
    { hostScoped: boolean; body: string }
  >();

  constructor(private readonly deviceId?: string) {}

  async getProjectCatalog(): Promise<ProjectCatalog> {
    const response = await fetch("/api/v1/projects", {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    });
    if (!response.ok) {
      throw new Error(`Project catalog failed with ${response.status}`);
    }
    const catalog = projectProjectCatalog(await response.json());
    this.hostId = catalog.host.id;
    this.catalogRevision = catalog.catalogRevision;
    return catalog;
  }

  async getRepositoryContext(
    projectId: string,
  ): Promise<RepositoryContextSnapshot> {
    const response = await fetch(
      `/api/v1/projects/${encodeURIComponent(projectId)}/context`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Project context failed with ${response.status}`);
    }
    return projectRepositoryContext(await response.json());
  }

  async getUsageStats(period: UsagePeriod): Promise<UsageStats> {
    const response = await fetch(
      `/api/v1/usage/stats?period=${encodeURIComponent(period)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Usage stats failed with ${response.status}`);
    }
    return projectUsageStats(await response.json());
  }

  async getUsageLifetime(): Promise<LifetimeUsage> {
    const response = await fetch("/api/v1/usage/lifetime", {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    });
    if (!response.ok) {
      throw new Error(`Lifetime usage failed with ${response.status}`);
    }
    return projectLifetimeUsage(await response.json());
  }

  async getUsageActivity(): Promise<UsageActivity> {
    const response = await fetch("/api/v1/usage/activity", {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    });
    if (!response.ok) {
      throw new Error(`Usage activity failed with ${response.status}`);
    }
    return projectUsageActivity(await response.json());
  }

  async connect(selectedSessionId?: string): Promise<HostBootstrap> {
    this.closedByClient = false;
    const request: RequestInit = {
      headers: { Accept: "application/json" },
      credentials: "same-origin",
    };
    const response = selectedSessionId
      ? await fetch(
          `/api/v1/bootstrap?selectedSessionId=${encodeURIComponent(selectedSessionId)}`,
          request,
        )
      : await fetch("/api/v1/bootstrap", request);
    if (!response.ok) {
      throw new Error(`Bootstrap failed with ${response.status}`);
    }
    const { bootstrap, selectedSession } = projectHostBootstrap(
      await response.json(),
    );
    this.hostId = bootstrap.host.id;
    this.catalogRevision = bootstrap.catalogRevision;
    this.catalogAnchorSessionId = bootstrap.selectedSessionId;
    this.models = bootstrap.models;
    this.summaries = new Map(
      bootstrap.sessions.map((summary) => [summary.id, summary]),
    );
    this.selectedSessionCache = selectedSession;
    this.rememberSnapshot(selectedSession);
    this.openSocket();
    return bootstrap;
  }

  async getSession(
    sessionId: string,
    signal?: AbortSignal,
  ): Promise<SessionSnapshot> {
    if (this.selectedSessionCache?.sessionId === sessionId) {
      const cached = this.selectedSessionCache;
      this.selectedSessionCache = null;
      this.assertSnapshotPastReplacementBarrier(cached);
      this.rememberSnapshot(cached);
      return clone(cached);
    }
    const response = await fetch(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        signal,
      },
    );
    if (!response.ok) {
      throw new Error(`Session failed with ${response.status}`);
    }
    const snapshot = projectSessionSnapshot(await response.json(), {
      summary: this.summaries.get(sessionId),
      models: this.models,
    });
    this.assertSnapshotPastReplacementBarrier(snapshot);
    this.rememberSnapshot(snapshot);
    return snapshot;
  }

  async send(command: ClientCommand): Promise<CommandAck> {
    let encoded = this.encodedCommands.get(command.id);
    if (!encoded) {
      const envelope = encodeClientCommand(command, {
        hostId: this.hostId ?? "",
        deviceId: this.deviceId ?? "",
        issuedAtMs: Date.now(),
        actorGenerationBySession: this.actorGenerationBySession,
        modelIdBySession: this.modelIdBySession,
        models: this.models,
      });
      encoded = {
        hostScoped:
          command.type === "session.create" ||
          command.type.startsWith("project.") ||
          command.type === "session.setLifecycle" ||
          command.type === "session.deletePermanently",
        body: JSON.stringify(envelope),
      };
      this.encodedCommands.set(command.id, encoded);
    }

    const request: RequestInit = {
      method: "POST",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: encoded.body,
    };
    const response = encoded.hostScoped
      ? await fetch("/api/v1/commands/host", request)
      : await fetch("/api/v1/commands/session", request);
    if (!response.ok) {
      throw new Error(`Command failed with ${response.status}`);
    }
    const ack = decodeWireCommandAck(await response.json());
    if (ack.commandId !== command.id) {
      throw new Error("The host acknowledged a different command.");
    }
    this.encodedCommands.delete(command.id);
    if (
      command.type === "session.create" &&
      ack.accepted &&
      ack.createdSessionId
    ) {
      this.rememberCreatedSession(command, ack.createdSessionId);
    } else if (
      command.type === "session.forkConversation" &&
      ack.accepted &&
      ack.createdSessionId
    ) {
      this.rememberForkedSession(command, ack.createdSessionId);
    }
    return ack;
  }

  private rememberCreatedSession(
    command: Extract<ClientCommand, { type: "session.create" }>,
    sessionId: string,
  ): void {
    this.summaries.set(sessionId, {
      id: sessionId,
      projectId: command.projectId,
      title: "New session",
      preview: "Ready when you are",
      status: "idle",
      updatedAt: new Date().toISOString(),
      pinned: false,
      archived: false,
      lifecycle: "active",
      unread: false,
      modelId: command.modelId,
      attentionCount: 0,
    });
    this.modelIdBySession[sessionId] = command.modelId;
  }

  private rememberForkedSession(
    command: Extract<
      ClientCommand,
      { type: "session.forkConversation" }
    >,
    sessionId: string,
  ): void {
    const source = this.summaries.get(command.sessionId);
    const modelId =
      this.modelIdBySession[command.sessionId] ??
      source?.modelId ??
      this.models[0]?.id ??
      "";
    this.summaries.set(sessionId, {
      id: sessionId,
      projectId: source?.projectId ?? "",
      title: source ? `${source.title} fork` : "Forked conversation",
      preview: "Forked conversation",
      status: "idle",
      updatedAt: new Date().toISOString(),
      pinned: false,
      archived: false,
      lifecycle: "active",
      forkedFrom: {
        operation: "forkSession",
        sourceSessionId: command.sessionId,
        sourceEntryId: command.entryId,
        externalEffectsPreserved: true,
        warning:
          "External side effects from the source session are preserved.",
      },
      unread: false,
      modelId,
      attentionCount: 0,
    });
    this.modelIdBySession[sessionId] = modelId;
  }

  async ingestAttachment(file: File): Promise<AttachmentRef> {
    const response = await fetch(
      `/api/v1/attachments?displayName=${encodeURIComponent(file.name)}`,
      {
        method: "POST",
        credentials: "same-origin",
        headers: {
          Accept: "application/json",
          "Content-Type": file.type || "application/octet-stream",
        },
        body: file,
      },
    );
    if (!response.ok) {
      throw new Error(`Attachment upload failed with ${response.status}`);
    }
    const value: unknown = await response.json();
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      throw new Error("Attachment upload returned an invalid response.");
    }
    const result = value as Record<string, unknown>;
    if (
      typeof result.handle !== "string" ||
      typeof result.displayName !== "string" ||
      typeof result.mediaType !== "string" ||
      typeof result.byteLen !== "number"
    ) {
      throw new Error("Attachment upload returned an invalid response.");
    }
    return {
      id: result.handle,
      handle: result.handle,
      name: result.displayName,
      mediaType: result.mediaType,
      size: result.byteLen,
    };
  }

  async ingestDocument(
    sessionId: string,
    file: File,
  ): Promise<DocumentReference> {
    const mediaType =
      file.type === "application/pdf"
        ? "application/pdf"
        : file.type === "text/markdown" ||
            /\.(?:md|markdown)$/iu.test(file.name)
          ? "text/markdown"
          : "text/plain";
    const response = await fetch(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/documents?displayName=${encodeURIComponent(file.name)}`,
      {
        method: "POST",
        credentials: "same-origin",
        headers: {
          Accept: "application/json",
          "Content-Type": mediaType,
        },
        body: file,
      },
    );
    if (!response.ok) {
      throw new Error(`Document upload failed with ${response.status}`);
    }
    return projectDocumentReference(await response.json());
  }

  async listDocuments(sessionId: string): Promise<DocumentReference[]> {
    const response = await fetch(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/documents`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Document listing failed with ${response.status}`);
    }
    const value: unknown = await response.json();
    if (!Array.isArray(value)) {
      throw new Error("Document listing returned an invalid response.");
    }
    return value.map((document, index) =>
      projectDocumentReference(document, `documents[${index}]`),
    );
  }

  async getTrustedFiles(projectId: string): Promise<TrustedFileCatalog> {
    const response = await fetch(
      `/api/v1/projects/${encodeURIComponent(projectId)}/files`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Project-file listing failed with ${response.status}`);
    }
    return projectTrustedFileCatalog(await response.json());
  }

  async searchTrustedFiles(
    projectId: string,
    query: string,
  ): Promise<TrustedFileSearchResult> {
    const response = await fetch(
      `/api/v1/projects/${encodeURIComponent(projectId)}/files/search?query=${encodeURIComponent(query)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Project-file search failed with ${response.status}`);
    }
    return projectTrustedFileSearchResult(await response.json());
  }

  async readTrustedFile(
    projectId: string,
    entryId: string,
  ): Promise<TrustedFileRead> {
    const response = await fetch(
      `/api/v1/projects/${encodeURIComponent(projectId)}/files/${encodeURIComponent(entryId)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Project-file read failed with ${response.status}`);
    }
    return projectTrustedFileRead(await response.json());
  }

  async searchTranscripts(
    request: TranscriptSearchRequest,
  ): Promise<TranscriptSearchResult> {
    const response = await fetch("/api/v1/search", {
      method: "POST",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(request),
    });
    if (!response.ok) {
      throw new Error(`Conversation search failed with ${response.status}`);
    }
    return projectTranscriptSearchResult(await response.json());
  }

  attachmentContentUrl(handle: string): string {
    return `/api/v1/attachments/${encodeURIComponent(handle)}`;
  }

  resourceContentUrl(sessionId: string, handle: string): string {
    return `/api/v1/sessions/${encodeURIComponent(sessionId)}/resources/${encodeURIComponent(handle)}`;
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  subscribeConnection(listener: ConnectionListener): () => void {
    this.connectionListeners.add(listener);
    listener(this.connectionState);
    return () => this.connectionListeners.delete(listener);
  }

  private setConnectionState(state: TransportConnectionState): void {
    if (this.connectionState === state) return;
    this.connectionState = state;
    for (const listener of this.connectionListeners) listener(state);
  }

  close(): void {
    this.closedByClient = true;
    if (this.reconnectTimer !== null) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.socket?.close();
    this.socket = null;
    this.bufferedEvents = [];
    this.replacementBarrierBySession.clear();
    this.listeners.clear();
    this.connectionListeners.clear();
  }

  private openSocket(): void {
    if (this.closedByClient) return;
    const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(
      `${scheme}//${window.location.host}/api/v1/events`,
    );
    this.socket = socket;

    socket.addEventListener("open", () => {
      this.reconnectAttempt = 0;
      this.setConnectionState("connected");
      this.replaying = true;
      void Promise.all([this.replayAll(), this.refreshCatalog()]).then(
        () => {
          if (this.socket !== socket) return;
          this.replaying = false;
          const buffered = this.bufferedEvents
            .splice(0)
            .sort((left, right) => left.hostSequence - right.hostSequence);
          for (const projection of buffered) {
            this.dispatch(projection.event);
          }
        },
        () => {
          if (this.socket === socket) socket.close();
        },
      );
    });

    socket.addEventListener("message", (message) => {
      try {
        const projection = projectHostStreamEvent(
          JSON.parse(String(message.data)),
          { models: this.models },
        );
        this.rememberEvent(projection.event);
        if (this.replaying) {
          this.bufferedEvents.push(projection);
        } else {
          this.dispatch(projection.event);
        }
      } catch {
        socket.close(1002, "Invalid ygg event");
      }
    });

    socket.addEventListener("close", () => {
      if (this.socket === socket) this.socket = null;
      if (this.closedByClient) return;
      this.setConnectionState("reconnecting");
      const delay = Math.min(5_000, 250 * 2 ** this.reconnectAttempt);
      this.reconnectAttempt += 1;
      this.reconnectTimer = window.setTimeout(() => {
        this.reconnectTimer = null;
        this.openSocket();
      }, delay);
    });
  }

  private dispatch(event: HostEvent): void {
    for (const listener of this.listeners) listener(event);
  }

  private cursorAtOrAfter(
    cursor: { actorGeneration: number; sequence: number },
    required: { actorGeneration: number; sequence: number },
  ): boolean {
    return (
      cursor.actorGeneration > required.actorGeneration ||
      (cursor.actorGeneration === required.actorGeneration &&
        cursor.sequence >= required.sequence)
    );
  }

  private assertSnapshotPastReplacementBarrier(
    snapshot: SessionSnapshot,
  ): void {
    const required = this.replacementBarrierBySession.get(snapshot.sessionId);
    if (
      required &&
      !this.cursorAtOrAfter(
        {
          actorGeneration: snapshot.actorGeneration,
          sequence: snapshot.sequence,
        },
        required,
      )
    ) {
      throw new Error(
        "Session snapshot predates the required projection replacement.",
      );
    }
  }

  private rememberSnapshot(snapshot: SessionSnapshot): void {
    this.actorGenerationBySession[snapshot.sessionId] =
      snapshot.actorGeneration;
    this.modelIdBySession[snapshot.sessionId] = snapshot.modelId;
    const required = this.replacementBarrierBySession.get(snapshot.sessionId);
    if (
      required &&
      !this.cursorAtOrAfter(
        {
          actorGeneration: snapshot.actorGeneration,
          sequence: snapshot.sequence,
        },
        required,
      )
    ) {
      return;
    }
    if (required) {
      this.replacementBarrierBySession.delete(snapshot.sessionId);
    }
    this.cursorBySession.set(snapshot.sessionId, {
      actorGeneration: snapshot.actorGeneration,
      sequence: snapshot.sequence,
    });
  }

  private rememberEvent(event: HostEvent): void {
    if (event.type === "catalog.summary") {
      this.catalogRevision = Math.max(
        this.catalogRevision,
        event.catalogRevision,
      );
      this.summaries.set(event.summary.id, event.summary);
      this.modelIdBySession[event.summary.id] = event.summary.modelId;
      return;
    }
    const generation = event.actorGeneration;
    if (generation !== undefined) {
      this.actorGenerationBySession[event.sessionId] = generation;
    }
    if (event.type === "session.projectionReplaced") {
      const required = {
        actorGeneration:
          generation ??
          this.actorGenerationBySession[event.sessionId] ??
          this.cursorBySession.get(event.sessionId)?.actorGeneration ??
          0,
        sequence: event.sequence,
      };
      const existing = this.replacementBarrierBySession.get(event.sessionId);
      if (!existing || this.cursorAtOrAfter(required, existing)) {
        this.replacementBarrierBySession.set(event.sessionId, required);
      }
    }
    if (
      generation !== undefined &&
      !this.replacementBarrierBySession.has(event.sessionId)
    ) {
      this.cursorBySession.set(event.sessionId, {
        actorGeneration: generation,
        sequence: event.sequence,
      });
    }
    if (event.type === "session.snapshot") {
      this.rememberSnapshot(event.snapshot);
    } else if (event.type === "session.updated" && event.patch.modelId) {
      this.modelIdBySession[event.sessionId] = event.patch.modelId;
    }
  }

  private async refreshCatalog(): Promise<void> {
    const anchor =
      this.catalogAnchorSessionId ?? this.cursorBySession.keys().next().value;
    if (!anchor) return;
    const response = await fetch(
      `/api/v1/bootstrap?selectedSessionId=${encodeURIComponent(anchor)}`,
      {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      },
    );
    if (!response.ok) {
      throw new Error(`Catalog refresh failed with ${response.status}`);
    }
    const { bootstrap } = projectHostBootstrap(await response.json());
    if (bootstrap.catalogRevision <= this.catalogRevision) return;

    const previous = this.summaries;
    this.catalogRevision = bootstrap.catalogRevision;
    this.models = bootstrap.models;
    this.summaries = new Map(
      bootstrap.sessions.map((summary) => [summary.id, summary]),
    );
    for (const summary of bootstrap.sessions) {
      const prior = previous.get(summary.id);
      if (prior && JSON.stringify(prior) === JSON.stringify(summary)) {
        continue;
      }
      this.dispatch({
        type: "catalog.summary",
        catalogRevision: bootstrap.catalogRevision,
        summary,
      });
    }
  }

  private async replayAll(): Promise<void> {
    const cursors = [...this.cursorBySession.entries()];
    await Promise.all(
      cursors.map(async ([sessionId, cursor]) => {
        const query = new URLSearchParams({
          actorGeneration: String(cursor.actorGeneration),
          sequence: String(cursor.sequence),
        });
        const response = await fetch(
          `/api/v1/sessions/${encodeURIComponent(sessionId)}/replay?${query}`,
          {
            headers: { Accept: "application/json" },
            credentials: "same-origin",
          },
        );
        if (!response.ok) {
          throw new Error(`Replay failed with ${response.status}`);
        }
        const replay = projectReplayResponse(await response.json(), {
          summary: this.summaries.get(sessionId),
          models: this.models,
        });
        if (replay.type === "gap") {
          this.rememberSnapshot(replay.snapshot);
          this.dispatch({
            type: "session.snapshot",
            sessionId: replay.snapshot.sessionId,
            actorGeneration: replay.snapshot.actorGeneration,
            sequence: replay.snapshot.sequence,
            snapshot: replay.snapshot,
          });
          return;
        }
        for (const event of replay.events) {
          this.rememberEvent(event);
          this.dispatch(event);
        }
        if (
          replay.events.length === 0 &&
          !this.replacementBarrierBySession.has(sessionId)
        ) {
          this.actorGenerationBySession[sessionId] = replay.actorGeneration;
          this.cursorBySession.set(sessionId, {
            actorGeneration: replay.actorGeneration,
            sequence: replay.sequence,
          });
        }
      }),
    );
  }
}

const loopbackDeviceStorageKey = "ygg:loopback-device-id";
const validDeviceId = /^[A-Za-z0-9_.:-]{1,128}$/;
let volatileLoopbackDeviceId: string | undefined;

export type TransportMode = "fixture" | "live";

export function transportModeFromSearch(search: string): TransportMode {
  // Fixture transport is a development-only surface. Keep this compile-time
  // guard here, at the mode boundary, so a production URL can never opt back
  // into simulated sessions with a query parameter.
  if (!import.meta.env.DEV) return "live";
  return new URLSearchParams(search).get("transport") === "fixture"
    ? "fixture"
    : "live";
}

export function resolveClientDeviceId(): string | undefined {
  const injected =
    document
      .querySelector<HTMLMetaElement>('meta[name="ygg-device-id"]')
      ?.content.trim() ||
    document.documentElement.dataset.yggDeviceId?.trim();
  if (injected && validDeviceId.test(injected)) return injected;

  const host = window.location.hostname;
  if (host !== "localhost" && host !== "127.0.0.1" && host !== "::1") {
    return undefined;
  }
  try {
    const stored = window.localStorage.getItem(loopbackDeviceStorageKey);
    if (stored && validDeviceId.test(stored)) return stored;
  } catch {
    // A stable ID for this page lifetime is still better than an empty ID.
  }
  const generated =
    volatileLoopbackDeviceId ?? `browser-${crypto.randomUUID()}`;
  volatileLoopbackDeviceId = generated;
  try {
    window.localStorage.setItem(loopbackDeviceStorageKey, generated);
  } catch {
    // Storage can be unavailable in a hardened browser.
  }
  return generated;
}

export function createTransport(
  mode = transportModeFromSearch(window.location.search),
): YggTransport {
  // Vite folds import.meta.env.DEV to false for production builds. That makes
  // FixtureTransport and its fixture-data imports unreachable and removable
  // from the production dependency graph.
  if (import.meta.env.DEV && mode === "fixture") {
    return new FixtureTransport();
  }
  return new HttpTransport(resolveClientDeviceId());
}
