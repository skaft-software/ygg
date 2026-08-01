import {
  Archive,
  ArchiveRestore,
  BarChart3,
  Folder,
  Files,
  GitMerge,
  GitPullRequest,
  GitPullRequestDraft,
  Laptop,
  LayoutDashboard,
  LoaderCircle,
  Menu,
  MessageSquarePlus,
  PanelLeftClose,
  Search,
  Settings,
  Trash2,
  X,
} from "lucide-react";
import {
  type ReactNode,
  Fragment,
  memo,
  useEffect,
  useRef,
  useState,
} from "react";
import { taskNeedsAttention } from "../fleet-status";
import type {
  ProjectSummary,
  SearchMatchRange,
  SessionSummary,
  TranscriptSearchHit,
  TranscriptSearchRequest,
  TranscriptSearchResult,
} from "../protocol";
import { displaySessionTitle } from "../session-title";

interface SidebarProps {
  open: boolean;
  blocked: boolean;
  sessions: SessionSummary[];
  projects?: ProjectSummary[];
  selectedSessionId: string | null;
  surface:
    | "fleet"
    | "session"
    | "projects"
    | "files"
    | "usage"
    | "settings"
    | "devices";
  devicesAvailable: boolean;
  filesAvailable?: boolean;
  onRestoreFocus: () => void;
  onClose: () => void;
  onNewSession: () => void;
  onOpenFleet: () => void;
  onSelectSession: (sessionId: string) => void;
  onRestoreSession: (sessionId: string) => void;
  sessionTrashAvailable?: boolean;
  onSetSessionLifecycle?: (
    sessionId: string,
    lifecycle: SessionSummary["lifecycle"],
  ) => void | Promise<void>;
  onDeleteSessionPermanently?: (
    sessionId: string,
    trashedAtMs: number,
    phrase: string,
  ) => void | Promise<void>;
  onOpenDevices: () => void;
  onOpenProjects: () => void;
  onOpenFiles?: () => void;
  onOpenUsage: () => void;
  onOpenSettings: () => void;
  transcriptSearchAvailable?: boolean;
  onSearchTranscripts?: (
    request: TranscriptSearchRequest,
  ) => Promise<TranscriptSearchResult>;
  onActivateSearchResult?: (sessionId: string, itemId: string) => void;
}

const sessionStatusLabel: Record<SessionSummary["status"], string> = {
  idle: "Ready",
  working: "Working",
  needs_attention: "Needs attention",
  done: "Done",
  failed: "Failed",
  stopped: "Stopped",
  disconnected: "Reconnecting",
};

const pullRequestLabels: Record<
  NonNullable<SessionSummary["pullRequest"]>["state"],
  string
> = {
  in_progress: "Pull request in progress",
  ready: "Pull request ready for review",
  merged: "Pull request merged",
};

const searchKindLabels: Record<TranscriptSearchHit["kind"], string> = {
  user: "User message",
  assistant: "Assistant message",
  tool: "Tool result",
  error: "Error",
  attachment: "Attachment",
};

function searchMatchRanges(
  textLength: number,
  ranges: readonly SearchMatchRange[],
): Array<{ start: number; end: number }> {
  return ranges
    .filter(
      (range) =>
        Number.isFinite(range.startChar) && Number.isFinite(range.endChar),
    )
    .map((range) => ({
      start: Math.max(0, Math.min(textLength, Math.trunc(range.startChar))),
      end: Math.max(0, Math.min(textLength, Math.trunc(range.endChar))),
    }))
    .filter((range) => range.end > range.start)
    .sort((left, right) => left.start - right.start || left.end - right.end)
    .reduce<Array<{ start: number; end: number }>>((merged, range) => {
      const previous = merged.at(-1);
      if (previous && range.start <= previous.end) {
        previous.end = Math.max(previous.end, range.end);
      } else {
        merged.push({ ...range });
      }
      return merged;
    }, []);
}

function BoldedSearchText({
  text,
  ranges,
}: {
  text: string;
  ranges: readonly SearchMatchRange[];
}) {
  const characters = Array.from(text);
  const highlights = searchMatchRanges(characters.length, ranges);
  if (!highlights.length) return text;

  const parts = [];
  let cursor = 0;
  for (const [index, range] of highlights.entries()) {
    if (range.start > cursor) {
      parts.push(
        <Fragment key={`text-${cursor}-${range.start}`}>
          {characters.slice(cursor, range.start).join("")}
        </Fragment>,
      );
    }
    parts.push(
      <strong
        className="sidebar-search-match"
        key={`match-${range.start}-${range.end}-${index}`}
      >
        {characters.slice(range.start, range.end).join("")}
      </strong>,
    );
    cursor = range.end;
  }
  if (cursor < characters.length) {
    parts.push(
      <Fragment key={`text-${cursor}-${characters.length}`}>
        {characters.slice(cursor).join("")}
      </Fragment>,
    );
  }
  return <>{parts}</>;
}

function SessionSearchHit({
  hit,
  onActivate,
}: {
  hit: TranscriptSearchHit;
  onActivate?: (sessionId: string, itemId: string) => void;
}) {
  return (
    <button
      className="session-search-hit"
      type="button"
      aria-label={`Open ${searchKindLabels[hit.kind]} result from ${displaySessionTitle(hit.sessionTitle)}`}
      onClick={() => onActivate?.(hit.sessionId, hit.itemId)}
    >
      <span>
        <BoldedSearchText text={hit.snippet} ranges={hit.matchRanges} />
      </span>
    </button>
  );
}

function PullRequestMark({
  state,
}: {
  state: NonNullable<SessionSummary["pullRequest"]>["state"];
}) {
  const Icon =
    state === "merged"
      ? GitMerge
      : state === "ready"
        ? GitPullRequest
        : GitPullRequestDraft;
  return (
    <span
      className={`session-pull-request is-${state.replace("_", "-")}`}
      title={pullRequestLabels[state]}
      aria-hidden="true"
    >
      <Icon />
    </span>
  );
}

function SessionRow({
  session,
  selected,
  onSelect,
  titleContent,
}: {
  session: SessionSummary;
  selected: boolean;
  onSelect: () => void;
  titleContent?: ReactNode;
}) {
  const displayTitle = displaySessionTitle(session.title);
  const pullRequestLabel = session.pullRequest
    ? `, ${pullRequestLabels[session.pullRequest.state]}`
    : "";
  return (
    <button
      className={`session-row ${selected ? "is-selected" : ""}`}
      onClick={onSelect}
      aria-current={selected ? "page" : undefined}
      aria-label={`Open task ${displayTitle}, ${sessionStatusLabel[session.status]}${pullRequestLabel}`}
      data-status={session.status}
    >
      <span className="session-row-title">
        {titleContent && displayTitle === session.title
          ? titleContent
          : displayTitle}
      </span>
      {session.status === "working" || session.status === "disconnected" ? (
        <span className="session-row-meta">
          <span
            className="session-loader"
            aria-label={sessionStatusLabel[session.status]}
          >
            <LoaderCircle className="spin" aria-hidden="true" />
          </span>
          {session.pullRequest ? (
            <PullRequestMark state={session.pullRequest.state} />
          ) : null}
        </span>
      ) : session.attentionCount > 0 || session.unread || session.pullRequest ? (
        <span className="session-row-meta">
          {session.attentionCount > 0 || session.unread ? (
            <span
              className="session-unread"
              aria-label={
                session.attentionCount > 0
                  ? "Needs attention"
                  : "Unread activity"
              }
            />
          ) : null}
          {session.pullRequest ? (
            <PullRequestMark state={session.pullRequest.state} />
          ) : null}
        </span>
      ) : null}
    </button>
  );
}

function WorkspaceSection({
  project,
  sessions,
  selectedSessionId,
  onSelectSession,
  renderControls,
  searchHitsBySession,
  onActivateSearchResult,
}: {
  project?: ProjectSummary;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  renderControls?: (session: SessionSummary) => ReactNode;
  searchHitsBySession?: ReadonlyMap<string, TranscriptSearchHit[]>;
  onActivateSearchResult?: (sessionId: string, itemId: string) => void;
}) {
  if (sessions.length === 0) return null;
  const title = project?.name ?? "Unassigned project";
  return (
    <section className="workspace-section" aria-label={title}>
      <header className="workspace-section-heading">
        <span className="workspace-name">
          <Folder aria-hidden="true" />
          <strong>{title}</strong>
        </span>
      </header>
      <div className="session-list">
        {sessions.map((session) => {
          const controls = renderControls?.(session);
          const searchHits = searchHitsBySession?.get(session.id);
          const titleMatchRanges =
            searchHits?.flatMap((hit) => hit.titleMatchRanges) ?? [];
          return (
            <div
              className={`session-row-shell ${controls ? "has-actions" : ""}`}
              key={session.id}
            >
              <SessionRow
                session={session}
                selected={session.id === selectedSessionId}
                onSelect={() => onSelectSession(session.id)}
                titleContent={
                  searchHits ? (
                    <BoldedSearchText
                      text={session.title}
                      ranges={titleMatchRanges}
                    />
                  ) : undefined
                }
              />
              {searchHits?.length ? (
                <div className="session-search-hits">
                  {searchHits.map((hit) => (
                    <SessionSearchHit
                      key={`${hit.sessionId}:${hit.itemId}`}
                      hit={hit}
                      onActivate={onActivateSearchResult}
                    />
                  ))}
                </div>
              ) : null}
              {controls}
            </div>
          );
        })}
      </div>
    </section>
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message
    ? error.message
    : "The host could not complete this task change.";
}

function purgeLabel(purgeAfterMs: number): string {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(purgeAfterMs));
}

function SidebarView({
  open,
  blocked,
  sessions,
  projects = [],
  selectedSessionId,
  surface,
  devicesAvailable,
  filesAvailable = false,
  onRestoreFocus,
  onClose,
  onNewSession,
  onOpenFleet,
  onSelectSession,
  onRestoreSession,
  sessionTrashAvailable = false,
  onSetSessionLifecycle,
  onDeleteSessionPermanently,
  onOpenDevices,
  onOpenProjects,
  onOpenFiles,
  onOpenUsage,
  onOpenSettings,
  transcriptSearchAvailable,
  onSearchTranscripts,
  onActivateSearchResult,
}: SidebarProps) {
  const sidebarRef = useRef<HTMLElement>(null);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    if (
      !open ||
      blocked ||
      !window.matchMedia("(max-width: 760px)").matches
    ) {
      return;
    }
    const sidebar = sidebarRef.current;
    const originalTarget =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    const focusable = () =>
      Array.from(
        sidebar?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      ).filter((element) => {
        const style = window.getComputedStyle(element);
        return style.display !== "none" && style.visibility !== "hidden";
      });
    window.requestAnimationFrame(() => focusable()[0]?.focus());
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (event.key !== "Tab") return;
      const targets = focusable();
      if (!targets.length) return;
      const first = targets[0];
      const last = targets.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      onRestoreFocus();
      if (
        document.activeElement === document.body &&
        originalTarget &&
        !originalTarget.closest("[inert]")
      ) {
        originalTarget.focus();
      }
    };
  }, [blocked, onRestoreFocus, open]);
  const [query, setQuery] = useState("");
  const [selectedView, setSelectedView] =
    useState<SessionSummary["lifecycle"]>("active");
  const [pendingSessionId, setPendingSessionId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<{
    sessionId: string;
    message: string;
  } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<{
    sessionId: string;
    trashedAtMs: number;
  } | null>(null);
  const [deletePhrase, setDeletePhrase] = useState("");
  const searchSequenceRef = useRef(0);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [searchResult, setSearchResult] =
    useState<TranscriptSearchResult | null>(null);
  const view =
    selectedView === "trash" && !sessionTrashAvailable
      ? "active"
      : selectedView;
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const searchQuery = query.trim();
  const contentSearchEnabled = Boolean(
    onSearchTranscripts && transcriptSearchAvailable !== false,
  );

  useEffect(() => {
    if (!contentSearchEnabled || !searchQuery || !onSearchTranscripts) {
      searchSequenceRef.current += 1;
      return;
    }

    const sequence = ++searchSequenceRef.current;
    const timer = window.setTimeout(() => {
      void onSearchTranscripts({
        query: searchQuery,
        filter: {},
        limit: 100,
      })
        .then((result) => {
          if (sequence !== searchSequenceRef.current) return;
          setSearchResult(result);
        })
        .catch(() => {
          if (sequence !== searchSequenceRef.current) return;
          setSearchError("Task search could not be completed. Try again.");
        })
        .finally(() => {
          if (sequence === searchSequenceRef.current) setSearchLoading(false);
        });
    }, 140);

    return () => {
      window.clearTimeout(timer);
      if (sequence === searchSequenceRef.current) {
        searchSequenceRef.current += 1;
      }
    };
  }, [contentSearchEnabled, onSearchTranscripts, searchQuery]);

  const updateSearchQuery = (nextQuery: string) => {
    setQuery(nextQuery);
    setSearchError(null);
    setSearchResult(null);
    setSearchLoading(contentSearchEnabled && Boolean(nextQuery.trim()));
  };

  const matchesSessionMetadata = (session: SessionSummary) =>
    !normalizedQuery ||
    session.title.toLocaleLowerCase().includes(normalizedQuery) ||
    displaySessionTitle(session.title)
      .toLocaleLowerCase()
      .includes(normalizedQuery) ||
    session.preview.toLocaleLowerCase().includes(normalizedQuery);
  const visibleSessions = sessions.filter(
    (session) => session.lifecycle === view && matchesSessionMetadata(session),
  );
  const searchHitsBySession = new Map<string, TranscriptSearchHit[]>();
  if (searchResult) {
    for (const hit of searchResult.hits) {
      const hits = searchHitsBySession.get(hit.sessionId);
      if (hits) hits.push(hit);
      else searchHitsBySession.set(hit.sessionId, [hit]);
    }
  }
  const searchMode = contentSearchEnabled && Boolean(searchQuery);
  const searchVisibleSessions = searchMode
    ? sessions.filter(
        (session) =>
          session.lifecycle === view &&
          (searchHitsBySession.has(session.id) ||
            matchesSessionMetadata(session)),
      )
    : visibleSessions;
  const projectsById = new Map(projects.map((project) => [project.id, project]));
  const workspaceSessions = new Map<string, SessionSummary[]>();
  for (const session of searchVisibleSessions) {
    const grouped = workspaceSessions.get(session.projectId);
    if (grouped) grouped.push(session);
    else workspaceSessions.set(session.projectId, [session]);
  }
  const workspaces = [
    ...projects
      .filter((project) => workspaceSessions.has(project.id))
      .map((project) => ({
        project,
        sessions: workspaceSessions.get(project.id)!,
      })),
    ...Array.from(workspaceSessions.entries())
      .filter(([projectId]) => !projectsById.has(projectId))
      .map(([, workspace]) => ({ project: undefined, sessions: workspace })),
  ];
  for (const workspace of workspaces) {
    workspace.sessions.sort(
      (left, right) => Number(right.pinned) - Number(left.pinned),
    );
  }
  const activeCount = sessions.filter(
    (session) => session.lifecycle === "active",
  ).length;
  const attentionCount = sessions.filter(
    (session) =>
      session.lifecycle === "active" && taskNeedsAttention(session),
  ).length;
  const archiveCount = sessions.filter(
    (session) => session.lifecycle === "archived",
  ).length;
  const trashCount = sessions.filter(
    (session) => session.lifecycle === "trash",
  ).length;

  const selectView = (nextView: SessionSummary["lifecycle"]) => {
    setSelectedView(nextView);
    updateSearchQuery("");
    setActionError(null);
    setDeleteTarget(null);
    setDeletePhrase("");
  };

  const setLifecycle = async (
    session: SessionSummary,
    lifecycle: SessionSummary["lifecycle"],
  ) => {
    setPendingSessionId(session.id);
    setActionError(null);
    try {
      if (onSetSessionLifecycle) {
        await onSetSessionLifecycle(session.id, lifecycle);
      } else if (lifecycle === "active") {
        await onRestoreSession(session.id);
      } else {
        return;
      }
    } catch (error) {
      setActionError({ sessionId: session.id, message: errorMessage(error) });
    } finally {
      setPendingSessionId(null);
    }
  };

  const beginPermanentDelete = (session: SessionSummary) => {
    if (!session.retention) return;
    setActionError(null);
    setDeletePhrase("");
    setDeleteTarget({
      sessionId: session.id,
      trashedAtMs: session.retention.trashedAtMs,
    });
  };

  const deletePermanently = async (session: SessionSummary) => {
    const retention = session.retention;
    const expectedPhrase = `permanently delete ${session.id}`;
    if (
      !retention ||
      !onDeleteSessionPermanently ||
      deleteTarget?.sessionId !== session.id ||
      deleteTarget.trashedAtMs !== retention.trashedAtMs ||
      deletePhrase !== expectedPhrase
    ) {
      return;
    }
    setPendingSessionId(session.id);
    setActionError(null);
    try {
      await onDeleteSessionPermanently(
        session.id,
        deleteTarget.trashedAtMs,
        deletePhrase,
      );
      setDeleteTarget(null);
      setDeletePhrase("");
    } catch (error) {
      setActionError({ sessionId: session.id, message: errorMessage(error) });
    } finally {
      setPendingSessionId(null);
    }
  };

  const renderSessionControls = (session: SessionSummary) => {
    if (view === "active" && !onSetSessionLifecycle) return null;
    const displayTitle = displaySessionTitle(session.title);
    const pending = pendingSessionId === session.id;
    const retention = session.retention;
    const expectedPhrase = `permanently delete ${session.id}`;
    const confirmingDelete =
      view === "trash" &&
      retention !== undefined &&
      deleteTarget?.sessionId === session.id &&
      deleteTarget.trashedAtMs === retention.trashedAtMs;

    return (
      <>
        <div className="session-row-actions">
          {view === "active" ? (
            <button
              type="button"
              className="session-row-archive"
              onClick={() => void setLifecycle(session, "archived")}
              aria-label={`Archive task ${displayTitle}`}
              title="Archive task"
              disabled={pending}
            >
              <Archive aria-hidden="true" />
            </button>
          ) : (
            <button
              type="button"
              className="session-row-restore"
              onClick={() => void setLifecycle(session, "active")}
              aria-label={`Restore task ${displayTitle}`}
              title="Restore task"
              disabled={pending}
            >
              <ArchiveRestore aria-hidden="true" />
            </button>
          )}
          {view === "archived" &&
          sessionTrashAvailable &&
          onSetSessionLifecycle ? (
            <button
              type="button"
              className="session-row-trash"
              onClick={() => void setLifecycle(session, "trash")}
              aria-label={`Move task ${displayTitle} to trash`}
              title="Move to trash"
              disabled={pending}
            >
              <Trash2 aria-hidden="true" />
            </button>
          ) : null}
          {view === "trash" &&
          retention &&
          onDeleteSessionPermanently ? (
            <button
              type="button"
              className="session-row-trash"
              onClick={() => beginPermanentDelete(session)}
              aria-label={`Permanently delete task ${displayTitle}`}
              title="Permanently delete"
              aria-expanded={confirmingDelete}
              disabled={pending}
            >
              <Trash2 aria-hidden="true" />
            </button>
          ) : null}
        </div>
        {retention ? (
          <p className="session-retention">
            Automatic purge{" "}
            <time dateTime={new Date(retention.purgeAfterMs).toISOString()}>
              {purgeLabel(retention.purgeAfterMs)}
            </time>
          </p>
        ) : null}
        {confirmingDelete ? (
          <div
            className="session-delete-confirmation"
            role="group"
            aria-label={`Permanent deletion confirmation for ${displayTitle}`}
          >
            <p>
              This cannot be undone. Type{" "}
              <code>{expectedPhrase}</code> to delete now.
            </p>
            <label>
              <span className="sr-only">
                Confirmation phrase for {displayTitle}
              </span>
              <input
                type="text"
                value={deletePhrase}
                onChange={(event) => setDeletePhrase(event.target.value)}
                aria-label={`Confirmation phrase for ${displayTitle}`}
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            <div className="session-delete-actions">
              <button
                type="button"
                className="secondary-button"
                onClick={() => {
                  setDeleteTarget(null);
                  setDeletePhrase("");
                }}
                disabled={pending}
              >
                Cancel
              </button>
              <button
                type="button"
                className="session-delete-danger"
                onClick={() => void deletePermanently(session)}
                disabled={pending || deletePhrase !== expectedPhrase}
              >
                Delete permanently
              </button>
            </div>
          </div>
        ) : null}
        {actionError?.sessionId === session.id ? (
          <p className="session-action-error" role="alert">
            {actionError.message}
          </p>
        ) : null}
      </>
    );
  };

  return (
    <>
      <button
        className={`sidebar-backdrop ${open ? "is-visible" : ""}`}
        aria-label="Close sidebar"
        onClick={onClose}
        tabIndex={open ? 0 : -1}
      />
      <aside
        ref={sidebarRef}
        className={`sidebar ${open ? "is-open" : ""}`}
        aria-label="ygg"
        inert={!open || blocked}
      >
        <header className="sidebar-header">
          <div className="brand-row">
            <strong>ygg</strong>
          </div>
          <button className="icon-button sidebar-close" onClick={onClose}>
            <PanelLeftClose aria-hidden="true" />
            <span className="sr-only">Close sidebar</span>
          </button>
          <button className="mobile-sidebar-close" onClick={onClose}>
            <X aria-hidden="true" />
            <span>Close</span>
          </button>
        </header>

        <nav className="primary-navigation" aria-label="Primary">
          <button className="primary-action" onClick={onNewSession}>
            <span className="primary-action-glyph" aria-hidden="true">
              <MessageSquarePlus />
            </span>
            <span>New task</span>
          </button>
          <button
            className={`sidebar-command-center ${surface === "fleet" ? "is-selected" : ""}`}
            type="button"
            aria-current={surface === "fleet" ? "page" : undefined}
            onClick={onOpenFleet}
          >
            <LayoutDashboard aria-hidden="true" />
            <span>Command center</span>
            {attentionCount > 0 ? (
              <em
                aria-label={`${attentionCount} ${attentionCount === 1 ? "task needs" : "tasks need"} you`}
              >
                {attentionCount}
              </em>
            ) : null}
          </button>
          <label className="sidebar-search">
            <Search aria-hidden="true" />
            <span className="sr-only">Search tasks and transcripts</span>
            <input
              type="search"
              value={query}
              onChange={(event) => updateSearchQuery(event.target.value)}
              placeholder={
                view === "archived"
                  ? "Search archive"
                  : view === "trash"
                    ? "Search trash"
                    : "Search tasks"
              }
            />
          </label>
          <div
            className="sidebar-lifecycle-tabs"
            role="tablist"
            aria-label="Task groups"
          >
            <button
              type="button"
              role="tab"
              aria-selected={view === "active"}
              aria-controls="sidebar-session-list"
              className={view === "active" ? "is-selected" : ""}
              onClick={() => selectView("active")}
            >
              <MessageSquarePlus aria-hidden="true" />
              <span>Active</span>
              {activeCount > 0 ? <em>{activeCount}</em> : null}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={view === "archived"}
              aria-controls="sidebar-session-list"
              className={view === "archived" ? "is-selected" : ""}
              onClick={() => selectView("archived")}
            >
              <Archive aria-hidden="true" />
              <span>Archive</span>
              {archiveCount > 0 ? <em>{archiveCount}</em> : null}
            </button>
            {sessionTrashAvailable ? (
              <button
                type="button"
                role="tab"
                aria-selected={view === "trash"}
                aria-controls="sidebar-session-list"
                className={view === "trash" ? "is-selected" : ""}
                onClick={() => selectView("trash")}
              >
                <Trash2 aria-hidden="true" />
                <span>Trash</span>
                {trashCount > 0 ? <em>{trashCount}</em> : null}
              </button>
            ) : null}
          </div>
        </nav>

        <div
          className="sidebar-scroll"
          id="sidebar-session-list"
          role="tabpanel"
          aria-label={`${view === "archived" ? "Archived" : view === "trash" ? "Trash" : "Active"} tasks`}
        >
          {searchMode &&
          (searchLoading || (!searchResult && !searchError)) ? (
            <div className="sidebar-search-state" role="status">
              Searching tasks…
            </div>
          ) : searchMode && searchError ? (
            <div className="sidebar-search-state is-error" role="alert">
              {searchError}
            </div>
          ) : searchVisibleSessions.length === 0 ? (
            <div className="sidebar-empty">
              {normalizedQuery ? (
                <Search aria-hidden="true" />
              ) : (
                <Menu aria-hidden="true" />
              )}
              <strong>
                {normalizedQuery
                  ? "No tasks match your query"
                  : view === "archived"
                    ? "Archive is empty"
                    : view === "trash"
                      ? "Trash is empty"
                      : "Start a task to see it here"}
              </strong>
              {view === "active" && !normalizedQuery ? (
                <button
                  className="secondary-button sidebar-empty-action"
                  type="button"
                  onClick={onNewSession}
                >
                  <MessageSquarePlus aria-hidden="true" />
                  New task
                </button>
              ) : null}
            </div>
          ) : (
            <>
              <div className="workspace-list-heading">
                <span>
                  {searchMode
                    ? "Search results"
                    : view === "active"
                      ? "Tasks"
                      : "Task history"}
                </span>
                <button type="button" onClick={onOpenProjects}>
                  Manage
                </button>
              </div>
              {workspaces.map((workspace) => (
                <WorkspaceSection
                  key={workspace.project?.id ?? workspace.sessions[0]!.projectId}
                  project={workspace.project}
                  sessions={workspace.sessions}
                  selectedSessionId={selectedSessionId}
                  onSelectSession={onSelectSession}
                  searchHitsBySession={
                    searchMode ? searchHitsBySession : undefined
                  }
                  onActivateSearchResult={onActivateSearchResult}
                  renderControls={
                    !searchMode ? renderSessionControls : undefined
                  }
                />
              ))}
            </>
          )}
        </div>

        <footer className="sidebar-footer">
          {filesAvailable && onOpenFiles ? (
            <button
              className={`sidebar-destination ${surface === "files" ? "is-selected" : ""}`}
              aria-current={surface === "files" ? "page" : undefined}
              onClick={onOpenFiles}
            >
              <Files aria-hidden="true" />
              <strong>Files</strong>
            </button>
          ) : null}
          <button
            className={`sidebar-destination ${surface === "projects" ? "is-selected" : ""}`}
            onClick={onOpenProjects}
          >
            <Folder aria-hidden="true" />
            <strong>Projects</strong>
          </button>
          <button
            className={`sidebar-destination ${surface === "usage" ? "is-selected" : ""}`}
            aria-current={surface === "usage" ? "page" : undefined}
            onClick={onOpenUsage}
          >
            <BarChart3 aria-hidden="true" />
            <strong>Usage</strong>
          </button>
          {devicesAvailable ? (
            <button
              className={`sidebar-destination ${surface === "devices" ? "is-selected" : ""}`}
              aria-label="Connected devices"
              onClick={onOpenDevices}
            >
              <Laptop aria-hidden="true" />
              <strong>Devices</strong>
            </button>
          ) : null}
          <button
            className={`sidebar-destination ${surface === "settings" ? "is-selected" : ""}`}
            onClick={onOpenSettings}
          >
            <Settings aria-hidden="true" />
            <strong>Settings</strong>
          </button>
        </footer>
      </aside>
    </>
  );
}

export const Sidebar = memo(SidebarView);
