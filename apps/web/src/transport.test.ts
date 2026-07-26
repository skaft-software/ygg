import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import eventEnvelopeGolden from "../../../extensions/ygg-serve/fixtures/event-envelope.json";
import hostBootstrapGolden from "../../../extensions/ygg-serve/fixtures/host-bootstrap.json";
import {
  createTransport,
  FixtureTransport,
  HttpTransport,
  resolveClientDeviceId,
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
    expect(createTransport()).toBeInstanceOf(HttpTransport);

    window.history.replaceState(null, "", "/?transport=fixture");
    expect(createTransport()).toBeInstanceOf(FixtureTransport);
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
