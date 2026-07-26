import {
  CircleUserRound,
  Laptop,
  Menu,
  MessageSquarePlus,
  PanelLeftClose,
  Search,
  Settings,
  Smartphone,
  X,
} from "lucide-react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import yggIconUrl from "../../../../docs/assets/ygg-braille.svg";
import type { SessionStatus, SessionSummary } from "../protocol";

interface SidebarProps {
  open: boolean;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  surface: "session" | "settings" | "devices";
  hostName: string;
  devicesAvailable: boolean;
  onRestoreFocus: () => void;
  onClose: () => void;
  onNewSession: () => void;
  onSelectSession: (sessionId: string) => void;
  onOpenSettings: () => void;
  onOpenDevices: () => void;
}

const statusLabels: Record<SessionStatus, string> = {
  idle: "Ready",
  working: "Working",
  needs_attention: "Needs attention",
  done: "Done",
  failed: "Failed",
  stopped: "Stopped",
  disconnected: "Disconnected",
};

function SessionRow({
  session,
  selected,
  onSelect,
}: {
  session: SessionSummary;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      className={`session-row ${selected ? "is-selected" : ""}`}
      onClick={onSelect}
      aria-current={selected ? "page" : undefined}
      data-status={session.status}
    >
      <span className="session-status" aria-hidden="true" />
      <span className="session-row-copy">
        <span className="session-row-title">{session.title}</span>
        <span className="session-row-preview">{session.preview}</span>
      </span>
      {session.attentionCount > 0 ? (
        <span
          className="attention-count"
          aria-label={`${session.attentionCount} item needs attention`}
        >
          {session.attentionCount}
        </span>
      ) : (
        <span className="session-status-label">
          {statusLabels[session.status]}
        </span>
      )}
    </button>
  );
}

function SessionSection({
  title,
  sessions,
  selectedSessionId,
  onSelectSession,
}: {
  title: string;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
}) {
  if (sessions.length === 0) return null;
  return (
    <section className="sidebar-section" aria-labelledby={`section-${title}`}>
      <div className="sidebar-section-heading">
        <h2 id={`section-${title}`}>{title}</h2>
        <span>{sessions.length}</span>
      </div>
      <div className="session-list">
        {sessions.map((session) => (
          <SessionRow
            key={session.id}
            session={session}
            selected={session.id === selectedSessionId}
            onSelect={() => onSelectSession(session.id)}
          />
        ))}
      </div>
    </section>
  );
}

export function Sidebar({
  open,
  sessions,
  selectedSessionId,
  surface,
  hostName,
  devicesAvailable,
  onRestoreFocus,
  onClose,
  onNewSession,
  onSelectSession,
  onOpenSettings,
  onOpenDevices,
}: SidebarProps) {
  const [query, setQuery] = useState("");
  const sidebarRef = useRef<HTMLElement>(null);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    if (!open || !window.matchMedia("(max-width: 760px)").matches) return;
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
  }, [onRestoreFocus, open]);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const visibleSessions = useMemo(
    () =>
      sessions.filter(
        (session) =>
          !session.archived &&
          (!normalizedQuery ||
            session.title.toLocaleLowerCase().includes(normalizedQuery) ||
            session.preview.toLocaleLowerCase().includes(normalizedQuery)),
      ),
    [normalizedQuery, sessions],
  );

  const live = visibleSessions.filter(
    (session) =>
      session.status === "working" ||
      session.status === "needs_attention" ||
      session.status === "disconnected",
  );
  const liveIds = new Set(live.map((session) => session.id));
  const pinned = visibleSessions.filter(
    (session) => session.pinned && !liveIds.has(session.id),
  );
  const shown = new Set([...live, ...pinned].map((session) => session.id));
  const recent = visibleSessions.filter((session) => !shown.has(session.id));

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
        aria-label="Ygg"
        inert={!open}
      >
        <header className="sidebar-header">
          <span />
          <button className="icon-button sidebar-close" onClick={onClose}>
            <PanelLeftClose aria-hidden="true" />
            <span className="sr-only">Close sidebar</span>
          </button>
          <button className="mobile-sidebar-close" onClick={onClose}>
            <X aria-hidden="true" />
            <span>Close</span>
          </button>
        </header>

        <div className="brand-row">
          <div className="brand-mark" aria-hidden="true">
            <img src={yggIconUrl} alt="" />
          </div>
          <div>
            <strong>ygg</strong>
            <span>Local agent</span>
          </div>
        </div>

        <nav className="primary-navigation" aria-label="Primary">
          <button className="primary-action" onClick={onNewSession}>
            <MessageSquarePlus aria-hidden="true" />
            <span>New session</span>
          </button>
          <label className="sidebar-search">
            <Search aria-hidden="true" />
            <span className="sr-only">Search sessions</span>
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search"
            />
          </label>
        </nav>

        <div className="sidebar-scroll">
          {visibleSessions.length === 0 ? (
            <div className="sidebar-empty">
              <Menu aria-hidden="true" />
              <span>No matching sessions</span>
            </div>
          ) : (
            <>
              <SessionSection
                title="Live"
                sessions={live}
                selectedSessionId={selectedSessionId}
                onSelectSession={onSelectSession}
              />
              <SessionSection
                title="Pinned"
                sessions={pinned}
                selectedSessionId={selectedSessionId}
                onSelectSession={onSelectSession}
              />
              <SessionSection
                title="Recents"
                sessions={recent}
                selectedSessionId={selectedSessionId}
                onSelectSession={onSelectSession}
              />
            </>
          )}
        </div>

        <footer className="sidebar-footer">
          {devicesAvailable ? (
            <button
              className={`sidebar-destination ${surface === "devices" ? "is-selected" : ""}`}
              onClick={onOpenDevices}
            >
              <span className="destination-icons" aria-hidden="true">
                <Laptop />
                <Smartphone />
              </span>
              <span>
                <strong>Connected devices</strong>
                <small>Manage local connections</small>
              </span>
            </button>
          ) : null}
          <button
            className={`sidebar-destination ${surface === "settings" ? "is-selected" : ""}`}
            onClick={onOpenSettings}
          >
            <Settings aria-hidden="true" />
            <span>
              <strong>Settings</strong>
              <small>Local preferences</small>
            </span>
          </button>
          <div className="local-identity">
            <CircleUserRound aria-hidden="true" />
            <span>
              <strong>{hostName}</strong>
              <small>No account · local only</small>
            </span>
          </div>
        </footer>
      </aside>
    </>
  );
}
