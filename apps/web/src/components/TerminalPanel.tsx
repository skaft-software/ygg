import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { Terminal, type ITheme } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { Minus, Plus, RotateCcw, SquareTerminal, X } from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
import {
  TerminalWebSocket,
  type TerminalConnectionState,
  type TerminalEvent,
} from "../transport";

const maximumCachedTerminals = 4;
const terminalFontSizeKey = "ygg.ui.terminal.font-size";
const terminalLegacyOwnerKeyPrefix = "ygg.ui.terminal.owner.";
const terminalOwnerKeysPrefix = "ygg.ui.terminal.owners.";
const terminalSelectedOwnerPrefix = "ygg.ui.terminal.selected-owner.";
const terminalMinimumFontSize = 11;
const terminalMaximumFontSize = 20;
const terminalDefaultFontSize = 13;
let nextTerminalAccessOrder = 1;

interface TerminalViewState {
  connection: TerminalConnectionState;
  message?: string;
}

interface CachedTerminal {
  terminal: Terminal;
  fit: FitAddon;
  transport: TerminalWebSocket;
  ownerKey: string;
  renderedSessionId: string | null;
  view: TerminalViewState;
  listeners: Set<(view: TerminalViewState) => void>;
  accessOrder: number;
}

interface TerminalTabs {
  owners: string[];
  activeOwner: string;
}

const terminalCache = new Map<string, CachedTerminal>();
const transientTerminalTabs = new Map<string, TerminalTabs>();

function storageValue(key: string): string | undefined {
  try {
    return window.localStorage.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function persistStorageValue(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // The current page can keep terminal state even when storage is disabled.
  }
}

function terminalFontSize(): number {
  const value = Number(storageValue(terminalFontSizeKey));
  return Number.isInteger(value) &&
    value >= terminalMinimumFontSize &&
    value <= terminalMaximumFontSize
    ? value
    : terminalDefaultFontSize;
}

function validOwnerKey(value: string): boolean {
  return /^[A-Za-z0-9_.-]{1,128}$/u.test(value);
}

function randomOwnerKey(): string {
  if (typeof crypto.randomUUID === "function") {
    return `web-${crypto.randomUUID()}`;
  }
  return `web-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

function terminalHostCacheKey(hostId: string): string {
  return `${window.location.origin}:${hostId}`;
}

function terminalCacheKey(hostId: string, ownerKey: string): string {
  return `${terminalHostCacheKey(hostId)}:${ownerKey}`;
}

function terminalStorageKey(prefix: string, hostCacheKey: string): string {
  return `${prefix}${encodeURIComponent(hostCacheKey)}`;
}

function moveOwnerToMostRecent(owners: readonly string[], ownerKey: string): string[] {
  return [...owners.filter((owner) => owner !== ownerKey), ownerKey];
}

function storedTerminalOwners(hostCacheKey: string): string[] {
  const transient = transientTerminalTabs.get(hostCacheKey);
  if (transient?.owners.length) return [...transient.owners];

  const stored = storageValue(
    terminalStorageKey(terminalOwnerKeysPrefix, hostCacheKey),
  );
  if (stored) {
    try {
      const parsed: unknown = JSON.parse(stored);
      if (Array.isArray(parsed)) {
        const owners = [...new Set(parsed)]
          .filter((owner): owner is string =>
            typeof owner === "string" && validOwnerKey(owner),
          )
          .slice(-maximumCachedTerminals);
        if (owners.length) return owners;
      }
    } catch {
      // Fall through to the previous single-owner preference.
    }
  }

  const legacyOwner = storageValue(
    terminalStorageKey(terminalLegacyOwnerKeyPrefix, hostCacheKey),
  );
  const ownerKey =
    legacyOwner && validOwnerKey(legacyOwner) ? legacyOwner : randomOwnerKey();
  return [ownerKey];
}

function initialTerminalTabs(hostId: string): TerminalTabs {
  const hostCacheKey = terminalHostCacheKey(hostId);
  const owners = storedTerminalOwners(hostCacheKey);
  const selectedOwner = storageValue(
    terminalStorageKey(terminalSelectedOwnerPrefix, hostCacheKey),
  );
  const activeOwner =
    selectedOwner && owners.includes(selectedOwner) ? selectedOwner : owners.at(-1)!;
  const orderedOwners = moveOwnerToMostRecent(owners, activeOwner);
  persistTerminalTabs(hostCacheKey, orderedOwners, activeOwner);
  return { owners: orderedOwners, activeOwner };
}

function persistTerminalTabs(
  hostCacheKey: string,
  owners: readonly string[],
  activeOwner: string,
): void {
  transientTerminalTabs.set(hostCacheKey, {
    owners: [...owners],
    activeOwner,
  });
  persistStorageValue(
    terminalStorageKey(terminalOwnerKeysPrefix, hostCacheKey),
    JSON.stringify(owners),
  );
  persistStorageValue(
    terminalStorageKey(terminalSelectedOwnerPrefix, hostCacheKey),
    activeOwner,
  );
}

function cssColor(name: string, fallback: string): string {
  const color = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return color || fallback;
}

function terminalTheme(): ITheme {
  return {
    background: cssColor("--bg", "#202124"),
    foreground: cssColor("--text", "#ececef"),
    cursor: cssColor("--accent-strong", "#d2d4da"),
    cursorAccent: cssColor("--bg", "#202124"),
    selectionBackground: cssColor("--accent-wash", "rgba(156, 160, 170, 0.28)"),
    black: cssColor("--sidebar", "#191a1d"),
    red: cssColor("--danger", "#ff7078"),
    green: cssColor("--success", "#52c77b"),
    yellow: cssColor("--warning", "#d9a557"),
    blue: cssColor("--focus", "#7aa2f7"),
    magenta: "#c792ea",
    cyan: cssColor("--accent", "#9ca0aa"),
    white: cssColor("--text", "#ececef"),
    brightBlack: cssColor("--text-faint", "#777a82"),
    brightRed: cssColor("--danger", "#ff7078"),
    brightGreen: cssColor("--success", "#52c77b"),
    brightYellow: cssColor("--warning", "#d9a557"),
    brightBlue: cssColor("--focus", "#7aa2f7"),
    brightMagenta: "#d8b4fe",
    brightCyan: cssColor("--accent-strong", "#d2d4da"),
    brightWhite: "#ffffff",
  };
}

function applyTerminalAppearance(entry: CachedTerminal): void {
  entry.terminal.options.theme = terminalTheme();
  entry.terminal.options.fontFamily = cssColor(
    "--mono-family",
    "ui-monospace, monospace",
  );
}

function openTerminalLink(event: MouseEvent, uri: string): void {
  event.preventDefault();
  try {
    const link = new URL(uri);
    if (link.protocol === "http:" || link.protocol === "https:") {
      const anchor = document.createElement("a");
      anchor.href = link.toString();
      anchor.target = "_blank";
      anchor.rel = "noopener noreferrer";
      anchor.click();
    }
  } catch {
    // The link addon only recognizes web URLs; ignore malformed output safely.
  }
}

function touchCachedTerminal(entry: CachedTerminal): void {
  entry.accessOrder = nextTerminalAccessOrder;
  nextTerminalAccessOrder += 1;
}

function disposeCachedTerminalEntry(entry: CachedTerminal): void {
  entry.transport.dispose();
  entry.terminal.dispose();
}

function evictCachedTerminalIfNeeded(): void {
  if (terminalCache.size < maximumCachedTerminals) return;
  const candidate = [...terminalCache.entries()]
    .filter(([, entry]) => entry.listeners.size === 0)
    .sort(([, left], [, right]) => left.accessOrder - right.accessOrder)[0];
  if (!candidate) return;
  const [cacheKey, entry] = candidate;
  terminalCache.delete(cacheKey);
  disposeCachedTerminalEntry(entry);
}

function disposeCachedTerminal(hostId: string, ownerKey: string): void {
  const cacheKey = terminalCacheKey(hostId, ownerKey);
  const entry = terminalCache.get(cacheKey);
  if (!entry) return;
  terminalCache.delete(cacheKey);
  disposeCachedTerminalEntry(entry);
}

function notify(entry: CachedTerminal): void {
  for (const listener of entry.listeners) listener(entry.view);
}

function consumeTerminalEvent(entry: CachedTerminal, event: TerminalEvent): void {
  if (event.type === "state") {
    entry.view = { connection: event.state };
  } else if (event.type === "opened") {
    entry.terminal.reset();
    if (event.replay) entry.terminal.write(event.replay);
    entry.renderedSessionId = event.id;
    entry.ownerKey = event.ownerKey;
    entry.view = { connection: "connected" };
  } else if (event.type === "output") {
    if (entry.renderedSessionId === event.id) entry.terminal.write(event.data);
  } else if (event.type === "exit") {
    if (entry.renderedSessionId !== event.id) return;
    entry.renderedSessionId = null;
    entry.view = {
      connection: "exited",
      message:
        event.signal
          ? `Shell exited (${event.signal}).`
          : `Shell exited with code ${event.exitCode}.`,
    };
  } else {
    entry.view = { ...entry.view, message: event.message };
  }
  notify(entry);
}

function createCachedTerminal(ownerKey: string): CachedTerminal {
  const terminal = new Terminal({
    allowProposedApi: false,
    convertEol: true,
    cursorBlink: true,
    cursorStyle: "bar",
    fontFamily: cssColor("--mono-family", "ui-monospace, monospace"),
    lineHeight: 1.2,
    linkHandler: {
      activate: openTerminalLink,
      allowNonHttpProtocols: false,
    },
    macOptionIsMeta: true,
    rightClickSelectsWord: true,
    scrollback: 5_000,
    screenReaderMode: true,
    theme: terminalTheme(),
  });
  terminal.options.fontSize = terminalFontSize();
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.loadAddon(new WebLinksAddon(openTerminalLink));
  const entry: CachedTerminal = {
    terminal,
    fit,
    transport: new TerminalWebSocket(),
    ownerKey,
    renderedSessionId: null,
    view: { connection: "detached" },
    listeners: new Set(),
    accessOrder: 0,
  };
  touchCachedTerminal(entry);
  terminal.onData((data) => entry.transport.input(data));
  entry.transport.subscribe((event) => consumeTerminalEvent(entry, event));
  return entry;
}

function cachedTerminal(hostId: string, ownerKey: string): CachedTerminal {
  const cacheKey = terminalCacheKey(hostId, ownerKey);
  const existing = terminalCache.get(cacheKey);
  if (existing) {
    touchCachedTerminal(existing);
    return existing;
  }
  evictCachedTerminalIfNeeded();
  const created = createCachedTerminal(ownerKey);
  terminalCache.set(cacheKey, created);
  return created;
}

function fitTerminal(
  entry: CachedTerminal,
  container: HTMLDivElement | null,
): void {
  if (!container || container.clientWidth < 20 || container.clientHeight < 20) {
    return;
  }
  try {
    entry.fit.fit();
    entry.transport.resize(entry.terminal.cols, entry.terminal.rows);
  } catch {
    // Layout can briefly report zero dimensions while the split pane changes.
  }
}

function connectionLabel(view: TerminalViewState): string {
  if (view.message) return view.message;
  switch (view.connection) {
    case "connected":
      return "Connected";
    case "connecting":
      return "Starting shell…";
    case "reconnecting":
      return "Reconnecting…";
    case "exited":
      return "Shell exited";
    case "detached":
      return "Paused";
  }
}

function setCachedTerminalFontSize(entry: CachedTerminal, size: number): void {
  entry.terminal.options.fontSize = size;
}

function restartCachedTerminal(entry: CachedTerminal): void {
  entry.renderedSessionId = null;
  entry.terminal.reset();
  entry.transport.open({
    cols: entry.terminal.cols,
    rows: entry.terminal.rows,
    ownerKey: entry.ownerKey,
  });
}

function useCachedTerminal(
  hostId: string,
  ownerKey: string,
): [
  CachedTerminal,
  TerminalViewState,
  MutableRefObject<HTMLDivElement | null>,
] {
  const [entry] = useState<CachedTerminal>(() => cachedTerminal(hostId, ownerKey));
  const containerRef = useRef<HTMLDivElement>(null);
  const [view, setView] = useState<TerminalViewState>(() => entry.view);

  useEffect(() => {
    touchCachedTerminal(entry);
    const listener = (next: TerminalViewState) => setView(next);
    entry.listeners.add(listener);
    return () => {
      entry.listeners.delete(listener);
    };
  }, [entry]);

  return [entry, view, containerRef];
}

function TerminalViewport({
  hostId,
  ownerKey,
  owners,
  onClose,
  onNewTerminal,
  onSelectTerminal,
}: {
  hostId: string;
  ownerKey: string;
  owners: readonly string[];
  onClose: () => void;
  onNewTerminal: () => void;
  onSelectTerminal: (ownerKey: string) => void;
}) {
  const [entry, view, containerRef] = useCachedTerminal(hostId, ownerKey);
  const [fontSize, setFontSize] = useState<number>(
    () => entry.terminal.options.fontSize ?? terminalDefaultFontSize,
  );

  const resize = useCallback(() => {
    fitTerminal(entry, containerRef.current);
  }, [containerRef, entry]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    if (entry.terminal.element) {
      container.append(entry.terminal.element);
    } else {
      entry.terminal.open(container);
    }
    applyTerminalAppearance(entry);
    const frame = window.requestAnimationFrame(() => {
      resize();
      entry.transport.open({
        cols: entry.terminal.cols,
        rows: entry.terminal.rows,
        ownerKey: entry.ownerKey,
      });
      entry.terminal.focus();
    });
    const observer =
      typeof ResizeObserver === "undefined"
        ? undefined
        : new ResizeObserver(() => resize());
    observer?.observe(container);
    window.addEventListener("resize", resize);
    const rootObserver = new MutationObserver(() => {
      applyTerminalAppearance(entry);
      resize();
    });
    rootObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class", "style", "data-color-scheme", "data-theme"],
    });
    return () => {
      window.cancelAnimationFrame(frame);
      observer?.disconnect();
      rootObserver.disconnect();
      window.removeEventListener("resize", resize);
      entry.transport.detach();
    };
  }, [containerRef, entry, resize]);

  const changeFontSize = useCallback(
    (next: number) => {
      const bounded = Math.max(
        terminalMinimumFontSize,
        Math.min(terminalMaximumFontSize, next),
      );
      setCachedTerminalFontSize(entry, bounded);
      persistStorageValue(terminalFontSizeKey, String(bounded));
      setFontSize(bounded);
      window.requestAnimationFrame(resize);
    },
    [entry, resize],
  );

  useEffect(() => {
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if ((!event.ctrlKey && !event.metaKey) || event.altKey) return;
      let nextFontSize: number | undefined;
      if (event.key === "+" || event.key === "=") {
        nextFontSize = fontSize + 1;
      } else if (event.key === "-") {
        nextFontSize = fontSize - 1;
      } else if (event.key === "0") {
        nextFontSize = terminalDefaultFontSize;
      }
      if (nextFontSize === undefined) return;
      event.preventDefault();
      changeFontSize(nextFontSize);
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [changeFontSize, fontSize]);

  const restart = useCallback(() => {
    restartCachedTerminal(entry);
  }, [entry]);

  return (
    <>
      <header className="terminal-panel-header">
        <div className="terminal-panel-title">
          <SquareTerminal aria-hidden="true" />
          <strong>Terminal</strong>
          <div className="terminal-tabs" role="tablist" aria-label="Terminal sessions">
            {owners.map((owner, index) => (
              <button
                className={`terminal-tab ${owner === ownerKey ? "is-active" : ""}`}
                key={owner}
                role="tab"
                aria-selected={owner === ownerKey}
                onClick={() => onSelectTerminal(owner)}
              >
                Shell {index + 1}
              </button>
            ))}
            <button
              className="terminal-new-button"
              onClick={onNewTerminal}
              aria-label="New terminal"
              title="New terminal"
            >
              <Plus aria-hidden="true" />
            </button>
          </div>
          <span
            className={`terminal-connection is-${view.connection}`}
            role="status"
          >
            {connectionLabel(view)}
          </span>
        </div>
        <div className="terminal-panel-actions">
          {view.connection === "exited" ? (
            <button
              className="terminal-toolbar-button"
              onClick={restart}
              aria-label="Start a new shell"
              title="Start a new shell"
            >
              <RotateCcw aria-hidden="true" />
            </button>
          ) : null}
          <button
            className="terminal-toolbar-button"
            onClick={() => changeFontSize(fontSize - 1)}
            disabled={fontSize <= terminalMinimumFontSize}
            aria-label="Decrease terminal font size"
            title="Decrease font size (Ctrl+-)"
          >
            <Minus aria-hidden="true" />
          </button>
          <span className="terminal-font-size" aria-label={`Font size ${fontSize}`}>
            {fontSize}
          </span>
          <button
            className="terminal-toolbar-button"
            onClick={() => changeFontSize(fontSize + 1)}
            disabled={fontSize >= terminalMaximumFontSize}
            aria-label="Increase terminal font size"
            title="Increase font size (Ctrl++)"
          >
            <Plus aria-hidden="true" />
          </button>
          <button
            className="terminal-toolbar-button"
            onClick={onClose}
            aria-label="Close terminal"
            title="Close terminal"
          >
            <X aria-hidden="true" />
          </button>
        </div>
      </header>
      <div
        className="terminal-panel-body"
        role="tabpanel"
        aria-label="Terminal output"
        ref={containerRef}
        onMouseDown={() => entry.terminal.focus()}
      />
    </>
  );
}

export function TerminalPanel({
  hostId,
  onClose,
}: {
  hostId: string;
  onClose: () => void;
}) {
  const hostCacheKey = terminalHostCacheKey(hostId);
  const [tabs, setTabs] = useState<TerminalTabs>(() => initialTerminalTabs(hostId));

  const selectTerminal = useCallback(
    (ownerKey: string) => {
      if (!tabs.owners.includes(ownerKey)) return;
      const owners = moveOwnerToMostRecent(tabs.owners, ownerKey);
      persistTerminalTabs(hostCacheKey, owners, ownerKey);
      setTabs({ owners, activeOwner: ownerKey });
    },
    [hostCacheKey, tabs],
  );

  const createTerminal = useCallback(() => {
    const ownerKey = randomOwnerKey();
    const evictedOwner =
      tabs.owners.length === maximumCachedTerminals ? tabs.owners[0] : undefined;
    const owners = [
      ...(evictedOwner ? tabs.owners.slice(1) : tabs.owners),
      ownerKey,
    ];
    if (evictedOwner) disposeCachedTerminal(hostId, evictedOwner);
    persistTerminalTabs(hostCacheKey, owners, ownerKey);
    setTabs({ owners, activeOwner: ownerKey });
  }, [hostCacheKey, hostId, tabs]);

  return (
    <section className="terminal-panel" aria-label="Terminal">
      <TerminalViewport
        key={tabs.activeOwner}
        hostId={hostId}
        ownerKey={tabs.activeOwner}
        owners={tabs.owners}
        onClose={onClose}
        onNewTerminal={createTerminal}
        onSelectTerminal={selectTerminal}
      />
    </section>
  );
}

// The app root disposes cached terminal resources when it unmounts.
// eslint-disable-next-line react-refresh/only-export-components
export function disposeTerminalCache(): void {
  for (const entry of terminalCache.values()) {
    disposeCachedTerminalEntry(entry);
  }
  terminalCache.clear();
}
