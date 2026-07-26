import {
  Archive,
  ChevronDown,
  Folder,
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
import type { SessionStatus } from "./protocol";
import {
  sessionIdFromPathname,
  YggStore,
  useYggStore,
} from "./store";
import { applyStoredTypePreferences, applyTheme } from "./theme";
import { createTransport, transportModeFromSearch } from "./transport";

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

function FixtureModeLabel() {
  if (transportMode !== "fixture") return null;
  return (
    <div className="fixture-mode-label" role="status">
      Demo data · responses and actions are simulated
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

function ErrorState({ message }: { message: string }) {
  return (
    <div className="app-error" role="alert">
      <div className="error-mark">
        <X aria-hidden="true" />
      </div>
      <h1>ygg could not connect</h1>
      <p>{message}</p>
      <button className="primary-button" onClick={() => window.location.reload()}>
        <RefreshCw aria-hidden="true" />
        Try again
      </button>
    </div>
  );
}

interface HeaderProps {
  sidebarOpen: boolean;
  sessionTitle: string;
  projectName: string;
  status: SessionStatus;
  activityAvailable: boolean;
  activityOpen: boolean;
  pinned: boolean;
  sessionActionsAvailable: boolean;
  activityButtonRef: RefObject<HTMLButtonElement | null>;
  sidebarButtonRef: RefObject<HTMLButtonElement | null>;
  onOpenSidebar: () => void;
  onToggleActivity: () => void;
  onRename: (title: string) => void;
  onPin: (pinned: boolean) => void;
  onArchive: () => void;
}

function SessionHeader({
  sidebarOpen,
  sessionTitle,
  projectName,
  status,
  activityAvailable,
  activityOpen,
  pinned,
  sessionActionsAvailable,
  activityButtonRef,
  sidebarButtonRef,
  onOpenSidebar,
  onToggleActivity,
  onRename,
  onPin,
  onArchive,
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
                </div>
              </>
            ) : null}
          </div>
        ) : null}
      </div>
    </header>
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
  const [inspector, setInspector] = useState<InspectorSelection | null>(null);
  const [surface, setSurface] = useState<Surface>("session");
  const activitySeenRef = useRef(new Map<string, boolean>());
  const activityDismissedRef = useRef(new Set<string>());
  const activityButtonRef = useRef<HTMLButtonElement>(null);
  const sidebarButtonRef = useRef<HTMLButtonElement>(null);
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
    if (state.selectedSessionId) {
      activityDismissedRef.current.add(state.selectedSessionId);
    }
    setActivityOpen(false);
    restoreActivityFocus();
  }, [restoreActivityFocus, state.selectedSessionId]);
  const closeInspector = useCallback(() => {
    setInspector(null);
    restoreActivityFocus();
  }, [restoreActivityFocus]);
  const closeSidebar = useCallback(() => {
    setSidebarOpen(false);
    restoreSidebarFocus();
  }, [restoreSidebarFocus]);
  const modalWorkspaceOpen =
    !wideLayout && (activityOpen || Boolean(inspector));

  useEffect(() => {
    applyStoredTypePreferences();
    void store.initialize();
    return () => store.dispose();
  }, []);

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
        session.items.some(
          (item) => item.kind === "action" || item.kind === "reasoning",
        ) ||
        (state.bootstrap?.capabilities.resources &&
          (session.outputs.length || session.sources.length))),
  );

  useEffect(() => {
    if (!session) return;
    const alreadySeen = activitySeenRef.current.get(session.sessionId) ?? false;
    activitySeenRef.current.set(session.sessionId, activityAvailable);
    if (
      activityAvailable &&
      !alreadySeen &&
      wideLayout &&
      !inspector &&
      !activityDismissedRef.current.has(session.sessionId)
    ) {
      setActivityOpen(true);
    }
  }, [activityAvailable, inspector, session, wideLayout]);

  useEffect(() => {
    if (selectedTheme) applyTheme(selectedTheme.theme);
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
    setActivityOpen(false);
    setInspector({ type: "output", id: outputId });
  };
  const openSource = (sourceId: string) => {
    setActivityOpen(false);
    setInspector({ type: "source", id: sourceId });
  };

  const appClass = useMemo(
    () =>
      [
        "app-shell",
        sidebarOpen ? "has-sidebar" : "",
        activityOpen && !inspector ? "has-activity" : "",
        inspector ? "has-inspector" : "",
        `surface-${surface}`,
      ]
        .filter(Boolean)
        .join(" "),
    [activityOpen, inspector, sidebarOpen, surface],
  );

  if (state.connecting) return <LoadingState />;
  if (state.error) return <ErrorState message={state.error} />;
  if (!state.bootstrap || !session) return <ErrorState message="No session was selected." />;

  return (
    <div className={appClass}>
      <Sidebar
        open={sidebarOpen}
        blocked={modalWorkspaceOpen}
        sessions={state.bootstrap.sessions}
        selectedSessionId={state.selectedSessionId}
        surface={surface}
        hostName={state.bootstrap.host.name}
        connection={state.connection}
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
          setActivityOpen(false);
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
            sessionTitle={session.title}
            projectName={project?.name ?? "Local project"}
            status={session.status}
            activityAvailable={activityAvailable}
            activityOpen={activityOpen}
            pinned={selectedSummary?.pinned ?? false}
            sessionActionsAvailable={
              state.bootstrap.capabilities.sessionMetadata
            }
            activityButtonRef={activityButtonRef}
            sidebarButtonRef={sidebarButtonRef}
            onOpenSidebar={() => setSidebarOpen(true)}
            onToggleActivity={() => {
              setInspector(null);
              setActivityOpen((open) => {
                if (open) {
                  activityDismissedRef.current.add(session.sessionId);
                }
                return !open;
              });
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
          />
          <FixtureModeLabel />
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
          <ActivityRail
            session={session}
            open={
              activityOpen &&
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
            modal={!wideLayout}
            previewsAvailable={state.bootstrap.capabilities.previews}
            resourceContentUrl={(handle) => store.resourceContentUrl(handle)}
            onRestoreFocus={restoreActivityFocus}
            onClose={closeInspector}
          />
        </>
      ) : null}
    </div>
  );
}
