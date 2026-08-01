import type { SessionSummary } from "./protocol";
import { displaySessionTitle } from "./session-title";

const NOTIFICATION_VERSION = 1;
const MAX_REMEMBERED_TRANSITIONS = 256;

export interface NotificationHandle {
  setOnClick(handler: () => void): void;
  close(): void;
}

export interface NotificationAdapter {
  permission(): NotificationPermission;
  requestPermission(): Promise<NotificationPermission>;
  show(
    title: string,
    options: NotificationOptions,
  ): NotificationHandle;
}

export interface NotificationStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export interface AttentionEnvironment {
  hidden: boolean;
  focused: boolean;
  focusWindow(): void;
  openSession(sessionId: string): void;
}

interface PersistedTransitions {
  version: typeof NOTIFICATION_VERSION;
  keys: string[];
  states?: Record<string, string>;
}

function storageKey(hostId: string): string {
  return `ygg.notifications.v${NOTIFICATION_VERSION}.${encodeURIComponent(hostId)}`;
}

function transitionKey(summary: SessionSummary): string {
  return [
    summary.id,
    summary.status,
    summary.updatedAt,
    summary.attentionCount,
    summary.unread ? "unread" : "read",
  ].join("\u001f");
}

function attentionState(summary: SessionSummary): string {
  return [
    summary.status,
    summary.attentionCount,
    summary.unread ? "unread" : "read",
  ].join("\u001f");
}

function notificationCopy(summary: SessionSummary): {
  title: string;
  body: string;
} | null {
  if (!summary.unread && summary.attentionCount === 0) return null;
  const taskTitle = displaySessionTitle(summary.title);
  switch (summary.status) {
    case "needs_attention":
      return {
        title: "ygg needs your attention",
        body: `${taskTitle} is waiting for approval or input.`,
      };
    case "failed":
      return {
        title: "ygg task failed",
        body: `${taskTitle} needs review.`,
      };
    case "done":
      return {
        title: "ygg finished",
        body: `${taskTitle} is ready to review.`,
      };
    default:
      return null;
  }
}

export class AttentionNotificationManager {
  private readonly seen = new Set<string>();
  private readonly states = new Map<string, string>();
  private enabled = false;

  constructor(
    private readonly hostId: string,
    private readonly adapter: NotificationAdapter | null,
    private readonly storage?: NotificationStorage,
  ) {
    this.restore();
  }

  get supported(): boolean {
    return this.adapter !== null;
  }

  get permission(): NotificationPermission | "unsupported" {
    return this.adapter?.permission() ?? "unsupported";
  }

  async enable(): Promise<boolean> {
    if (!this.adapter) return false;
    const permission =
      this.adapter.permission() === "default"
        ? await this.adapter.requestPermission()
        : this.adapter.permission();
    this.enabled = permission === "granted";
    return this.enabled;
  }

  disable(): void {
    this.enabled = false;
  }

  observe(
    summary: SessionSummary,
    environment: AttentionEnvironment,
  ): boolean {
    if (
      !this.enabled ||
      !this.adapter ||
      this.adapter.permission() !== "granted"
    ) {
      return false;
    }
    const state = attentionState(summary);
    if (this.states.get(summary.id) === state) return false;
    this.states.set(summary.id, state);
    const copy = notificationCopy(summary);
    if (!copy) {
      this.persist();
      return false;
    }
    const key = transitionKey(summary);
    const replayed = this.seen.has(key);
    this.remember(key);
    if (replayed || (!environment.hidden && environment.focused)) return false;

    const notification = this.adapter.show(copy.title, {
      body: copy.body,
      tag: `ygg-session-${summary.id}`,
      silent: false,
    });
    notification.setOnClick(() => {
      notification.close();
      environment.focusWindow();
      environment.openSession(summary.id);
    });
    return true;
  }

  private restore(): void {
    if (!this.storage) return;
    let raw: string | null;
    try {
      raw = this.storage.getItem(storageKey(this.hostId));
    } catch {
      return;
    }
    if (!raw) return;
    try {
      const parsed = JSON.parse(raw) as Partial<PersistedTransitions>;
      if (
        parsed.version !== NOTIFICATION_VERSION ||
        !Array.isArray(parsed.keys)
      ) {
        return;
      }
      for (const key of parsed.keys.slice(-MAX_REMEMBERED_TRANSITIONS)) {
        if (typeof key === "string" && key.length <= 2_048) {
          this.seen.add(key);
        }
      }
      if (parsed.states && typeof parsed.states === "object") {
        for (const [sessionId, state] of Object.entries(parsed.states).slice(
          -MAX_REMEMBERED_TRANSITIONS,
        )) {
          if (
            sessionId.length > 0 &&
            sessionId.length <= 256 &&
            typeof state === "string" &&
            state.length <= 512
          ) {
            this.states.set(sessionId, state);
          }
        }
      }
    } catch {
      // Invalid device-local state is ignored and replaced on the next event.
    }
  }

  private remember(key: string): void {
    this.seen.add(key);
    while (this.seen.size > MAX_REMEMBERED_TRANSITIONS) {
      const oldest = this.seen.values().next().value;
      if (typeof oldest !== "string") break;
      this.seen.delete(oldest);
    }
    this.persist();
  }

  private persist(): void {
    while (this.states.size > MAX_REMEMBERED_TRANSITIONS) {
      const oldest = this.states.keys().next().value;
      if (typeof oldest !== "string") break;
      this.states.delete(oldest);
    }
    try {
      this.storage?.setItem(
        storageKey(this.hostId),
        JSON.stringify({
          version: NOTIFICATION_VERSION,
          keys: [...this.seen],
          states: Object.fromEntries(this.states),
        } satisfies PersistedTransitions),
      );
    } catch {
      // In-memory transition tracking remains valid for this page lifetime.
    }
  }
}

export function browserNotificationAdapter(): NotificationAdapter | null {
  if (typeof Notification === "undefined") return null;
  return {
    permission: () => Notification.permission,
    requestPermission: () => Notification.requestPermission(),
    show: (title, options) => {
      const notification = new Notification(title, options);
      return {
        setOnClick: (handler) => {
          notification.onclick = handler;
        },
        close: () => notification.close(),
      };
    },
  };
}
