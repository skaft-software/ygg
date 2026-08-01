import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const xtermMocks = vi.hoisted(() => {
  class FakeTerminal {
    static instances: FakeTerminal[] = [];

    cols = 80;
    rows = 24;
    element: HTMLDivElement | undefined;
    options: { fontSize?: number; [key: string]: unknown };
    readonly reset = vi.fn();
    readonly write = vi.fn();
    readonly focus = vi.fn();
    readonly dispose = vi.fn();

    constructor(options: { fontSize?: number }) {
      this.options = { ...options };
      FakeTerminal.instances.push(this);
    }

    loadAddon(): void {}

    onData(listener: (data: string) => void): { dispose: () => void } {
      void listener;
      return { dispose: () => undefined };
    }

    open(container: HTMLElement): void {
      this.element = document.createElement("div");
      this.element.className = "xterm";
      container.append(this.element);
    }
  }

  class FakeFitAddon {
    readonly fit = vi.fn();
  }

  class FakeWebLinksAddon {
    constructor(openLink: (event: MouseEvent, uri: string) => void) {
      void openLink;
    }
  }

  return { FakeFitAddon, FakeTerminal, FakeWebLinksAddon };
});

vi.mock("@xterm/xterm", () => ({ Terminal: xtermMocks.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: xtermMocks.FakeFitAddon }));
vi.mock("@xterm/addon-web-links", () => ({
  WebLinksAddon: xtermMocks.FakeWebLinksAddon,
}));

import { TerminalPanel, disposeTerminalCache } from "./TerminalPanel";

class FakeTerminalWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeTerminalWebSocket[] = [];

  readonly sent: string[] = [];
  readyState = FakeTerminalWebSocket.CONNECTING;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  constructor(url: string) {
    void url;
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

function messages(socket: FakeTerminalWebSocket): unknown[] {
  return socket.sent.map((message) => JSON.parse(message));
}

describe("TerminalPanel", () => {
  beforeEach(() => {
    FakeTerminalWebSocket.instances = [];
    xtermMocks.FakeTerminal.instances = [];
    vi.stubGlobal(
      "WebSocket",
      FakeTerminalWebSocket as unknown as typeof WebSocket,
    );
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      callback(0);
      return 1;
    });
    vi.stubGlobal("cancelAnimationFrame", () => undefined);
    try {
      window.localStorage.clear();
    } catch {
      // jsdom may intentionally omit local storage in isolated workers.
    }
  });

  afterEach(() => {
    cleanup();
    disposeTerminalCache();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("replays and reattaches the cached shell after its pane remounts", async () => {
    const firstView = render(<TerminalPanel hostId="host-terminal" onClose={vi.fn()} />);
    await waitFor(() => expect(FakeTerminalWebSocket.instances).toHaveLength(1));
    const firstSocket = FakeTerminalWebSocket.instances[0];
    const terminal = xtermMocks.FakeTerminal.instances[0];
    expect(terminal).toBeDefined();

    act(() => firstSocket?.open());
    const initialOpen = messages(firstSocket as FakeTerminalWebSocket)[0] as {
      ownerKey: string;
    };
    expect(initialOpen).toMatchObject({ type: "open", cols: 80, rows: 24 });

    act(() => {
      firstSocket?.message({
        type: "opened",
        id: "terminal-1",
        ownerKey: "owner-returned",
        replay: "first replay\r\n",
      });
    });
    expect(terminal?.reset).toHaveBeenCalledOnce();
    expect(terminal?.write).toHaveBeenCalledWith("first replay\r\n");

    firstView.unmount();
    expect(messages(firstSocket as FakeTerminalWebSocket)).toContainEqual({
      type: "detach",
      id: "terminal-1",
    });

    render(<TerminalPanel hostId="host-terminal" onClose={vi.fn()} />);
    await waitFor(() => expect(FakeTerminalWebSocket.instances).toHaveLength(2));
    const secondSocket = FakeTerminalWebSocket.instances[1];
    act(() => secondSocket?.open());
    expect(messages(secondSocket as FakeTerminalWebSocket)[0]).toEqual({
      type: "open",
      cols: 80,
      rows: 24,
      ownerKey: "owner-returned",
    });

    act(() => {
      secondSocket?.message({
        type: "opened",
        id: "terminal-1",
        ownerKey: "owner-returned",
        replay: "reconnected replay\r\n",
      });
    });
    expect(terminal?.reset).toHaveBeenCalledTimes(2);
    expect(terminal?.write).toHaveBeenLastCalledWith("reconnected replay\r\n");
  });

  it("routes terminal links through an audited noopener anchor", async () => {
    render(<TerminalPanel hostId="host-links" onClose={vi.fn()} />);
    await waitFor(() => expect(xtermMocks.FakeTerminal.instances).toHaveLength(1));
    const options = xtermMocks.FakeTerminal.instances[0]?.options;
    const linkHandler = options?.linkHandler as
      | {
          activate: (event: MouseEvent, uri: string) => void;
          allowNonHttpProtocols: boolean;
        }
      | undefined;
    expect(linkHandler?.allowNonHttpProtocols).toBe(false);

    const click = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(() => undefined);
    const preventDefault = vi.fn();
    linkHandler?.activate(
      { preventDefault } as unknown as MouseEvent,
      "https://example.test/output",
    );

    expect(preventDefault).toHaveBeenCalledOnce();
    expect(click).toHaveBeenCalledOnce();
    const clicked = click.mock.instances[0] as HTMLAnchorElement;
    expect(clicked.href).toBe("https://example.test/output");
    expect(clicked.target).toBe("_blank");
    expect(clicked.rel).toBe("noopener noreferrer");

    linkHandler?.activate(
      { preventDefault } as unknown as MouseEvent,
      "javascript:alert(1)",
    );
    expect(click).toHaveBeenCalledOnce();
  });

  it("keeps four terminal tabs and supports font-size shortcuts", async () => {
    render(<TerminalPanel hostId="host-tabs" onClose={vi.fn()} />);
    await waitFor(() => expect(FakeTerminalWebSocket.instances).toHaveLength(1));

    fireEvent.keyDown(document, { ctrlKey: true, key: "+" });
    expect(screen.getByLabelText("Font size 14")).toBeInTheDocument();
    fireEvent.keyDown(document, { ctrlKey: true, key: "0" });
    expect(screen.getByLabelText("Font size 13")).toBeInTheDocument();

    for (let index = 0; index < 4; index += 1) {
      fireEvent.click(screen.getByRole("button", { name: "New terminal" }));
    }

    await waitFor(() => expect(FakeTerminalWebSocket.instances).toHaveLength(5));
    expect(screen.getAllByRole("tab")).toHaveLength(4);
    expect(xtermMocks.FakeTerminal.instances[0]?.dispose).toHaveBeenCalledOnce();
  });
});
