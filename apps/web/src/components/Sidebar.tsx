import {
  Archive,
  ArchiveRestore,
  BarChart3,
  Folder,
  GitMerge,
  GitPullRequest,
  GitPullRequestDraft,
  Laptop,
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
  memo,
  useEffect,
  useRef,
  useState,
} from "react";
import type { ProjectSummary, SessionSummary } from "../protocol";

interface SidebarProps {
  open: boolean;
  blocked: boolean;
  sessions: SessionSummary[];
  projects?: ProjectSummary[];
  selectedSessionId: string | null;
  surface: "session" | "projects" | "usage" | "settings" | "devices";
  devicesAvailable: boolean;
  onRestoreFocus: () => void;
  onClose: () => void;
  onNewSession: () => void;
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
  onOpenUsage: () => void;
  onOpenSettings: () => void;
  transcriptSearchAvailable?: boolean;
  onOpenTranscriptSearch?: () => void;
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
}: {
  session: SessionSummary;
  selected: boolean;
  onSelect: () => void;
}) {
  const pullRequestLabel = session.pullRequest
    ? `, ${pullRequestLabels[session.pullRequest.state]}`
    : "";
  return (
    <button
      className={`session-row ${selected ? "is-selected" : ""}`}
      onClick={onSelect}
      aria-current={selected ? "page" : undefined}
      aria-label={`Open session ${session.title}, ${sessionStatusLabel[session.status]}${pullRequestLabel}`}
      data-status={session.status}
    >
      <span className="session-row-title">{session.title}</span>
      {session.pullRequest ? (
        <PullRequestMark state={session.pullRequest.state} />
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
}: {
  project?: ProjectSummary;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  renderControls?: (session: SessionSummary) => ReactNode;
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
          return (
            <div
              className={`session-row-shell ${controls ? "has-actions" : ""}`}
              key={session.id}
            >
              <SessionRow
                session={session}
                selected={session.id === selectedSessionId}
                onSelect={() => onSelectSession(session.id)}
              />
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
    : "The host could not complete this session change.";
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
  onRestoreFocus,
  onClose,
  onNewSession,
  onSelectSession,
  onRestoreSession,
  sessionTrashAvailable = false,
  onSetSessionLifecycle,
  onDeleteSessionPermanently,
  onOpenDevices,
  onOpenProjects,
  onOpenUsage,
  onOpenSettings,
  transcriptSearchAvailable = false,
  onOpenTranscriptSearch,
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
  const view =
    selectedView === "trash" && !sessionTrashAvailable
      ? "active"
      : selectedView;
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const visibleSessions = sessions.filter(
    (session) =>
      session.lifecycle === view &&
      (!normalizedQuery ||
        session.title.toLocaleLowerCase().includes(normalizedQuery) ||
        session.preview.toLocaleLowerCase().includes(normalizedQuery)),
  );
  const projectsById = new Map(projects.map((project) => [project.id, project]));
  const workspaceSessions = new Map<string, SessionSummary[]>();
  for (const session of visibleSessions) {
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
  const archiveCount = sessions.filter(
    (session) => session.lifecycle === "archived",
  ).length;
  const trashCount = sessions.filter(
    (session) => session.lifecycle === "trash",
  ).length;

  const selectView = (nextView: SessionSummary["lifecycle"]) => {
    setSelectedView(nextView);
    setQuery("");
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
    if (view === "active") return null;
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
          <button
            type="button"
            className="session-row-restore"
            onClick={() => void setLifecycle(session, "active")}
            aria-label={`Restore session ${session.title}`}
            title="Restore session"
            disabled={pending}
          >
            <ArchiveRestore aria-hidden="true" />
          </button>
          {view === "archived" &&
          sessionTrashAvailable &&
          onSetSessionLifecycle ? (
            <button
              type="button"
              className="session-row-trash"
              onClick={() => void setLifecycle(session, "trash")}
              aria-label={`Move session ${session.title} to trash`}
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
              aria-label={`Permanently delete session ${session.title}`}
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
            aria-label={`Permanent deletion confirmation for ${session.title}`}
          >
            <p>
              This cannot be undone. Type{" "}
              <code>{expectedPhrase}</code> to delete now.
            </p>
            <label>
              <span className="sr-only">
                Confirmation phrase for {session.title}
              </span>
              <input
                type="text"
                value={deletePhrase}
                onChange={(event) => setDeletePhrase(event.target.value)}
                aria-label={`Confirmation phrase for ${session.title}`}
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
            <span>New session</span>
          </button>
          <label className="sidebar-search">
            <Search aria-hidden="true" />
            <span className="sr-only">Search sessions</span>
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder={
                view === "archived"
                  ? "Search archive"
                  : view === "trash"
                    ? "Search trash"
                    : "Search sessions"
              }
            />
          </label>
          {transcriptSearchAvailable && onOpenTranscriptSearch ? (
            <button
              type="button"
              className="sidebar-transcript-search"
              onClick={onOpenTranscriptSearch}
            >
              <Search aria-hidden="true" />
              <span>Search conversation contents</span>
            </button>
          ) : null}
          <div
            className="sidebar-lifecycle-tabs"
            role="tablist"
            aria-label="Session groups"
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
          aria-label={`${view === "archived" ? "Archive" : view === "trash" ? "Trash" : "Active"} sessions`}
        >
          {visibleSessions.length === 0 ? (
            <div className="sidebar-empty">
              {normalizedQuery ? (
                <Search aria-hidden="true" />
              ) : (
                <Menu aria-hidden="true" />
              )}
              <span>
                {normalizedQuery
                  ? "No matching sessions"
                  : view === "archived"
                    ? "Archive is empty"
                    : view === "trash"
                      ? "Trash is empty"
                      : "No sessions yet"}
              </span>
            </div>
          ) : (
            <>
              <div className="workspace-list-heading">
                <span>{view === "active" ? "Sessions" : "Session history"}</span>
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
                  renderControls={
                    view === "active" ? undefined : renderSessionControls
                  }
                />
              ))}
            </>
          )}
        </div>

        <footer className="sidebar-footer">
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
              onClick={onOpenDevices}
            >
              <Laptop aria-hidden="true" />
              <strong>Connected devices</strong>
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
