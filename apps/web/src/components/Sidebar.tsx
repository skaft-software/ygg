import {
  Menu,
  MessageSquarePlus,
  PanelLeftClose,
  Settings,
  X,
} from "lucide-react";
import {
  useEffect,
  useRef,
} from "react";
import type { SessionSummary } from "../protocol";
import type { TransportConnectionState } from "../transport";
import { YggGlyph } from "./YggGlyph";

interface SidebarProps {
  open: boolean;
  sessions: SessionSummary[];
  selectedSessionId: string | null;
  surface: "session" | "settings" | "devices";
  hostName: string;
  connection: TransportConnectionState;
  onRestoreFocus: () => void;
  onClose: () => void;
  onNewSession: () => void;
  onSelectSession: (sessionId: string) => void;
  onOpenSettings: () => void;
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
      aria-label={`Open session ${session.title}`}
      data-status={session.status}
    >
      <span className="session-status" aria-hidden="true" />
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
  sessions,
  selectedSessionId,
  surface,
  hostName,
  connection,
  onRestoreFocus,
  onClose,
  onNewSession,
  onSelectSession,
  onOpenSettings,
}: SidebarProps) {
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
  const visibleSessions = sessions.filter((session) => !session.archived);
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
        inert={!open}
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
        </nav>

        <div className="sidebar-scroll">
          {visibleSessions.length === 0 ? (
            <div className="sidebar-empty">
              <Menu aria-hidden="true" />
              <span>No sessions yet</span>
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
