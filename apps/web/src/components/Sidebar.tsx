import {
  AlertCircle,
  Circle,
  Laptop,
  LoaderCircle,
  Menu,
  MessageSquarePlus,
  PanelLeftClose,
  Search,
  Settings,
  X,
} from "lucide-react";
import {
  useEffect,
  useRef,
  useState,
} from "react";
import type { SessionSummary } from "../protocol";
import type { TransportConnectionState } from "../transport";
import { YggGlyph } from "./YggGlyph";

interface SidebarProps {
  open: boolean;
  blocked: boolean;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  surface: "session" | "settings" | "devices";
  hostName: string;
  connection: TransportConnectionState;
  devicesAvailable: boolean;
  onRestoreFocus: () => void;
  onClose: () => void;
  onNewSession: () => void;
  onSelectSession: (sessionId: string) => void;
  onOpenDevices: () => void;
  onOpenSettings: () => void;
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

function SessionStatusGlyph({ status }: { status: SessionSummary["status"] }) {
  if (status === "working" || status === "disconnected") {
    return <LoaderCircle className="spin" aria-hidden="true" />;
  }
  if (status === "needs_attention" || status === "failed") {
    return <AlertCircle aria-hidden="true" />;
  }
  return <Circle aria-hidden="true" />;
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
  return (
    <button
      className={`session-row ${selected ? "is-selected" : ""}`}
      onClick={onSelect}
      aria-current={selected ? "page" : undefined}
      aria-label={`Open session ${session.title}, ${sessionStatusLabel[session.status]}`}
      data-status={session.status}
    >
      <span className="session-status">
        <SessionStatusGlyph status={session.status} />
        <span className="sr-only">{sessionStatusLabel[session.status]}</span>
      </span>
      <span className="session-row-copy">
        <span className="session-row-title">{session.title}</span>
      </span>
      {session.attentionCount > 0 ? (
        <span
          className="attention-count"
          aria-label={`${session.attentionCount} item needs attention`}
        >
          {session.attentionCount}
        </span>
      ) : null}
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
  blocked,
  sessions,
  selectedSessionId,
  surface,
  hostName,
  connection,
  devicesAvailable,
  onRestoreFocus,
  onClose,
  onNewSession,
  onSelectSession,
  onOpenDevices,
  onOpenSettings,
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
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const visibleSessions = sessions.filter(
    (session) =>
      !session.archived &&
      (!normalizedQuery ||
        session.title.toLocaleLowerCase().includes(normalizedQuery) ||
        session.preview.toLocaleLowerCase().includes(normalizedQuery)),
  );
  const pinned = visibleSessions.filter((session) => session.pinned);
  const pinnedIds = new Set(pinned.map((session) => session.id));
  const recent = visibleSessions.filter((session) => !pinnedIds.has(session.id));

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
            <YggGlyph />
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
              placeholder="Search sessions"
            />
          </label>
        </nav>

        <div className="sidebar-scroll">
          {visibleSessions.length === 0 ? (
            <div className="sidebar-empty">
              {normalizedQuery ? (
                <Search aria-hidden="true" />
              ) : (
                <Menu aria-hidden="true" />
              )}
              <span>
                {normalizedQuery ? "No matching sessions" : "No sessions yet"}
              </span>
            </div>
          ) : (
            <>
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
          <div className="local-identity">
            <span className="local-avatar" aria-hidden="true">Y</span>
            <span>
              <strong>{hostName}</strong>
              <small
                className="connection-label"
                data-connection={connection}
                role="status"
              >
                <span aria-hidden="true" />
                {connection === "connected"
                  ? "Connected to local ygg"
                  : connection === "reconnecting"
                    ? "Reconnecting to ygg…"
                    : "Connecting to ygg…"}
              </small>
            </span>
          </div>
        </footer>
      </aside>
    </>
  );
}
