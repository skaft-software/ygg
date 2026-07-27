import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import eventEnvelopeGolden from "../../../extensions/ygg-serve/fixtures/event-envelope.json";
import hostBootstrapGolden from "../../../extensions/ygg-serve/fixtures/host-bootstrap.json";
import {
  createTransport,
  FixtureTransport,
  HttpTransport,
  resolveClientDeviceId,
  transportModeFromSearch,
} from "./transport";

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];

  readonly url: string;
  private listeners = new Map<
    string,
    Set<EventListenerOrEventListenerObject>
  >();

  constructor(url: string | URL) {
    this.url = String(url);
    FakeWebSocket.instances.push(this);
  }

  addEventListener(
    type: string,
    callback: EventListenerOrEventListenerObject | null,
  ): void {
    if (!callback) return;
    const listeners =
      this.listeners.get(type) ??
      new Set<EventListenerOrEventListenerObject>();
    listeners.add(callback);
    this.listeners.set(type, listeners);
  }

  close(): void {
    this.emit("close", new Event("close"));
  }

  emit(type: string, event: Event): void {
    for (const listener of this.listeners.get(type) ?? []) {
      if (typeof listener === "function") {
        listener(event);
      } else {
        listener.handleEvent(event);
      }
    }
  }
}

const jsonResponse = (value: unknown) =>
  new Response(JSON.stringify(value), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });

describe("HTTP Ygg transport", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal(
      "WebSocket",
      FakeWebSocket as unknown as typeof WebSocket,
    );
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    try {
      window.localStorage?.clear();
    } catch {
      // Some hardened/jsdom environments intentionally omit storage.
    }
    document.documentElement.removeAttribute("data-ygg-device-id");
    window.history.replaceState(null, "", "/");
  });

  it("uses live HTTP by default and fixtures only when explicitly requested", () => {
    window.history.replaceState(null, "", "/");
    expect(transportModeFromSearch(window.location.search)).toBe("live");
    expect(createTransport()).toBeInstanceOf(HttpTransport);

    window.history.replaceState(null, "", "/?transport=fixture");
    expect(transportModeFromSearch(window.location.search)).toBe("fixture");
    expect(createTransport()).toBeInstanceOf(FixtureTransport);

    expect(transportModeFromSearch("?transport=Fixture")).toBe("live");
    expect(transportModeFromSearch("?transport=fixture-preview")).toBe("live");
  });

  it("scopes opaque resource URLs to the owning session", () => {
    expect(
      new FixtureTransport().resourceContentUrl(
        "session with space",
        "resource/handle",
      ),
    ).toBe(
      "/api/v1/sessions/session%20with%20space/resources/resource%2Fhandle",
    );
    expect(
      new HttpTransport("device-browser").resourceContentUrl(
        "session-demo",
        "resource-handle",
      ),
    ).toBe(
      "/api/v1/sessions/session-demo/resources/resource-handle",
    );
  });

  it("fixture checkout replaces the selected transcript instead of merging branches", async () => {
    const transport = new FixtureTransport();
    await transport.connect();
    const events: unknown[] = [];
    transport.subscribe((event) => events.push(event));

    await transport.send({
      id: "command-fixture-checkout",
      type: "session.checkout",
      sessionId: "session-done",
      entryId: "entry-release-draft",
    });
    const replacement = await transport.getSession("session-done");

    expect(replacement.branches.head).toBe("entry-release-draft");
    expect(replacement.items.map((item) => item.id)).toEqual([
      "done-user",
      "done-draft",
    ]);
    expect(replacement.sources).toEqual([]);
    expect(replacement.outputs).toEqual([]);
    expect(replacement.previews).toEqual([]);
    expect(events).toContainEqual(
      expect.objectContaining({
        type: "session.projectionReplaced",
        durableHead: "entry-release-draft",
      }),
    );
    transport.close();
  });

  it("fixture resource upserts merge by identity without losing prior evidence", async () => {
    const transport = new FixtureTransport();
    await transport.connect();
    const sessionId = "session-fresh";
    const emit = (
      transport as unknown as {
        emit: (event: {
          type: "session.resources";
          sessionId: string;
          sequence: number;
          merge: true;
          sources: Array<{
            id: string;
            kind: "file";
            title: string;
            subtitle: string;
            consultedAt: string;
            iconLabel: string;
          }>;
        }) => void;
      }
    ).emit.bind(transport);

    emit({
      type: "session.resources",
      sessionId,
      sequence: 90,
      merge: true,
      sources: [
        {
          id: "source-one",
          kind: "file",
          title: "one.ts",
          subtitle: "Consulted",
          consultedAt: new Date(0).toISOString(),
          iconLabel: "SRC",
        },
      ],
    });
    emit({
      type: "session.resources",
      sessionId,
      sequence: 91,
      merge: true,
      sources: [
        {
          id: "source-two",
          kind: "file",
          title: "two.ts",
          subtitle: "Consulted",
          consultedAt: new Date(0).toISOString(),
          iconLabel: "SRC",
        },
      ],
    });

    expect(
      (await transport.getSession(sessionId)).sources.map(
        (source) => source.id,
      ),
    ).toEqual(["source-one", "source-two"]);
    transport.close();
  });

  it("persists a valid non-empty loopback device identity", () => {
    const first = resolveClientDeviceId();
    const second = resolveClientDeviceId();

    expect(first).toMatch(/^browser-[A-Za-z0-9-]+$/);
    expect(second).toBe(first);
    expect(first?.length).toBeLessThanOrEqual(128);
  });

  it("puts the generated loopback identity into a live command envelope", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(
        jsonResponse({
          protocol: 1,
          sessionId: "session-demo",
          commandId: "command-live",
          acknowledgedAtMs: 1_721_000_000_051,
          cursor: { actorGeneration: 3, sequence: 43 },
          disposition: { status: "accepted", runId: "run-live" },
        }),
      );
    vi.stubGlobal("fetch", fetchMock);
    const transport = createTransport();
    await transport.connect();
    await transport.send({
      id: "command-live",
      type: "session.submit",
      sessionId: "session-demo",
      prompt: "Start",
      attachments: [],
    });

    expect(JSON.parse(String(fetchMock.mock.calls[1]?.[1]?.body))).toMatchObject(
      {
        hostId: "host-demo",
        deviceId: expect.stringMatching(/^browser-[A-Za-z0-9-]+$/),
        commandId: "command-live",
      },
    );
    transport.close();
  });

  it("requests the routed session in the bootstrap projection", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse(hostBootstrapGolden));
    vi.stubGlobal("fetch", fetchMock);
    const transport = new HttpTransport("device-browser");

    await transport.connect("session-demo");

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/bootstrap?selectedSessionId=session-demo",
      expect.objectContaining({ credentials: "same-origin" }),
    );
    transport.close();
  });

  it("preserves the exact command envelope across a network retry", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockRejectedValueOnce(new TypeError("connection reset"))
      .mockResolvedValueOnce(
        jsonResponse({
          protocol: 1,
          sessionId: "session-demo",
          commandId: "command-submit",
          acknowledgedAtMs: 1_721_000_000_051,
          cursor: { actorGeneration: 3, sequence: 43 },
          disposition: { status: "accepted", runId: "run-1" },
        }),
      );
    vi.stubGlobal("fetch", fetchMock);
    vi.spyOn(Date, "now").mockReturnValue(1_721_000_000_050);
    const transport = new HttpTransport("device-browser");
    await transport.connect();
    const command = {
      id: "command-submit",
      type: "session.submit" as const,
      sessionId: "session-demo",
      prompt: "Continue",
      attachments: [],
    };

    await expect(transport.send(command)).rejects.toThrow("connection reset");
    await expect(transport.send(command)).resolves.toMatchObject({
      commandId: "command-submit",
      accepted: true,
    });

    expect(fetchMock.mock.calls[1]?.[0]).toBe("/api/v1/commands/session");
    expect(fetchMock.mock.calls[2]?.[0]).toBe("/api/v1/commands/session");
    expect(fetchMock.mock.calls[1]?.[1]?.body).toBe(
      fetchMock.mock.calls[2]?.[1]?.body,
    );
    expect(JSON.parse(String(fetchMock.mock.calls[1]?.[1]?.body))).toMatchObject(
      {
        hostId: "host-demo",
        deviceId: "device-browser",
        commandId: "command-submit",
        issuedAtMs: 1_721_000_000_050,
        expectedActorGeneration: 3,
      },
    );
    transport.close();
  });

  it("decodes the nested host WebSocket envelope before publishing", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        jsonResponse(hostBootstrapGolden),
      ),
    );
    const transport = new HttpTransport("device-browser");
    const received: unknown[] = [];
    transport.subscribe((event) => received.push(event));
    await transport.connect();

    const socket = FakeWebSocket.instances[0];
    expect(socket?.url).toMatch(/\/api\/v1\/events$/);
    socket?.emit(
      "message",
      new MessageEvent("message", {
        data: JSON.stringify({
          protocol: 1,
          hostSequence: 9,
          event: eventEnvelopeGolden,
        }),
      }),
    );

    expect(received).toEqual([
      {
        type: "item.delta",
        sessionId: "session-demo",
        actorGeneration: 3,
        sequence: 43,
        itemId: "item-stream",
        field: "content",
        delta: " world",
      },
    ]);
    transport.close();
  });

  it("publishes cross-client catalog summaries from the host stream", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        jsonResponse(hostBootstrapGolden),
      ),
    );
    const transport = new HttpTransport("device-browser");
    const received: unknown[] = [];
    transport.subscribe((event) => received.push(event));
    await transport.connect();
    const summary = structuredClone(hostBootstrapGolden.sessions[0]);
    summary.id = "session-from-phone";
    summary.title = "Created from phone";

    FakeWebSocket.instances[0]?.emit(
      "message",
      new MessageEvent("message", {
        data: JSON.stringify({
          protocol: 1,
          hostSequence: 10,
          catalog: {
            catalogCursor: 9,
            summary,
          },
        }),
      }),
    );

    expect(received).toEqual([
      expect.objectContaining({
        type: "catalog.summary",
        catalogRevision: 9,
        summary: expect.objectContaining({
          id: "session-from-phone",
          title: "Created from phone",
        }),
      }),
    ]);
    transport.close();
  });

  it("replays the bootstrap cursor on the first WebSocket open", async () => {
    const replay = {
      type: "events",
      after: { actorGeneration: 3, sequence: 42 },
      through: { actorGeneration: 3, sequence: 43 },
      events: [eventEnvelopeGolden],
    };
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(jsonResponse(replay))
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden));
    vi.stubGlobal("fetch", fetchMock);
    const transport = new HttpTransport("device-browser");
    const received: unknown[] = [];
    transport.subscribe((event) => received.push(event));
    await transport.connect();

    FakeWebSocket.instances[0]?.emit("open", new Event("open"));

    await vi.waitFor(() => expect(received).toHaveLength(1));
    expect(fetchMock.mock.calls[1]?.[0]).toBe(
      "/api/v1/sessions/session-demo/replay?actorGeneration=3&sequence=42",
    );
    expect(received[0]).toMatchObject({
      type: "item.delta",
      sessionId: "session-demo",
      sequence: 43,
    });
    transport.close();
  });

  it("keeps a projection replacement behind the replay cursor until a fresh snapshot installs", async () => {
    const replacementEnvelope = structuredClone(eventEnvelopeGolden) as {
      cursor: { actorGeneration: number; sequence: number };
      event: unknown;
      [key: string]: unknown;
    };
    replacementEnvelope.cursor.sequence = 43;
    replacementEnvelope.event = {
      type: "session.projectionReplaced",
      data: { durableEntryId: "entry-42" },
    };
    const staleSnapshot = structuredClone(hostBootstrapGolden.selectedSession);
    const freshSnapshot = structuredClone(hostBootstrapGolden.selectedSession);
    freshSnapshot.cursor.sequence = 43;
    const replay = {
      type: "events",
      after: { actorGeneration: 3, sequence: 42 },
      through: { actorGeneration: 3, sequence: 43 },
      events: [replacementEnvelope],
    };
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(jsonResponse(staleSnapshot))
      .mockResolvedValueOnce(jsonResponse(replay))
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(jsonResponse(freshSnapshot));
    vi.stubGlobal("fetch", fetchMock);
    const transport = new HttpTransport("device-browser");
    await transport.connect();
    await transport.getSession("session-demo");

    FakeWebSocket.instances[0]?.emit(
      "message",
      new MessageEvent("message", {
        data: JSON.stringify({
          protocol: 1,
          hostSequence: 11,
          event: replacementEnvelope,
        }),
      }),
    );
    await expect(transport.getSession("session-demo")).rejects.toThrow(
      "predates the required projection replacement",
    );

    FakeWebSocket.instances[0]?.emit("close", new Event("close"));
    await vi.waitFor(() => expect(FakeWebSocket.instances).toHaveLength(2), {
      timeout: 1_000,
    });
    FakeWebSocket.instances[1]?.emit("open", new Event("open"));
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(4));
    expect(fetchMock.mock.calls[2]?.[0]).toBe(
      "/api/v1/sessions/session-demo/replay?actorGeneration=3&sequence=42",
    );

    const fresh = await transport.getSession("session-demo");
    expect(fresh.sequence).toBe(43);
    transport.close();
  });

  it("refreshes catalog changes missed while the socket was closed", async () => {
    const replay = {
      type: "events",
      after: { actorGeneration: 3, sequence: 42 },
      through: { actorGeneration: 3, sequence: 42 },
      events: [],
    };
    const refreshed = structuredClone(hostBootstrapGolden);
    refreshed.catalogCursor = 9;
    const remoteSummary = structuredClone(hostBootstrapGolden.sessions[0]);
    remoteSummary.id = "session-created-remotely";
    remoteSummary.title = "Created while disconnected";
    refreshed.sessions.unshift(remoteSummary);
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(jsonResponse(replay))
      .mockResolvedValueOnce(jsonResponse(refreshed));
    vi.stubGlobal("fetch", fetchMock);
    const transport = new HttpTransport("device-browser");
    const received: unknown[] = [];
    transport.subscribe((event) => received.push(event));
    await transport.connect();

    FakeWebSocket.instances[0]?.emit("open", new Event("open"));

    await vi.waitFor(() =>
      expect(received).toContainEqual(
        expect.objectContaining({
          type: "catalog.summary",
          catalogRevision: 9,
          summary: expect.objectContaining({
            id: "session-created-remotely",
            title: "Created while disconnected",
          }),
        }),
      ),
    );
    expect(fetchMock.mock.calls[2]?.[0]).toBe(
      "/api/v1/bootstrap?selectedSessionId=session-demo",
    );
    transport.close();
  });

  it("retains new-session summary context before loading its snapshot", async () => {
    const createdSnapshot = structuredClone(
      hostBootstrapGolden.selectedSession,
    ) as unknown as Record<string, unknown>;
    createdSnapshot.sessionId = "session-created";
    createdSnapshot.actorGeneration = 4;
    createdSnapshot.cursor = { actorGeneration: 4, sequence: 0 };
    delete createdSnapshot.durableHead;
    createdSnapshot.branches = { entries: [], truncated: false };
    createdSnapshot.items = [];
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(jsonResponse(hostBootstrapGolden))
      .mockResolvedValueOnce(
        jsonResponse({
          protocol: 1,
          hostId: "host-demo",
          commandId: "command-create-live",
          acknowledgedAtMs: 1_721_000_000_060,
          catalogCursor: 8,
          disposition: {
            status: "accepted",
            createdSessionId: "session-created",
          },
        }),
      )
      .mockResolvedValueOnce(jsonResponse(createdSnapshot));
    vi.stubGlobal("fetch", fetchMock);
    const transport = new HttpTransport("device-browser");
    await transport.connect();

    const ack = await transport.send({
      id: "command-create-live",
      type: "session.create",
      projectId: "project-ygg",
      modelId: "gpt-5.6",
      reasoning: "high",
      authority: "fullAccess",
    });
    const snapshot = await transport.getSession(ack.createdSessionId!);

    expect(snapshot).toMatchObject({
      sessionId: "session-created",
      projectId: "project-ygg",
      title: "New session",
      modelId: "gpt-5.6",
      actorGeneration: 4,
    });
    transport.close();
  });

  it("uses relative HTTP routes and derives WebSocket origin from the page", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse(hostBootstrapGolden));
    vi.stubGlobal("fetch", fetchMock);

    const transport = new HttpTransport("device-browser");
    await transport.connect();

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/bootstrap",
      expect.objectContaining({ credentials: "same-origin" }),
    );
    expect(FakeWebSocket.instances[0]?.url).toBe(
      `ws://${window.location.host}/api/v1/events`,
    );
    transport.close();
  });
});
