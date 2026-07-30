import {
  Archive,
  ArchiveRestore,
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
  SquareTerminal,
  X,
} from "lucide-react";
import {
  type CSSProperties,
  type RefObject,
  lazy,
  memo,
  Suspense,
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
import { ProjectsView } from "./components/Projects";
import {
  disposeTerminalCache,
  TerminalPanel,
} from "./components/TerminalPanel";
import { UsagePage } from "./pages/UsagePage";
import { YggGlyph } from "./components/YggGlyph";
import type {
  AttachmentRef,
  AuthorityProfile,
  DocumentReference,
  ReasoningEffort,
  SessionSnapshot,
  SessionStatus,
  TrustedFileEntry,
  TranscriptSearchRequest,
  TranscriptSearchResult,
  UsagePeriod,
} from "./protocol";
import {
  AttentionNotificationManager,
  browserNotificationAdapter,
} from "./notifications";
import {
  sessionIdFromPathname,
  YggStore,
  useYggStore,
} from "./store";
import { applyStoredTypePreferences } from "./theme";
import {
  createTransport,
  type TransportConnectionState,
  transportModeFromSearch,
} from "./transport";

const FilesPanel = lazy(() =>
  import("./components/FilesPanel").then((module) => ({
    default: module.FilesPanel,
  })),
);

type Surface =
  | "session"
  | "projects"
  | "files"
  | "usage"
  | "settings"
  | "devices";

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
const terminalPaneStorageKey = "ygg.ui.terminal-width";
const terminalPaneOpenStorageKey = "ygg.ui.terminal.open";
const notificationPreferenceKey = (hostId: string) =>
  `ygg.notifications.enabled.${encodeURIComponent(hostId)}`;
const MemoizedInspector = memo(
  Inspector,
  (previous, next) =>
    previous.session.sessionId === next.session.sessionId &&
    previous.session.outputs === next.session.outputs &&
    previous.session.sources === next.session.sources &&
    previous.session.previews === next.session.previews &&
    previous.selection === next.selection &&
    previous.closing === next.closing &&
    previous.modal === next.modal &&
    previous.previewsAvailable === next.previewsAvailable &&
    previous.resourceContentUrl === next.resourceContentUrl &&
    previous.onRestoreFocus === next.onRestoreFocus &&
    previous.onClose === next.onClose,
);

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

function storedBoolean(key: string): boolean {
  try {
    return window.localStorage.getItem(key) === "true";
  } catch {
    return false;
  }
}

function persistBoolean(key: string, value: boolean): void {
  try {
    window.localStorage.setItem(key, String(value));
  } catch {
    // A hardened browser may disable storage; the panel still works in memory.
  }
}

function localStorageIfAvailable(): Storage | undefined {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

function storedNotificationPreference(hostId: string): boolean {
  try {
    return (
      window.localStorage.getItem(notificationPreferenceKey(hostId)) === "true"
    );
  } catch {
    return false;
  }
}

function persistNotificationPreference(hostId: string, enabled: boolean) {
  try {
    window.localStorage.setItem(
      notificationPreferenceKey(hostId),
      String(enabled),
    );
  } catch {
    // A storage-hardened browser keeps the preference for this page only.
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
    <div className="app-loading" role="status" aria-live="polite">
      <YggGlyph />
      <span className="loading-pulse" aria-hidden="true" />
      <strong>Connecting to ygg</strong>
      <p>Preparing your workspace.</p>
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
  terminalAvailable: boolean;
  terminalOpen: boolean;
  pinned: boolean;
  archived: boolean;
  sessionActionsAvailable: boolean;
  metadataActionsAvailable: boolean;
  branchHistoryAvailable: boolean;
  sessionExportAvailable: boolean;
  activityButtonRef: RefObject<HTMLButtonElement | null>;
  sidebarButtonRef: RefObject<HTMLButtonElement | null>;
  onOpenSidebar: () => void;
  onToggleActivity: () => void;
  onToggleTerminal: () => void;
  onRename: (title: string) => void;
  onPin: (pinned: boolean) => void;
  onArchive: (archived: boolean) => void;
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
  terminalAvailable,
  terminalOpen,
  pinned,
  archived,
  sessionActionsAvailable,
  metadataActionsAvailable,
  branchHistoryAvailable,
  sessionExportAvailable,
  activityButtonRef,
  sidebarButtonRef,
  onOpenSidebar,
  onToggleActivity,
  onToggleTerminal,
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
        {terminalAvailable ? (
          <button
            className={`icon-button ${terminalOpen ? "is-active" : ""}`}
            onClick={onToggleTerminal}
            aria-label={terminalOpen ? "Close terminal" : "Open terminal"}
            aria-pressed={terminalOpen}
          >
            <SquareTerminal aria-hidden="true" />
          </button>
        ) : null}
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
                        className={archived ? undefined : "danger-row"}
                        role="menuitem"
                        onClick={() => onArchive(!archived)}
                      >
                        {archived ? (
                          <ArchiveRestore aria-hidden="true" />
                        ) : (
                          <Archive aria-hidden="true" />
                        )}
                        {archived ? "Restore from archive" : "Archive"}
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
  const [terminalSplitLayout, setTerminalSplitLayout] = useState(
    () => window.matchMedia("(min-width: 900px)").matches,
  );
  const [activityOpen, setActivityOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(() =>
    storedBoolean(terminalPaneOpenStorageKey),
  );
  const [branchHistoryOpen, setBranchHistoryOpen] = useState(false);
  const [inspector, setInspector] = useState<InspectorSelection | null>(null);
  const [inspectorClosing, setInspectorClosing] = useState(false);
  const [activityPaneWidth, setActivityPaneWidth] = useState(() =>
    storedPaneWidth(activityPaneStorageKey, 400),
  );
  const [inspectorPaneWidth, setInspectorPaneWidth] = useState(() =>
    storedPaneWidth(inspectorPaneStorageKey, 720),
  );
  const [terminalPaneWidth, setTerminalPaneWidth] = useState(() =>
    storedPaneWidth(terminalPaneStorageKey, 460),
  );
  const [surface, setSurface] = useState<Surface>("session");
  const notificationManagerRef =
    useRef<AttentionNotificationManager | null>(null);
  const [notificationState, setNotificationState] = useState<{
    supported: boolean;
    enabled: boolean;
    permission: NotificationPermission | "unsupported";
  }>({
    supported: false,
    enabled: false,
    permission: "unsupported",
  });
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
    if (!wideLayout) {
      setInspector(null);
      setInspectorClosing(false);
      restoreActivityFocus();
      return;
    }
    setInspectorClosing(true);
    inspectorCloseTimerRef.current = window.setTimeout(() => {
      inspectorCloseTimerRef.current = null;
      setInspector(null);
      setInspectorClosing(false);
      restoreActivityFocus();
    }, 180);
  }, [restoreActivityFocus, wideLayout]);
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
      disposeTerminalCache();
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
    const hostId = state.bootstrap?.host.id;
    if (!hostId) return;
    const manager = new AttentionNotificationManager(
      hostId,
      browserNotificationAdapter(),
      localStorageIfAvailable(),
    );
    notificationManagerRef.current = manager;
    let cancelled = false;
    const preferred = storedNotificationPreference(hostId);
    const initialFrame = window.requestAnimationFrame(() => {
      if (cancelled) return;
      setNotificationState({
        supported: manager.supported,
        enabled: false,
        permission: manager.permission,
      });
    });
    if (preferred && manager.permission === "granted") {
      void manager.enable().then((enabled) => {
        if (cancelled) return;
        setNotificationState({
          supported: manager.supported,
          enabled,
          permission: manager.permission,
        });
      });
    }
    return () => {
      cancelled = true;
      window.cancelAnimationFrame(initialFrame);
      manager.disable();
      if (notificationManagerRef.current === manager) {
        notificationManagerRef.current = null;
      }
    };
  }, [state.bootstrap?.host.id]);

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
  const terminalAvailable = Boolean(state.bootstrap?.capabilities.terminal);

  const closeTerminal = useCallback(() => {
    setTerminalOpen(false);
    persistBoolean(terminalPaneOpenStorageKey, false);
  }, []);

  const visibleTerminalOpen = terminalAvailable && terminalOpen;

  const activityAvailable = Boolean(
    session &&
      (session.items.some((item) => item.kind === "run_outcome") ||
        session.progress.length ||
        (state.bootstrap?.capabilities.resources &&
          (session.outputs.length || session.sources.length))),
  );
  const visibleActivityOpen = activityOpen && activityAvailable;
  const modalWorkspaceOpen =
    branchHistoryOpen ||
    (!wideLayout && (visibleActivityOpen || Boolean(inspector))) ||
    (!terminalSplitLayout && visibleTerminalOpen);

  useEffect(() => {
    const media = window.matchMedia("(max-width: 760px)");
    const onChange = (event: MediaQueryListEvent) => {
      setMobileLayout(event.matches);
    };
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, []);

  useEffect(() => {
    const media = window.matchMedia("(min-width: 900px)");
    const onChange = (event: MediaQueryListEvent) => {
      setTerminalSplitLayout(event.matches);
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

  const openOutput = useCallback(
    (outputId: string) => {
      closeTerminal();
      if (inspectorCloseTimerRef.current !== null) {
        window.clearTimeout(inspectorCloseTimerRef.current);
        inspectorCloseTimerRef.current = null;
      }
      setInspectorClosing(false);
      setActivityOpen(false);
      setInspector({ type: "output", id: outputId });
    },
    [closeTerminal],
  );
  const openSource = useCallback(
    (sourceId: string) => {
      closeTerminal();
      if (inspectorCloseTimerRef.current !== null) {
        window.clearTimeout(inspectorCloseTimerRef.current);
        inspectorCloseTimerRef.current = null;
      }
      setInspectorClosing(false);
      setActivityOpen(false);
      setInspector({ type: "source", id: sourceId });
    },
    [closeTerminal],
  );
  const openResource = useCallback(
    (
      handle: string,
      title: string,
      presentation: "text" | "diff" | "image",
    ) => {
      closeTerminal();
      if (inspectorCloseTimerRef.current !== null) {
        window.clearTimeout(inspectorCloseTimerRef.current);
        inspectorCloseTimerRef.current = null;
      }
      setInspectorClosing(false);
      setActivityOpen(false);
      setInspector({ type: "resource", handle, title, presentation });
    },
    [closeTerminal],
  );

  const paneBounds = useCallback(
    (kind: "activity" | "inspector" | "terminal") => {
      const sidebarWidth = sidebarOpen ? 296 : 0;
      if (kind === "activity") {
        return {
          min: 280,
          max: Math.max(
            280,
            Math.min(440, window.innerWidth - sidebarWidth - 520),
          ),
        };
      }
      if (kind === "terminal") {
        return {
          min: 300,
          max: Math.max(
            300,
            Math.min(720, window.innerWidth - sidebarWidth - 360),
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
    (kind: "activity" | "inspector" | "terminal", delta: number) => {
      const bounds = paneBounds(kind);
      const current =
        kind === "activity"
          ? activityPaneWidth
          : kind === "inspector"
            ? inspectorPaneWidth
            : terminalPaneWidth;
      const next = Math.max(bounds.min, Math.min(bounds.max, current + delta));
      if (kind === "activity") {
        setActivityPaneWidth(next);
        persistPaneWidth(activityPaneStorageKey, next);
      } else if (kind === "inspector") {
        setInspectorPaneWidth(next);
        persistPaneWidth(inspectorPaneStorageKey, next);
      } else {
        setTerminalPaneWidth(next);
        persistPaneWidth(terminalPaneStorageKey, next);
      }
    },
    [activityPaneWidth, inspectorPaneWidth, paneBounds, terminalPaneWidth],
  );

  const beginPaneResize = useCallback(
    (kind: "activity" | "inspector" | "terminal", startX: number) => {
      paneResizeCleanupRef.current?.();
      const bounds = paneBounds(kind);
      const startWidth =
        kind === "activity"
          ? activityPaneWidth
          : kind === "inspector"
            ? inspectorPaneWidth
            : terminalPaneWidth;
      let nextWidth = startWidth;
      const onMove = (event: PointerEvent) => {
        nextWidth = Math.max(
          bounds.min,
          Math.min(bounds.max, startWidth + startX - event.clientX),
        );
        if (kind === "activity") setActivityPaneWidth(nextWidth);
        else if (kind === "inspector") setInspectorPaneWidth(nextWidth);
        else setTerminalPaneWidth(nextWidth);
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
            : kind === "inspector"
              ? inspectorPaneStorageKey
              : terminalPaneStorageKey,
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
    [activityPaneWidth, inspectorPaneWidth, paneBounds, terminalPaneWidth],
  );

  const appClass = useMemo(
    () =>
      [
        "app-shell",
        sidebarOpen ? "has-sidebar" : "",
        visibleActivityOpen && !inspector ? "has-activity" : "",
        inspector ? "has-inspector" : "",
        visibleTerminalOpen ? "has-terminal" : "",
        `surface-${surface}`,
      ]
        .filter(Boolean)
        .join(" "),
    [
      inspector,
      sidebarOpen,
      surface,
      visibleActivityOpen,
      visibleTerminalOpen,
    ],
  );
  const appStyle = {
    "--activity-width": `${activityPaneWidth}px`,
    "--inspector-width": `${inspectorPaneWidth}px`,
    "--terminal-width": `${terminalPaneWidth}px`,
  } as CSSProperties;
  const toggleTerminal = useCallback(() => {
    if (!terminalAvailable) return;
    if (terminalOpen) {
      closeTerminal();
      return;
    }
    setTerminalOpen(true);
    persistBoolean(terminalPaneOpenStorageKey, true);
    setActivityOpen(false);
    setInspector(null);
  }, [closeTerminal, terminalAvailable, terminalOpen]);
  const startNewSession = useCallback(() => {
    setSurface("session");
    setInspector(null);
    setActivityOpen(false);
    void store.createSession();
  }, []);
  const selectSession = useCallback(
    (sessionId: string) => {
      setSurface("session");
      setInspector(null);
      if (sessionId !== state.selectedSessionId) {
        setActivityOpen(false);
      }
      void store.selectSession(sessionId);
      if (window.matchMedia("(max-width: 760px)").matches) {
        setSidebarOpen(false);
      }
    },
    [state.selectedSessionId],
  );
  const searchTranscripts = useCallback(
    (request: TranscriptSearchRequest): Promise<TranscriptSearchResult> =>
      store.searchTranscripts(request),
    [],
  );
  const activateSearchResult = useCallback(
    (sessionId: string, itemId: string) => {
      void (async () => {
        setSurface("session");
        setInspector(null);
        setActivityOpen(false);
        if (window.matchMedia("(max-width: 760px)").matches) {
          setSidebarOpen(false);
        }
        await store.selectSession(sessionId);
        for (let attempt = 0; attempt < 12; attempt += 1) {
          await new Promise<void>((resolve) =>
            window.requestAnimationFrame(() => resolve()),
          );
          const target = document.getElementById(
            `transcript-item-${itemId}`,
          );
          if (!target) continue;
          for (
            let details = target.closest("details");
            details;
            details = details.parentElement?.closest("details") ?? null
          ) {
            details.open = true;
          }
          target.scrollIntoView({ block: "center", behavior: "smooth" });
          target.focus({ preventScroll: true });
          target.classList.add("is-search-target");
          window.setTimeout(
            () => target.classList.remove("is-search-target"),
            1_800,
          );
          break;
        }
      })();
    },
    [],
  );
  const restoreSession = useCallback(async (sessionId: string) => {
    setSurface("session");
    setInspector(null);
    setActivityOpen(false);
    await store.selectSession(sessionId);
    await store.archive(false);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const setSessionLifecycle = useCallback(
    async (
      sessionId: string,
      lifecycle: "active" | "archived" | "trash",
    ): Promise<void> => {
      if (lifecycle === "active") {
        await restoreSession(sessionId);
        return;
      }
      await store.setSessionLifecycle(sessionId, lifecycle);
      if (lifecycle === "archived" && sessionId === state.selectedSessionId) {
        await store.createSession();
      }
    },
    [restoreSession, state.selectedSessionId],
  );
  const changeNotifications = useCallback(
    async (enabled: boolean): Promise<boolean> => {
      const manager = notificationManagerRef.current;
      const hostId = state.bootstrap?.host.id;
      if (!manager || !hostId) return false;
      if (!enabled) {
        manager.disable();
        persistNotificationPreference(hostId, false);
        setNotificationState({
          supported: manager.supported,
          enabled: false,
          permission: manager.permission,
        });
        return false;
      }
      const granted = await manager.enable();
      persistNotificationPreference(hostId, granted);
      setNotificationState({
        supported: manager.supported,
        enabled: granted,
        permission: manager.permission,
      });
      return granted;
    },
    [state.bootstrap?.host.id],
  );
  useEffect(() => {
    const manager = notificationManagerRef.current;
    const summaries = state.bootstrap?.sessions;
    if (!manager || !summaries || !notificationState.enabled) return;
    const observe = () => {
      for (const summary of summaries) {
        manager.observe(summary, {
          hidden: document.visibilityState !== "visible",
          focused: document.hasFocus(),
          focusWindow: () => window.focus(),
          openSession: selectSession,
        });
      }
    };
    observe();
    document.addEventListener("visibilitychange", observe);
    window.addEventListener("focus", observe);
    window.addEventListener("blur", observe);
    return () => {
      document.removeEventListener("visibilitychange", observe);
      window.removeEventListener("focus", observe);
      window.removeEventListener("blur", observe);
    };
  }, [
    notificationState.enabled,
    selectSession,
    state.bootstrap?.sessions,
  ]);
  const openSettings = useCallback(() => {
    setSurface("settings");
    setInspector(null);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const openProjects = useCallback(() => {
    setSurface("projects");
    setInspector(null);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const openFiles = useCallback(() => {
    setSurface("files");
    setInspector(null);
    setActivityOpen(false);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const openUsage = useCallback(() => {
    setSurface("usage");
    setInspector(null);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const openDevices = useCallback(() => {
    setSurface("devices");
    setInspector(null);
    if (window.matchMedia("(max-width: 760px)").matches) {
      setSidebarOpen(false);
    }
  }, []);
  const submitSession = useCallback(
    (
      prompt: string,
      attachments: AttachmentRef[],
      activeDelivery?: "steer" | "followUp",
      idempotencyKey?: string,
      documents?: DocumentReference[],
      projectFiles?: TrustedFileEntry[],
    ) =>
      store.submit(
        prompt,
        attachments,
        activeDelivery,
        idempotencyKey,
        documents,
        projectFiles,
      ),
    [],
  );
  const interruptSession = useCallback(() => store.interrupt(), []);
  const configureSession = useCallback(
    (patch: {
      modelId?: string;
      reasoning?: ReasoningEffort;
      authority?: AuthorityProfile;
    }) => store.configure(patch),
    [],
  );
  const resolveApproval = useCallback(
    (
      requestId: string,
      decision: "allowed_once" | "allowed_session" | "denied",
    ) => store.resolveApproval(requestId, decision),
    [],
  );
  const resolveUserInput = useCallback(
    (
      requestId: string,
      answer: Parameters<YggStore["resolveUserInput"]>[1],
    ) => store.resolveUserInput(requestId, answer),
    [],
  );
  const ingestAttachment = useCallback(
    (file: File) => store.ingestAttachment(file),
    [],
  );
  const ingestDocument = useCallback(
    (file: File) => store.ingestDocument(file),
    [],
  );
  const selectedProjectId = session?.projectId;
  const listProjectFiles = useCallback(() => {
    if (!selectedProjectId) {
      throw new Error("This conversation is not bound to a project.");
    }
    return store.getTrustedFiles(selectedProjectId);
  }, [selectedProjectId]);
  const searchProjectFiles = useCallback(
    (query: string) => {
      if (!selectedProjectId) {
        throw new Error("This conversation is not bound to a project.");
      }
      return store.searchTrustedFiles(selectedProjectId, query);
    },
    [selectedProjectId],
  );
  const readProjectFile = useCallback(
    (entryId: string) => {
      if (!selectedProjectId) {
        throw new Error("This conversation is not bound to a project.");
      }
      return store.readTrustedFile(selectedProjectId, entryId);
    },
    [selectedProjectId],
  );

  const getProjectFileTree = useCallback(
    (projectId: string, path?: string) => store.getProjectFileTree(projectId, path),
    [],
  );
  const readProjectFileContent = useCallback(
    (projectId: string, path: string, startLine?: number, endLine?: number) =>
      store.readProjectFile(projectId, path, startLine, endLine),
    [],
  );
  const searchProjectFilesystem = useCallback(
    (projectId: string, query: string) => store.searchProjectFiles(projectId, query),
    [],
  );
  const writeProjectFile = useCallback(
    (projectId: string, request: Parameters<YggStore["writeProjectFile"]>[1]) =>
      store.writeProjectFile(projectId, request),
    [],
  );
const getCommandDiscovery = useCallback(
    () => store.getCommandDiscovery(),
    [],
  );
  const invokeSlashCommand = useCallback(
    (invocation: string, idempotencyKey: string) =>
      store.invokeSlashCommand(invocation, idempotencyKey),
    [],
  );
  const exportSession = useCallback(() => {
    if (!session) throw new Error("No session is selected.");
    const link = document.createElement("a");
    link.href = `/api/v1/sessions/${encodeURIComponent(session.sessionId)}/export`;
    link.download = "";
    link.hidden = true;
    document.body.append(link);
    link.click();
    link.remove();
  }, [session]);
  const forkCurrentSession = useCallback(() => {
    const entryId = session?.branches.head;
    if (!entryId) {
      return Promise.reject(
        new Error("This conversation does not have a checkpoint to fork."),
      );
    }
    return store.forkConversation(entryId);
  }, [session?.branches.head]);
  const openRuntimeStatus = useCallback(() => {
    setInspector(null);
    setActivityOpen(true);
  }, []);

  const editUserTurn = useCallback(
    (entryId: string, text: string) =>
      store.editUserTurn(entryId, text),
    [],
  );
  const retryResponse = useCallback(
    (
      entryId: string,
      model?: { id: string; reasoning: ReasoningEffort },
    ) => store.retryResponse(entryId, model),
    [],
  );
  const forkConversation = useCallback(
    (entryId: string) => store.forkConversation(entryId),
    [],
  );
  const attachmentContentUrl = useCallback(
    (handle: string) => store.attachmentContentUrl(handle),
    [],
  );
  const resourceContentUrl = useCallback(
    (sessionId: string, handle: string) =>
      store.resourceContentUrl(sessionId, handle),
    [],
  );
  const renameProject = useCallback(
    (projectId: string, name: string) =>
      store.renameProject(projectId, name),
    [],
  );
  const setDefaultProject = useCallback(
    (projectId: string | null) => store.setDefaultProject(projectId),
    [],
  );
  const setProjectTrust = useCallback(
    (projectId: string, trusted: boolean) =>
      store.setProjectTrust(projectId, trusted),
    [],
  );
  const archiveProject = useCallback(
    (projectId: string) => store.archiveProject(projectId),
    [],
  );
  const loadProjectContext = useCallback(
    (projectId: string) => store.getRepositoryContext(projectId),
    [],
  );
  const loadUsageStats = useCallback(
    (period: UsagePeriod) => store.getUsageStats(period),
    [],
  );
  const loadUsageLifetime = useCallback(() => store.getUsageLifetime(), []);
  const loadUsageActivity = useCallback(() => store.getUsageActivity(), []);

  if (state.connecting) return <LoadingState />;
  if (state.error) {
    return (
      <ErrorState
        message={state.error}
        onRetry={() => void store.initialize()}
      />
    );
  }
  if (!state.bootstrap || !session) {
    if (state.projectCatalog) {
      return (
        <ProjectsView
          catalog={state.projectCatalog}
          onboarding
          onRename={renameProject}
          onSetDefault={setDefaultProject}
          onSetTrust={setProjectTrust}
          onArchive={archiveProject}
          onLoadContext={loadProjectContext}
        />
      );
    }
    return <ErrorState message="No session was selected." />;
  }

  return (
    <div className={appClass} style={appStyle}>
      <Sidebar
        open={sidebarOpen}
        blocked={modalWorkspaceOpen}
        sessions={state.bootstrap.sessions}
        projects={state.bootstrap.projects}
        selectedSessionId={state.selectedSessionId}
        surface={surface}
        devicesAvailable={state.bootstrap.capabilities.connectedDevices}
        filesAvailable={state.bootstrap.capabilities.projectFileBrowser}
        onRestoreFocus={restoreSidebarFocus}
        onClose={closeSidebar}
        onNewSession={startNewSession}
        onSelectSession={selectSession}
        onRestoreSession={(sessionId) => {
          void restoreSession(sessionId);
        }}
        onSetSessionLifecycle={setSessionLifecycle}
        onOpenProjects={openProjects}
        onOpenFiles={openFiles}
        onOpenUsage={openUsage}
        onOpenSettings={openSettings}
        onOpenDevices={openDevices}
        transcriptSearchAvailable={
          state.bootstrap.capabilities.transcriptSearch
        }
        onSearchTranscripts={searchTranscripts}
        onActivateSearchResult={activateSearchResult}
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
            terminalAvailable={terminalAvailable}
            terminalOpen={visibleTerminalOpen}
            pinned={selectedSummary?.pinned ?? false}
            archived={selectedSummary?.archived ?? false}
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
              closeTerminal();
              setInspector(null);
              setActivityOpen((open) => !open);
            }}
            onToggleTerminal={toggleTerminal}
            onRename={(title) => void store.rename(title)}
            onPin={(pinned) => {
              void store.pin(pinned);
            }}
            onArchive={(archived) => {
              void (async () => {
                if (await store.archive(archived) && archived) {
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
            onSubmit={submitSession}
            onInterrupt={interruptSession}
            onConfigure={configureSession}
            onResolveApproval={resolveApproval}
            onResolveUserInput={resolveUserInput}
            onOpenOutput={openOutput}
            onOpenSource={openSource}
            onOpenResource={
              state.bootstrap.capabilities.resources
                ? openResource
                : undefined
            }
            resourceContentUrl={resourceContentUrl}
            onIngestAttachment={ingestAttachment}
            onIngestDocument={ingestDocument}
            onListProjectFiles={listProjectFiles}
            onSearchProjectFiles={searchProjectFiles}
            onReadProjectFile={readProjectFile}
            onGetCommandDiscovery={getCommandDiscovery}
            onInvokeSlashCommand={invokeSlashCommand}
            onExportSession={exportSession}
            onForkSession={forkCurrentSession}
            onOpenRuntimeStatus={openRuntimeStatus}
            onEditUserTurn={editUserTurn}
            onRetryResponse={retryResponse}
            onForkConversation={forkConversation}
            attachmentContentUrl={attachmentContentUrl}
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
            title={
              surface === "settings"
                ? "Settings"
                : surface === "projects"
                  ? "Projects"
                  : surface === "files"
                    ? "Files"
                    : surface === "usage"
                      ? "Usage"
                      : "Connected devices"
            }
            sidebarOpen={sidebarOpen}
            onOpenSidebar={() => setSidebarOpen(true)}
            sidebarButtonRef={sidebarButtonRef}
          />
          <ConnectionBanner connection={state.connection} />
          <FixtureModeLabel />
          {surface === "files" ? (
            <Suspense
              fallback={
                <main className="files-panel files-empty" aria-busy="true">
                  <Folder aria-hidden="true" />
                  <p role="status">Loading project files…</p>
                </main>
              }
            >
              <FilesPanel
                projects={state.bootstrap.projects}
                preferredProjectId={session.projectId}
                writeAvailable={state.bootstrap.capabilities.projectFileWrite}
                getTree={getProjectFileTree}
                readFile={readProjectFileContent}
                searchFiles={searchProjectFilesystem}
                writeFile={writeProjectFile}
              />
            </Suspense>
          ) : surface === "settings" ? (
            <SettingsView
              notificationsSupported={notificationState.supported}
              notificationsEnabled={notificationState.enabled}
              notificationPermission={notificationState.permission}
              onNotificationsChange={changeNotifications}
            />
          ) : surface === "usage" ? (
            <UsagePage
              loadStats={loadUsageStats}
              loadLifetime={loadUsageLifetime}
              loadActivity={loadUsageActivity}
            />
          ) : surface === "projects" && state.projectCatalog ? (
            <ProjectsView
              catalog={state.projectCatalog}
              onRename={renameProject}
              onSetDefault={setDefaultProject}
              onSetTrust={setProjectTrust}
              onArchive={archiveProject}
              onLoadContext={loadProjectContext}
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

      {terminalSplitLayout && visibleTerminalOpen ? (
        <div
          className="pane-resize-handle terminal-pane-resize-handle"
          role="separator"
          aria-label="Resize terminal"
          aria-orientation="vertical"
          aria-valuemin={300}
          aria-valuemax={paneBounds("terminal").max}
          aria-valuenow={terminalPaneWidth}
          tabIndex={0}
          onPointerDown={(event) => {
            event.preventDefault();
            beginPaneResize("terminal", event.clientX);
          }}
          onKeyDown={(event) => {
            if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
              return;
            }
            event.preventDefault();
            resizePaneBy(
              "terminal",
              event.key === "ArrowLeft" ? 16 : -16,
            );
          }}
        />
      ) : null}
      {visibleTerminalOpen ? (
        <TerminalPanel
          hostId={state.bootstrap.host.id}
          onClose={closeTerminal}
        />
      ) : null}

      {surface === "session" && session ? (
        <>
          {wideLayout &&
          !visibleTerminalOpen &&
          ((visibleActivityOpen && !inspector) || inspector) ? (
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
            onOpenResource={
              state.bootstrap.capabilities.resources
                ? openResource
                : undefined
            }
            modal={!wideLayout}
            onRestoreFocus={restoreActivityFocus}
            resourcesAvailable={state.bootstrap.capabilities.resources}
          />
          <MemoizedInspector
            session={session}
            selection={inspector}
            closing={inspectorClosing}
            modal={!wideLayout}
            previewsAvailable={state.bootstrap.capabilities.previews}
            resourceContentUrl={resourceContentUrl}
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
