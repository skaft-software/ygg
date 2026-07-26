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
import { useEffect, useMemo, useRef, useState } from "react";
import yggIconUrl from "../../../docs/assets/ygg-braille.svg";
import { ActivityRail } from "./components/ActivityRail";
import { Conversation } from "./components/Conversation";
import { DevicesView } from "./components/Devices";
import {
  Inspector,
  type InspectorSelection,
} from "./components/Inspector";
import { SettingsView } from "./components/Settings";
import { Sidebar } from "./components/Sidebar";
import type { SessionStatus } from "./protocol";
import { YggStore, useYggStore } from "./store";
import { applyTheme } from "./theme";
import { createTransport } from "./transport";

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

const store = new YggStore(createTransport());

function LoadingState() {
  return (
    <div className="app-loading" aria-live="polite">
      <img src={yggIconUrl} alt="" />
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
      <h1>Ygg could not connect</h1>
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
  onOpenSidebar,
  onToggleActivity,
  onRename,
  onPin,
  onArchive,
}: HeaderProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [draftTitle, setDraftTitle] = useState(sessionTitle);

  const submitRename = () => {
    if (draftTitle.trim()) onRename(draftTitle);
    setRenaming(false);
    setMenuOpen(false);
  };

  return (
    <header className="session-header">
      <div className="session-header-leading">
        {!sidebarOpen ? (
          <button className="icon-button open-sidebar" onClick={onOpenSidebar}>
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
              onBlur={submitRename}
              onKeyDown={(event) => {
                if (event.key === "Enter") submitRename();
                if (event.key === "Escape") setRenaming(false);
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
            className={`icon-button ${activityOpen ? "is-active" : ""}`}
            onClick={onToggleActivity}
            aria-label={activityOpen ? "Close activity" : "Open activity"}
          >
            <PanelRight aria-hidden="true" />
          </button>
        ) : null}
        <div className="menu-anchor">
          <button
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
                <button className="danger-row" role="menuitem" onClick={onArchive}>
                  <Archive aria-hidden="true" />
                  Archive
                </button>
              </div>
            </>
          ) : null}
        </div>
      </div>
    </header>
  );
}

function UtilityTopbar({
  title,
  onOpenSidebar,
}: {
  title: string;
  onOpenSidebar: () => void;
}) {
  return (
    <header className="utility-topbar">
      <button className="icon-button" onClick={onOpenSidebar}>
        <Menu aria-hidden="true" />
        <span className="sr-only">Open sidebar</span>
      </button>
      <img src={yggIconUrl} alt="" />
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
  const [activityOpen, setActivityOpen] = useState(false);
  const [inspector, setInspector] = useState<InspectorSelection | null>(null);
  const [surface, setSurface] = useState<Surface>("session");
  const querySelectionApplied = useRef(false);

  useEffect(() => {
    void store.initialize();
    return () => store.dispose();
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
        session.outputs.length ||
        session.sources.length),
  );

  useEffect(() => {
    if (
      querySelectionApplied.current ||
      !state.ready ||
      !state.bootstrap
    ) {
      return;
    }
    querySelectionApplied.current = true;
    const requested = new URLSearchParams(window.location.search).get("session");
    if (
      requested &&
      state.bootstrap.sessions.some((candidate) => candidate.id === requested)
    ) {
      void store.selectSession(requested);
    }
  }, [state.ready, state.bootstrap]);

  useEffect(() => {
    if (selectedTheme) applyTheme(selectedTheme.theme);
  }, [selectedTheme]);

  useEffect(() => {
    const media = window.matchMedia("(max-width: 760px)");
    const onChange = (event: MediaQueryListEvent) => {
      setMobileLayout(event.matches);
      if (event.matches) setSidebarOpen(false);
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
        sessions={state.bootstrap.sessions}
        selectedSessionId={state.selectedSessionId}
        surface={surface}
        hostName={state.bootstrap.host.name}
        devicesAvailable={
          state.bootstrap.capabilities.connectedDevices &&
          state.bootstrap.capabilities.lanClients
        }
        onClose={() => setSidebarOpen(false)}
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
          inert={mobileLayout && sidebarOpen}
        >
          <SessionHeader
            sidebarOpen={sidebarOpen}
            sessionTitle={session.title}
            projectName={project?.name ?? "Local project"}
            status={session.status}
            activityAvailable={activityAvailable}
            activityOpen={activityOpen}
            pinned={selectedSummary?.pinned ?? false}
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
              void store.archive(true);
              void store.createSession();
            }}
          />
          <Conversation
            session={session}
            bootstrap={state.bootstrap}
            onSubmit={(prompt, attachments) =>
              store.submit(prompt, attachments)
            }
            onInterrupt={() => store.interrupt()}
            onConfigure={(patch) => store.configure(patch)}
            onResolveApproval={(requestId, decision) =>
              store.resolveApproval(requestId, decision)
            }
            onOpenOutput={openOutput}
            onOpenSource={openSource}
          />
        </div>
      ) : (
        <div
          className="utility-column"
          inert={mobileLayout && sidebarOpen}
        >
          <UtilityTopbar
            title={surface === "settings" ? "Settings" : "Connected devices"}
            onOpenSidebar={() => setSidebarOpen(true)}
          />
          {surface === "settings" ? (
            <SettingsView
              themes={state.bootstrap.themes}
              selectedThemeId={state.bootstrap.selectedThemeId}
              onThemeChange={(themeId) => void store.selectTheme(themeId)}
            />
          ) : (
            <DevicesView
              hostName={state.bootstrap.host.name}
              devices={state.bootstrap.devices}
              lanAvailable={
                state.bootstrap.capabilities.connectedDevices &&
                state.bootstrap.capabilities.lanClients
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
            onClose={() => setActivityOpen(false)}
            onOpenOutput={openOutput}
            onOpenSource={openSource}
          />
          <Inspector
            session={session}
            selection={inspector}
            onClose={() => setInspector(null)}
          />
        </>
      ) : null}
    </div>
  );
}
