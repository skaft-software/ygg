import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TerminalWebSocket } from "./transport";

class FakeTerminalWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeTerminalWebSocket[] = [];

  readonly sent: string[] = [];
  readyState = FakeTerminalWebSocket.CONNECTING;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  constructor(readonly url: string) {
    FakeTerminalWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  open(): void {
    this.readyState = FakeTerminalWebSocket.OPEN;
    this.onopen?.(new Event("open"));
  }

  message(value: unknown): void {
    this.onmessage?.(
      new MessageEvent("message", { data: JSON.stringify(value) }),
    );
  }

  close(): void {
    if (this.readyState === FakeTerminalWebSocket.CLOSED) return;
    this.readyState = FakeTerminalWebSocket.CLOSED;
    this.onclose?.(new CloseEvent("close"));
  }
}

function sent(socket: FakeTerminalWebSocket): unknown[] {
  return socket.sent.map((message) => JSON.parse(message));
}

describe("terminal WebSocket transport", () => {
  beforeEach(() => {
    FakeTerminalWebSocket.instances = [];
    vi.stubGlobal(
      "WebSocket",
      FakeTerminalWebSocket as unknown as typeof WebSocket,
    );
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("opens, forwards terminal I/O, and detaches without replacing the shell", () => {
    const terminal = new TerminalWebSocket();
    const events: string[] = [];
    terminal.subscribe((event) => events.push(event.type));

    terminal.open({ cols: 80, rows: 24, ownerKey: "owner-initial" });
    const socket = FakeTerminalWebSocket.instances[0];
    expect(socket?.url).toBe(
      `ws://${window.location.host}/api/v1/terminal`,
    );
    socket?.open();
    expect(socket && sent(socket)).toEqual([
      {
        type: "open",
        cols: 80,
        rows: 24,
        ownerKey: "owner-initial",
      },
    ]);

    socket?.message({
      type: "opened",
      id: "terminal-1",
      ownerKey: "owner-returned",
      replay: "ready\r\n",
    });
    terminal.resize(120, 40);
    terminal.input("echo hello\n");
    terminal.detach();

    expect(socket && sent(socket)).toEqual([
      {
        type: "open",
        cols: 80,
        rows: 24,
        ownerKey: "owner-initial",
      },
      { type: "resize", id: "terminal-1", cols: 120, rows: 40 },
      { type: "input", id: "terminal-1", data: "echo hello\n" },
      { type: "detach", id: "terminal-1" },
    ]);
    expect(events).toEqual(["state", "state", "state", "opened", "state"]);
  });

  it("reopens with the server-provided owner key after a connection loss", () => {
    vi.useFakeTimers();
    const terminal = new TerminalWebSocket();

    terminal.open({ cols: 80, rows: 24, ownerKey: "owner-initial" });
    const first = FakeTerminalWebSocket.instances[0];
    first?.open();
    first?.message({
      type: "opened",
      id: "terminal-1",
      ownerKey: "owner-returned",
    });
    first?.close();

    vi.advanceTimersByTime(250);
    const second = FakeTerminalWebSocket.instances[1];
    expect(second).toBeDefined();
    second?.open();
    expect(second && sent(second)).toEqual([
      {
        type: "open",
        cols: 80,
        rows: 24,
        ownerKey: "owner-returned",
      },
    ]);

    terminal.dispose();
  });

  it("invalidates the prior terminal identity when explicitly reopening", () => {
    vi.useFakeTimers();
    const terminal = new TerminalWebSocket();
    const events: string[] = [];
    terminal.subscribe((event) => events.push(event.type));

    terminal.open({ cols: 80, rows: 24, ownerKey: "owner-initial" });
    const first = FakeTerminalWebSocket.instances[0];
    first?.open();
    first?.message({
      type: "opened",
      id: "terminal-old",
      ownerKey: "owner-returned",
    });

    terminal.open({ cols: 100, rows: 30, ownerKey: "owner-returned" });
    const second = FakeTerminalWebSocket.instances[1];
    second?.open();
    terminal.input("must not target the prior terminal");
    second?.message({ type: "exit", id: "terminal-old", exitCode: 0 });

    expect(second && sent(second)).toEqual([
      {
        type: "open",
        cols: 100,
        rows: 30,
        ownerKey: "owner-returned",
      },
    ]);
    expect(events).not.toContain("exit");

    second?.close();
    vi.advanceTimersByTime(250);
    expect(FakeTerminalWebSocket.instances).toHaveLength(3);
    terminal.dispose();
  });

  it("does not reconnect after the shell exits", () => {
    vi.useFakeTimers();
    const terminal = new TerminalWebSocket();

    terminal.open({ cols: 80, rows: 24 });
    const socket = FakeTerminalWebSocket.instances[0];
    socket?.open();
    socket?.message({
      type: "opened",
      id: "terminal-1",
      ownerKey: "owner-returned",
    });
    socket?.message({ type: "exit", id: "terminal-1", exitCode: 0 });
    socket?.close();
    vi.advanceTimersByTime(8_000);

    expect(FakeTerminalWebSocket.instances).toHaveLength(1);
    terminal.dispose();
  });
});
