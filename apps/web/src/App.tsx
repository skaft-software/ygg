import {
  Archive,
  ChevronDown,
  Download,
  Folder,
  GitBranch,
  Menu,
  MoreHorizontal,
  PanelRight,
  Pencil,
  Pin,
  PinOff,
  RefreshCw,
  X,
} from "lucide-react";
import {
  type CSSProperties,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ActivityRail } from "./components/ActivityRail";
import { Conversation } from "./components/Conversation";
import { DevicesView } from "./components/Devices";
import {
  Inspector,
  type InspectorSelection,
} from "./components/Inspector";
import { SettingsView } from "./components/Settings";
import { Sidebar } from "./components/Sidebar";
import { YggGlyph } from "./components/YggGlyph";
import type { SessionSnapshot, SessionStatus } from "./protocol";
import {
  sessionIdFromPathname,
  YggStore,
  useYggStore,
} from "./store";
import { applyStoredTypePreferences, applyTheme } from "./theme";
import {
  createTransport,
  type TransportConnectionState,
  transportModeFromSearch,
} from "./transport";

type Surface = "session" | "settings" | "devices";

const statusLabel: Record<SessionStatus, string> = {
  idle: "Ready",
  working: "Working",
  needs_attention: "Needs attention",
  done: "Done",
  failed: "Failed",
  stopped: "Stopped",
  disconnected: "Reconnecting",
};

const transportMode = transportModeFromSearch(window.location.search);
const store = new YggStore(createTransport(transportMode));
const activityPaneStorageKey = "ygg.ui.activity-width";
const inspectorPaneStorageKey = "ygg.ui.inspector-width";

function storedPaneWidth(key: string, fallback: number): number {
  try {
    const value = Number(window.localStorage.getItem(key));
    return Number.isFinite(value) && value > 0 ? value : fallback;
  } catch {
    return fallback;
  }
}

function persistPaneWidth(key: string, value: number) {
  try {
    window.localStorage.setItem(key, String(Math.round(value)));
  } catch {
    // A hardened browser may disable storage; resizing still works in memory.
  }
}

function FixtureModeLabel() {
  if (!import.meta.env.DEV || transportMode !== "fixture") return null;
  return (
    <div className="fixture-mode-label" role="status">
      Demo data · responses and actions are simulated
    </div>
  );
}

function ConnectionBanner({
  connection,
}: {
  connection: TransportConnectionState;
}) {
  if (connection === "connected") return null;
  return (
    <div className="connection-banner" role="status">
      <RefreshCw className="spin" aria-hidden="true" />
      <span>
        {connection === "reconnecting"
          ? "Connection interrupted. Reconnecting to ygg…"
          : "Connecting to local ygg…"}
      </span>
      <small>Your current session remains visible while ygg reconnects.</small>
    </div>
  );
}

function LoadingState() {
  return (
    <div className="app-loading" aria-live="polite">
      <YggGlyph />
      <span className="loading-pulse" />
      <strong>Opening a new session</strong>
    </div>
  );
}

function ErrorState({
  message,
  onRetry,
}: {
  message: string;
  onRetry?: () => void;
}) {
  return (
    <div className="app-error" role="alert">
      <div className="error-mark">
        <X aria-hidden="true" />
      </div>
      <h1>ygg could not connect</h1>
      <p>{message}</p>
      {onRetry ? <small>Retrying automatically in the background.</small> : null}
      <button
        className="primary-button"
        onClick={onRetry ?? (() => window.location.reload())}
      >
        <RefreshCw aria-hidden="true" />
        Try now
      </button>
    </div>
  );
}

interface HeaderProps {
  sidebarOpen: boolean;
  sessionId: string;
  sessionTitle: string;
  projectName: string;
  status: SessionStatus;
  activityAvailable: boolean;
  activityOpen: boolean;
  pinned: boolean;
  sessionActionsAvailable: boolean;
  metadataActionsAvailable: boolean;
  branchHistoryAvailable: boolean;
  sessionExportAvailable: boolean;
  activityButtonRef: RefObject<HTMLButtonElement | null>;
  sidebarButtonRef: RefObject<HTMLButtonElement | null>;
  onOpenSidebar: () => void;
  onToggleActivity: () => void;
  onRename: (title: string) => void;
  onPin: (pinned: boolean) => void;
  onArchive: () => void;
  onOpenBranchHistory: () => void;
}

export function SessionHeader({
  sidebarOpen,
  sessionId,
  sessionTitle,
  projectName,
  status,
  activityAvailable,
  activityOpen,
  pinned,
  sessionActionsAvailable,
  metadataActionsAvailable,
  branchHistoryAvailable,
  sessionExportAvailable,
  activityButtonRef,
  sidebarButtonRef,
  onOpenSidebar,
  onToggleActivity,
  onRename,
  onPin,
  onArchive,
  onOpenBranchHistory,
}: HeaderProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [draftTitle, setDraftTitle] = useState(sessionTitle);
  const menuTriggerRef = useRef<HTMLButtonElement>(null);
  const renameFinishedRef = useRef(false);

  useEffect(() => {
    if (!menuOpen) return;
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setMenuOpen(false);
      window.requestAnimationFrame(() => menuTriggerRef.current?.focus());
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [menuOpen]);

  const finishRename = (commit: boolean, restoreFocus: boolean) => {
    if (renameFinishedRef.current) return;
    renameFinishedRef.current = true;
    if (commit && draftTitle.trim()) onRename(draftTitle);
    setRenaming(false);
    setMenuOpen(false);
    if (restoreFocus) {
      window.requestAnimationFrame(() => menuTriggerRef.current?.focus());
    }
  };

  return (
    <header className="session-header">
      <div className="session-header-leading">
        {!sidebarOpen ? (
          <button
            ref={sidebarButtonRef}
            className="icon-button open-sidebar"
            onClick={onOpenSidebar}
          >
            <Menu aria-hidden="true" />
            <span className="sr-only">Open sidebar</span>
          </button>
        ) : null}
        <div className="session-breadcrumb">
          <span>
            <Folder aria-hidden="true" />
            {projectName}
          </span>
          <ChevronDown aria-hidden="true" />
          {renaming ? (
            <input
              autoFocus
              value={draftTitle}
              onChange={(event) => setDraftTitle(event.target.value)}
              onBlur={() => finishRename(true, false)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  finishRename(true, true);
                }
                if (event.key === "Escape") {
                  event.preventDefault();
                  finishRename(false, true);
                }
              }}
              aria-label="Session title"
            />
          ) : (
            <strong>{sessionTitle}</strong>
          )}
        </div>
      </div>

      <div className="session-header-actions">
        <span className={`header-status is-${status}`}>
          {status === "working" ? (
            <span className="status-orbit" aria-hidden="true" />
          ) : null}
          {statusLabel[status]}
        </span>
        {activityAvailable ? (
          <button
            ref={activityButtonRef}
            className={`icon-button ${activityOpen ? "is-active" : ""}`}
            onClick={onToggleActivity}
            aria-label={activityOpen ? "Close activity" : "Open activity"}
          >
            <PanelRight aria-hidden="true" />
          </button>
        ) : null}
        {sessionActionsAvailable ? (
          <div className="menu-anchor">
            <button
              ref={menuTriggerRef}
              className="icon-button"
              onClick={() => setMenuOpen((open) => !open)}
              aria-expanded={menuOpen}
              aria-label="Session actions"
            >
              <MoreHorizontal aria-hidden="true" />
            </button>
            {menuOpen ? (
              <>
                <button
                  className="menu-dismiss"
                  onClick={() => setMenuOpen(false)}
                  aria-label="Close menu"
                  tabIndex={-1}
                />
                <div className="session-menu" role="menu">
                  {branchHistoryAvailable ? (
                    <button
                      role="menuitem"
                      onClick={() => {
                        setMenuOpen(false);
                        onOpenBranchHistory();
                      }}
                    >
                      <GitBranch aria-hidden="true" />
                      Session history
                    </button>
                  ) : null}
                  {sessionExportAvailable ? (
                    <a
                      role="menuitem"
                      href={`/api/v1/sessions/${encodeURIComponent(sessionId)}/export`}
                      download
                      onClick={() => setMenuOpen(false)}
                    >
                      <Download aria-hidden="true" />
                      Download safe export
                    </a>
                  ) : null}
                  {metadataActionsAvailable ? (
                    <>
                      <button
                        role="menuitem"
                        onClick={() => {
                          setDraftTitle(sessionTitle);
                          renameFinishedRef.current = false;
                          setRenaming(true);
                          setMenuOpen(false);
                        }}
                      >
                        <Pencil aria-hidden="true" />
                        Rename
                      </button>
                      <button role="menuitem" onClick={() => onPin(!pinned)}>
                        {pinned ? (
                          <PinOff aria-hidden="true" />
                        ) : (
                          <Pin aria-hidden="true" />
                        )}
                        {pinned ? "Unpin" : "Pin"}
                      </button>
                      <button
                        className="danger-row"
                        role="menuitem"
                        onClick={onArchive}
                      >
                        <Archive aria-hidden="true" />
                        Archive
                      </button>
                    </>
                  ) : null}
                </div>
              </>
            ) : null}
          </div>
        ) : null}
      </div>
    </header>
  );
}

function BranchHistorySheet({
  session,
  onClose,
  onCheckout,
}: {
  session: SessionSnapshot;
  onClose: () => void;
  onCheckout: (entryId: string) => Promise<void>;
}) {
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const byId = useMemo(
    () => new Map(session.branches.entries.map((entry) => [entry.entryId, entry])),
    [session.branches.entries],
  );
  const activeAncestry = useMemo(() => {
    const active = new Set<string>();
    let cursor = session.branches.head;
    while (cursor && !active.has(cursor)) {
      active.add(cursor);
      cursor = byId.get(cursor)?.parentEntryId;
    }
    return active;
  }, [byId, session.branches.head]);
  const currentCheckpoint = useMemo(() => {
    let cursor = session.branches.head;
    while (cursor) {
      const entry = byId.get(cursor);
      if (!entry) return undefined;
      if (entry.checkoutable) return entry.entryId;
      cursor = entry.parentEntryId;
    }
    return undefined;
  }, [byId, session.branches.head]);
  const visibleEntries = session.branches.entries.filter(
    (entry) => entry.checkoutable,
  );
  const checkoutDisabled =
    session.activeRunId !== undefined ||
    !["idle", "done", "failed", "stopped"].includes(session.status);

  useEffect(() => {
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    <div className="branch-sheet-backdrop" role="presentation">
      <section
        className="branch-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="branch-sheet-title"
      >
        <header>
          <div>
            <span className="branch-sheet-glyph" aria-hidden="true">
              <GitBranch />
            </span>
            <div>
              <h2 id="branch-sheet-title">Session history</h2>
              <p>Choose where the conversation should continue.</p>
            </div>
          </div>
          <button className="icon-button" onClick={onClose} aria-label="Close history">
            <X aria-hidden="true" />
          </button>
        </header>
        <div className="branch-history-list">
          {visibleEntries.length ? (
            [...visibleEntries].reverse().map((entry) => {
              const current = entry.entryId === currentCheckpoint;
              const active = activeAncestry.has(entry.entryId);
              return (
                <article
                  className={`branch-history-row ${active ? "is-active" : ""}`}
                  key={entry.entryId}
                >
                  <span className="branch-history-node" aria-hidden="true" />
                  <div>
                    <strong>{entry.label}</strong>
                    <small>
                      {entry.kind === "userMessage"
                        ? "Your message"
                        : entry.kind === "compaction"
                          ? "Context checkpoint"
                          : "ygg response"}
                      {current ? " · Current" : ""}
                    </small>
                  </div>
                  <button
                    disabled={current || checkoutDisabled || pending !== null}
                    onClick={() => {
                      setPending(entry.entryId);
                      setError(null);
                      void onCheckout(entry.entryId)
                        .then(onClose)
                        .catch((reason: unknown) => {
                          setPending(null);
                          setError(
                            reason instanceof Error
                              ? reason.message
                              : "ygg could not switch checkpoints.",
                          );
                        });
                    }}
                  >
                    {pending === entry.entryId
                      ? "Switching…"
                      : current
                        ? "Current"
                        : "Switch here"}
                  </button>
                </article>
              );
            })
          ) : (
            <p className="branch-history-empty">History appears after the first message.</p>
          )}
        </div>
        <footer>
          <p>
            This changes the conversation state. Files, commands, and other side
            effects are not rolled back.
            {session.branches.truncated
              ? " Older checkpoints are not shown in this recent-history view."
              : ""}
          </p>
          {error ? <span role="alert">{error}</span> : null}
        </footer>
      </section>
    </div>
  );
}

function UtilityTopbar({
  title,
  sidebarOpen,
  onOpenSidebar,
  sidebarButtonRef,
}: {
  title: string;
  sidebarOpen: boolean;
  onOpenSidebar: () => void;
  sidebarButtonRef: RefObject<HTMLButtonElement | null>;
}) {
  return (
    <header className="utility-topbar">
      {!sidebarOpen ? (
        <button
          ref={sidebarButtonRef}
          className="icon-button"
          onClick={onOpenSidebar}
        >
          <Menu aria-hidden="true" />
          <span className="sr-only">Open sidebar</span>
        </button>
      ) : null}
      <strong>{title}</strong>
    </header>
  );
}

export default function App() {
  const state = useYggStore(store);
  const [sidebarOpen, setSidebarOpen] = useState(
    () => !window.matchMedia("(max-width: 760px)").matches,
  );
  const [mobileLayout, setMobileLayout] = useState(
    () => window.matchMedia("(max-width: 760px)").matches,
  );
  const [wideLayout, setWideLayout] = useState(
    () => window.matchMedia("(min-width: 1280px)").matches,
  );
  const [activityOpen, setActivityOpen] = useState(false);
  const [branchHistoryOpen, setBranchHistoryOpen] = useState(false);
  const [inspector, setInspector] = useState<InspectorSelection | null>(null);
  const [inspectorClosing, setInspectorClosing] = useState(false);
  const [activityPaneWidth, setActivityPaneWidth] = useState(() =>
    storedPaneWidth(activityPaneStorageKey, 320),
  );
  const [inspectorPaneWidth, setInspectorPaneWidth] = useState(() =>
    storedPaneWidth(inspectorPaneStorageKey, 720),
  );
  const [surface, setSurface] = useState<Surface>("session");
  const activityButtonRef = useRef<HTMLButtonElement>(null);
  const sidebarButtonRef = useRef<HTMLButtonElement>(null);
  const inspectorCloseTimerRef = useRef<number | null>(null);
  const paneResizeCleanupRef = useRef<(() => void) | null>(null);
  const restoreActivityFocus = useCallback(() => {
    const restore = () => activityButtonRef.current?.focus();
    restore();
    window.requestAnimationFrame(restore);
  }, []);
  const restoreSidebarFocus = useCallback(() => {
    const restore = () => sidebarButtonRef.current?.focus();
    restore();
    window.requestAnimationFrame(restore);
  }, []);
  const closeActivity = useCallback(() => {
    setActivityOpen(false);
    restoreActivityFocus();
  }, [restoreActivityFocus]);
  const closeInspector = useCallback(() => {
    if (inspectorCloseTimerRef.current !== null) return;
    setInspectorClosing(true);
    inspectorCloseTimerRef.current = window.setTimeout(() => {
      inspectorCloseTimerRef.current = null;
      setInspector(null);
      setInspectorClosing(false);
      restoreActivityFocus();
    }, 180);
  }, [restoreActivityFocus]);
  const closeSidebar = useCallback(() => {
    setSidebarOpen(false);
    restoreSidebarFocus();
  }, [restoreSidebarFocus]);

  useEffect(() => {
    applyStoredTypePreferences();
    void store.initialize();
    return () => {
      if (inspectorCloseTimerRef.current !== null) {
        window.clearTimeout(inspectorCloseTimerRef.current);
      }
      paneResizeCleanupRef.current?.();
      store.dispose();
    };
  }, []);

  useEffect(() => {
    if (!state.error || state.bootstrap) return;
    let cancelled = false;
    let delay = 1_000;
    let timer = 0;
    const retry = () => {
      timer = window.setTimeout(() => {
        if (cancelled) return;
        void store.initialize().finally(() => {
          if (cancelled || store.getSnapshot().ready) return;
          delay = Math.min(delay * 2, 8_000);
          retry();
        });
      }, delay);
    };
    retry();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [state.bootstrap, state.error]);

  useEffect(() => {
    const onPopState = () => {
      const sessionId = sessionIdFromPathname(window.location.pathname);
      if (sessionId) void store.selectSession(sessionId, "none");
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, []);

  const session = state.selectedSessionId
    ? state.sessions[state.selectedSessionId]
    : null;
  const selectedSummary = state.bootstrap?.sessions.find(
    (summary) => summary.id === state.selectedSessionId,
  );
  const project = state.bootstrap?.projects.find(
    (candidate) => candidate.id === session?.projectId,
  );
  const selectedTheme = state.bootstrap?.themes.find(
    (option) => option.id === state.bootstrap?.selectedThemeId,
  );

  const activityAvailable = Boolean(
    session &&
      (session.progress.length ||
        (state.bootstrap?.capabilities.resources &&
          (session.outputs.length || session.sources.length))),
  );
  const visibleActivityOpen = activityOpen && activityAvailable;
  const modalWorkspaceOpen =
    branchHistoryOpen ||
    (!wideLayout && (visibleActivityOpen || Boolean(inspector)));

  useEffect(() => {
    if (selectedTheme) applyTheme(selectedTheme.theme, selectedTheme.id);
  }, [selectedTheme]);

  useEffect(() => {
    const media = window.matchMedia("(max-width: 760px)");
    const onChange = (event: MediaQueryListEvent) => {
      setMobileLayout(event.matches);
    };
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);

  useEffect(() => {
    const media = window.matchMedia("(min-width: 1280px)");
    const onChange = (event: MediaQueryListEvent) => {
      setWideLayout(event.matches);
    };
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);

  const openOutput = (outputId: string) => {
    if (inspectorCloseTimerRef.current !== null) {
      window.clearTimeout(inspectorCloseTimerRef.current);
      inspectorCloseTimerRef.current = null;
    }
    setInspectorClosing(false);
    setActivityOpen(false);
    setInspector({ type: "output", id: outputId });
  };
  const openSource = (sourceId: string) => {
    if (inspectorCloseTimerRef.current !== null) {
      window.clearTimeout(inspectorCloseTimerRef.current);
      inspectorCloseTimerRef.current = null;
    }
    setInspectorClosing(false);
    setActivityOpen(false);
    setInspector({ type: "source", id: sourceId });
  };
  const openResource = (
    handle: string,
    title: string,
    presentation: "text" | "diff" | "image",
  ) => {
    if (inspectorCloseTimerRef.current !== null) {
      window.clearTimeout(inspectorCloseTimerRef.current);
      inspectorCloseTimerRef.current = null;
    }
    setInspectorClosing(false);
    setActivityOpen(false);
    setInspector({ type: "resource", handle, title, presentation });
  };

  const paneBounds = useCallback(
    (kind: "activity" | "inspector") => {
      const sidebarWidth = sidebarOpen ? 264 : 0;
      if (kind === "activity") {
        return {
          min: 280,
          max: Math.max(
            280,
            Math.min(440, window.innerWidth - sidebarWidth - 520),
          ),
        };
      }
      return {
        min: 520,
        max: Math.max(520, window.innerWidth - sidebarWidth - 380),
      };
    },
    [sidebarOpen],
  );

  const resizePaneBy = useCallback(
    (kind: "activity" | "inspector", delta: number) => {
      const bounds = paneBounds(kind);
      const current =
        kind === "activity" ? activityPaneWidth : inspectorPaneWidth;
      const next = Math.max(bounds.min, Math.min(bounds.max, current + delta));
      if (kind === "activity") {
        setActivityPaneWidth(next);
        persistPaneWidth(activityPaneStorageKey, next);
      } else {
        setInspectorPaneWidth(next);
        persistPaneWidth(inspectorPaneStorageKey, next);
      }
    },
    [activityPaneWidth, inspectorPaneWidth, paneBounds],
  );

  const beginPaneResize = useCallback(
    (kind: "activity" | "inspector", startX: number) => {
      paneResizeCleanupRef.current?.();
      const bounds = paneBounds(kind);
      const startWidth =
        kind === "activity" ? activityPaneWidth : inspectorPaneWidth;
      let nextWidth = startWidth;
      const onMove = (event: PointerEvent) => {
        nextWidth = Math.max(
          bounds.min,
          Math.min(bounds.max, startWidth + startX - event.clientX),
        );
        if (kind === "activity") setActivityPaneWidth(nextWidth);
        else setInspectorPaneWidth(nextWidth);
      };
      const cleanup = () => {
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", finish);
        window.removeEventListener("pointercancel", finish);
        window.removeEventListener("blur", finish);
        document.documentElement.classList.remove("is-resizing-pane");
        paneResizeCleanupRef.current = null;
      };
      const finish = () => {
        cleanup();
        persistPaneWidth(
          kind === "activity"
            ? activityPaneStorageKey
            : inspectorPaneStorageKey,
          nextWidth,
        );
      };
      paneResizeCleanupRef.current = cleanup;
      document.documentElement.classList.add("is-resizing-pane");
      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", finish);
      window.addEventListener("pointercancel", finish);
      window.addEventListener("blur", finish);
    },
    [activityPaneWidth, inspectorPaneWidth, paneBounds],
  );

  const appClass = useMemo(
    () =>
      [
        "app-shell",
        sidebarOpen ? "has-sidebar" : "",
        visibleActivityOpen && !inspector ? "has-activity" : "",
        inspector ? "has-inspector" : "",
        `surface-${surface}`,
      ]
        .filter(Boolean)
        .join(" "),
    [inspector, sidebarOpen, surface, visibleActivityOpen],
  );
  const appStyle = {
    "--activity-width": `${activityPaneWidth}px`,
    "--inspector-width": `${inspectorPaneWidth}px`,
  } as CSSProperties;

  if (state.connecting) return <LoadingState />;
  if (state.error) {
    return (
      <ErrorState
        message={state.error}
        onRetry={() => void store.initialize()}
      />
    );
  }
  if (!state.bootstrap || !session) return <ErrorState message="No session was selected." />;

  return (
    <div className={appClass} style={appStyle}>
      <Sidebar
        open={sidebarOpen}
        blocked={modalWorkspaceOpen}
        sessions={state.bootstrap.sessions}
        models={state.bootstrap.models}
        selectedSessionId={state.selectedSessionId}
        surface={surface}
        devicesAvailable={state.bootstrap.capabilities.connectedDevices}
        onRestoreFocus={restoreSidebarFocus}
        onClose={closeSidebar}
        onNewSession={() => {
          setSurface("session");
          setInspector(null);
          setActivityOpen(false);
          void store.createSession();
        }}
        onSelectSession={(sessionId) => {
          setSurface("session");
          setInspector(null);
          if (sessionId !== state.selectedSessionId) {
            setActivityOpen(false);
          }
          void store.selectSession(sessionId);
          if (window.matchMedia("(max-width: 760px)").matches) {
            setSidebarOpen(false);
          }
        }}
        onOpenSettings={() => {
          setSurface("settings");
          setInspector(null);
          if (window.matchMedia("(max-width: 760px)").matches) {
            setSidebarOpen(false);
          }
        }}
        onOpenDevices={() => {
          setSurface("devices");
          setInspector(null);
          if (window.matchMedia("(max-width: 760px)").matches) {
            setSidebarOpen(false);
          }
        }}
      />

      {surface === "session" ? (
        <div
          className="session-column"
          inert={
            (mobileLayout && sidebarOpen) || modalWorkspaceOpen
          }
        >
          <SessionHeader
            sidebarOpen={sidebarOpen}
            sessionId={session.sessionId}
            sessionTitle={session.title}
            projectName={project?.name ?? "Local project"}
            status={session.status}
            activityAvailable={activityAvailable}
            activityOpen={visibleActivityOpen}
            pinned={selectedSummary?.pinned ?? false}
            sessionActionsAvailable={
              state.bootstrap.capabilities.sessionMetadata ||
              state.bootstrap.capabilities.sessionBranches ||
              state.bootstrap.capabilities.sessionExport
            }
            metadataActionsAvailable={
              state.bootstrap.capabilities.sessionMetadata
            }
            branchHistoryAvailable={
              state.bootstrap.capabilities.sessionBranches &&
              session.branches.entries.some((entry) => entry.checkoutable)
            }
            sessionExportAvailable={
              state.bootstrap.capabilities.sessionExport
            }
            activityButtonRef={activityButtonRef}
            sidebarButtonRef={sidebarButtonRef}
            onOpenSidebar={() => setSidebarOpen(true)}
            onToggleActivity={() => {
              setInspector(null);
              setActivityOpen((open) => !open);
            }}
            onRename={(title) => void store.rename(title)}
            onPin={(pinned) => {
              void store.pin(pinned);
            }}
            onArchive={() => {
              void (async () => {
                if (await store.archive(true)) {
                  await store.createSession();
                }
              })();
            }}
            onOpenBranchHistory={() => setBranchHistoryOpen(true)}
          />
          <FixtureModeLabel />
          <ConnectionBanner connection={state.connection} />
          <Conversation
            key={session.sessionId}
            session={session}
            bootstrap={state.bootstrap}
            onSubmit={(prompt, attachments, activeDelivery) =>
              store.submit(prompt, attachments, activeDelivery)
            }
            onInterrupt={() => store.interrupt()}
            onConfigure={(patch) => store.configure(patch)}
            onResolveApproval={(requestId, decision) =>
              store.resolveApproval(requestId, decision)
            }
            onResolveUserInput={(requestId, answer) =>
              store.resolveUserInput(requestId, answer)
            }
            onOpenOutput={openOutput}
            onOpenSource={openSource}
            onOpenResource={
              state.bootstrap.capabilities.resources
                ? openResource
                : undefined
            }
            onIngestAttachment={(file) => store.ingestAttachment(file)}
            attachmentContentUrl={(handle) =>
              store.attachmentContentUrl(handle)
            }
          />
        </div>
      ) : (
        <div
          className="utility-column"
          inert={
            (mobileLayout && sidebarOpen) || modalWorkspaceOpen
          }
        >
          <UtilityTopbar
            title={surface === "settings" ? "Settings" : "Connected devices"}
            sidebarOpen={sidebarOpen}
            onOpenSidebar={() => setSidebarOpen(true)}
            sidebarButtonRef={sidebarButtonRef}
          />
          <ConnectionBanner connection={state.connection} />
          <FixtureModeLabel />
          {surface === "settings" ? (
            <SettingsView
              themes={state.bootstrap.themes}
              selectedThemeId={state.bootstrap.selectedThemeId}
              selectionAvailable={
                state.bootstrap.capabilities.themeSelection
              }
              onThemeChange={(themeId) => void store.selectTheme(themeId)}
            />
          ) : (
            <DevicesView
              hostName={state.bootstrap.host.name}
              devices={state.bootstrap.devices}
              lanAvailable={
                state.bootstrap.capabilities.connectedDevices &&
                state.bootstrap.capabilities.lanClients &&
                state.bootstrap.capabilities.pairDevices
              }
            />
          )}
        </div>
      )}

      {surface === "session" && session ? (
        <>
          {wideLayout && ((visibleActivityOpen && !inspector) || inspector) ? (
            <div
              className="pane-resize-handle"
              role="separator"
              aria-label={
                inspector ? "Resize inspector" : "Resize session activity"
              }
              aria-orientation="vertical"
              aria-valuemin={inspector ? 520 : 280}
              aria-valuemax={paneBounds(inspector ? "inspector" : "activity").max}
              aria-valuenow={
                inspector ? inspectorPaneWidth : activityPaneWidth
              }
              tabIndex={0}
              onPointerDown={(event) => {
                event.preventDefault();
                beginPaneResize(
                  inspector ? "inspector" : "activity",
                  event.clientX,
                );
              }}
              onKeyDown={(event) => {
                if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
                  return;
                }
                event.preventDefault();
                resizePaneBy(
                  inspector ? "inspector" : "activity",
                  event.key === "ArrowLeft" ? 16 : -16,
                );
              }}
            />
          ) : null}
          <ActivityRail
            session={session}
            open={
              visibleActivityOpen &&
              !inspector &&
              !(mobileLayout && sidebarOpen)
            }
            onClose={closeActivity}
            onOpenOutput={openOutput}
            onOpenSource={openSource}
            modal={!wideLayout}
            onRestoreFocus={restoreActivityFocus}
            resourcesAvailable={state.bootstrap.capabilities.resources}
          />
          <Inspector
            session={session}
            selection={inspector}
            closing={inspectorClosing}
            modal={!wideLayout}
            previewsAvailable={state.bootstrap.capabilities.previews}
            resourceContentUrl={(sessionId, handle) =>
              store.resourceContentUrl(sessionId, handle)
            }
            onRestoreFocus={restoreActivityFocus}
            onClose={closeInspector}
          />
        </>
      ) : null}
      {surface === "session" && session && branchHistoryOpen ? (
        <BranchHistorySheet
          session={session}
          onClose={() => setBranchHistoryOpen(false)}
          onCheckout={(entryId) => store.checkoutBranch(entryId)}
        />
      ) : null}
    </div>
  );
}
