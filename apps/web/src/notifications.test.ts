import { describe, expect, it, vi } from "vitest";
import type { SessionSummary } from "./protocol";
import {
  AttentionNotificationManager,
  type NotificationAdapter,
  type NotificationHandle,
  type NotificationStorage,
} from "./notifications";

function summary(
  patch: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    id: "session-one",
    projectId: "project-one",
    title: "Review the release",
    preview: "",
    status: "done",
    updatedAt: "2026-07-27T03:30:00Z",
    pinned: false,
    archived: false,
    lifecycle: "active",
    unread: true,
    modelId: "model",
    attentionCount: 0,
    ...patch,
  };
}

function memoryStorage(): NotificationStorage {
  const values = new Map<string, string>();
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
  };
}

function grantedAdapter() {
  const handles: NotificationHandle[] = [];
  const show = vi.fn(() => {
    let clickHandler: (() => void) | null = null;
    const handle: NotificationHandle = {
      setOnClick: (handler) => {
        clickHandler = handler;
      },
      close: vi.fn(),
    };
    Object.defineProperty(handle, "click", {
      value: () => clickHandler?.(),
    });
    handles.push(handle);
    return handle;
  });
  const adapter: NotificationAdapter = {
    permission: () => "granted",
    requestPermission: vi.fn(
      async (): Promise<NotificationPermission> => "granted",
    ),
    show,
  };
  return { adapter, handles, show };
}

const background = () => ({
  hidden: true,
  focused: false,
  focusWindow: vi.fn(),
  openSession: vi.fn(),
});

describe("attention notification manager", () => {
  it("notifies once for a background attention transition and deep-links", async () => {
    const { adapter, handles, show } = grantedAdapter();
    const manager = new AttentionNotificationManager(
      "host",
      adapter,
      memoryStorage(),
    );
    expect(await manager.enable()).toBe(true);
    const environment = background();

    expect(manager.observe(summary(), environment)).toBe(true);
    expect(manager.observe(summary(), environment)).toBe(false);
    expect(show).toHaveBeenCalledWith(
      "ygg finished",
      expect.objectContaining({
        body: "Review the release is ready to review.",
        tag: "ygg-session-session-one",
      }),
    );

    (
      handles[0] as NotificationHandle & { click?: () => void }
    )?.click?.();
    expect(handles[0]?.close).toHaveBeenCalled();
    expect(environment.focusWindow).toHaveBeenCalled();
    expect(environment.openSession).toHaveBeenCalledWith("session-one");
  });

  it("deduplicates replayed transitions across page reloads", async () => {
    const storage = memoryStorage();
    const first = grantedAdapter();
    const firstManager = new AttentionNotificationManager(
      "host",
      first.adapter,
      storage,
    );
    await firstManager.enable();
    expect(firstManager.observe(summary(), background())).toBe(true);

    const second = grantedAdapter();
    const secondManager = new AttentionNotificationManager(
      "host",
      second.adapter,
      storage,
    );
    await secondManager.enable();
    expect(secondManager.observe(summary(), background())).toBe(false);
    expect(second.show).not.toHaveBeenCalled();
  });

  it("does not notify while focused or for non-attention states", async () => {
    const { adapter, show } = grantedAdapter();
    const manager = new AttentionNotificationManager("host", adapter);
    await manager.enable();
    expect(
      manager.observe(summary(), {
        ...background(),
        hidden: false,
        focused: true,
      }),
    ).toBe(false);
    expect(
      manager.observe(
        summary({
          status: "working",
          updatedAt: "2026-07-27T03:31:00Z",
        }),
        background(),
      ),
    ).toBe(false);
    expect(show).not.toHaveBeenCalled();
  });

  it("records foreground transitions and ignores metadata-only updates", async () => {
    const { adapter, show } = grantedAdapter();
    const manager = new AttentionNotificationManager(
      "host",
      adapter,
      memoryStorage(),
    );
    await manager.enable();
    const completed = summary();

    expect(
      manager.observe(completed, {
        ...background(),
        hidden: false,
        focused: true,
      }),
    ).toBe(false);
    expect(manager.observe(completed, background())).toBe(false);
    expect(
      manager.observe(
        {
          ...completed,
          title: "Renamed release review",
          pinned: true,
          updatedAt: "2026-07-27T03:31:00Z",
        },
        background(),
      ),
    ).toBe(false);

    expect(
      manager.observe(
        summary({
          status: "working",
          unread: false,
          updatedAt: "2026-07-27T03:32:00Z",
        }),
        background(),
      ),
    ).toBe(false);
    expect(
      manager.observe(
        summary({ updatedAt: "2026-07-27T03:33:00Z" }),
        background(),
      ),
    ).toBe(true);
    expect(show).toHaveBeenCalledTimes(1);
  });

  it("degrades gracefully when permission is denied or unsupported", async () => {
    const denied: NotificationAdapter = {
      permission: () => "denied",
      requestPermission: vi.fn(
        async (): Promise<NotificationPermission> => "denied",
      ),
      show: vi.fn(() => ({
        setOnClick: vi.fn(),
        close: vi.fn(),
      })),
    };
    const manager = new AttentionNotificationManager("host", denied);
    expect(await manager.enable()).toBe(false);
    expect(manager.observe(summary(), background())).toBe(false);
    expect(denied.show).not.toHaveBeenCalled();

    const unsupported = new AttentionNotificationManager("host", null);
    expect(unsupported.supported).toBe(false);
    expect(await unsupported.enable()).toBe(false);
  });
});
