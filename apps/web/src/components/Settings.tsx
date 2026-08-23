import { Bell, BellOff, ShieldCheck, Type } from "lucide-react";
import { useEffect, useState } from "react";

function readNativeSettingsUrl(): string | null {
  const value = document
    .querySelector<HTMLMetaElement>('meta[name="ygg-native-settings-url"]')
    ?.content.trim();
  if (!value) return null;
  try {
    const url = new URL(value);
    if (
      url.protocol !== "http:" ||
      url.hostname !== "127.0.0.1" ||
      !url.port ||
      url.username ||
      url.password ||
      url.search ||
      url.hash ||
      !/^\/_native\/settings\/start\/[0-9a-f]{64}$/.test(url.pathname)
    ) {
      return null;
    }
    return url.href;
  } catch {
    return null;
  }
}

interface SettingsViewProps {
  notificationsSupported: boolean;
  notificationsEnabled: boolean;
  notificationPermission: NotificationPermission | "unsupported";
  onNotificationsChange: (enabled: boolean) => Promise<boolean>;
}

const fontStacks = [
  {
    id: "local",
    label: "Local Grotesk + Mono (default)",
    ui: '"Local Grotesk", ui-sans-serif, system-ui, -apple-system, sans-serif',
    mono: '"Local Mono", "LocalMono Nerd Font Mono", "SFMono-Regular", "Cascadia Mono", Menlo, Consolas, ui-monospace, monospace',
  },
  {
    id: "geist",
    label: "Geist Sans + Geist Mono",
    ui: '"Geist", Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"Geist Mono", "SFMono-Regular", Menlo, Consolas, ui-monospace, monospace',
  },
  {
    id: "ibm-plex",
    label: "IBM Plex Sans + Mono",
    ui: '"IBM Plex Sans", Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"IBM Plex Mono", "IBM Plex Mono Text", ui-monospace, monospace',
  },
  {
    id: "jetbrains-nerd",
    label: "System Sans + JetBrains Mono",
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"JetBrainsMono Nerd Font", "JetBrains Mono", ui-monospace, monospace',
  },
  {
    id: "iosevka-nerd",
    label: "System Sans + Iosevka",
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"Iosevka Nerd Font", Iosevka, ui-monospace, monospace',
  },
  {
    id: "firacode-nerd",
    label: "System Sans + Fira Code",
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"FiraCode Nerd Font", "Fira Code", ui-monospace, monospace',
  },
  {
    id: "system-mono",
    label: "Native UI + System Mono",
    ui: '-apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    mono: '"SFMono-Regular", "Cascadia Mono", Menlo, Consolas, ui-monospace, monospace',
  },
] as const;

const uiSizes = [12, 13, 14, 15] as const;

export function SettingsView({
  notificationsSupported,
  notificationsEnabled,
  notificationPermission,
  onNotificationsChange,
}: SettingsViewProps) {
  const [fontStackId, setFontStackId] = useState(() => {
    const stored = localStorage.getItem("ygg.ui.font");
    if (stored === "ibm-plex-mono") return "geist";
    return stored !== null && fontStacks.some((stack) => stack.id === stored)
      ? stored
      : "local";
  });
  const [uiSize, setUiSize] = useState(
    () => Number(localStorage.getItem("ygg.ui.size") ?? "14"),
  );
  const [notificationPending, setNotificationPending] = useState(false);
  const [notificationMessage, setNotificationMessage] = useState<string | null>(
    null,
  );
  const [nativeSettingsUrl] = useState(readNativeSettingsUrl);

  useEffect(() => {
    const stack =
      fontStacks.find((candidate) => candidate.id === fontStackId) ??
      fontStacks[0];
    document.documentElement.style.setProperty("--ui-family", stack.ui);
    document.documentElement.style.setProperty("--mono-family", stack.mono);
    localStorage.setItem("ygg.ui.font", stack.id);
  }, [fontStackId]);

  useEffect(() => {
    const normalized = uiSizes.includes(uiSize as (typeof uiSizes)[number])
      ? uiSize
      : 14;
    const root = document.documentElement;
    root.style.setProperty("--font-body", `${normalized}px`);
    root.style.setProperty("--font-meta", `${Math.max(11, normalized - 2)}px`);
    localStorage.setItem("ygg.ui.size", String(normalized));
  }, [uiSize]);

  return (
    <main className="utility-view" aria-labelledby="settings-title">
      <header className="utility-header">
        <span>ygg preferences</span>
        <h1 id="settings-title">Settings</h1>
        <p>
          Preferences live on this device and never require a ygg account.
        </p>
      </header>

      <section className="settings-section" aria-labelledby="type-title">
        <div className="settings-section-heading">
          <Type aria-hidden="true" />
          <div>
            <h2 id="type-title">Interface type</h2>
            <p>
              Local ships with ygg for legible, playful DIY work; alternatives
              use fonts installed on this device.
            </p>
          </div>
        </div>
        <div className="preference-fields">
          <label>
            <span>Font pairing</span>
            <select
              value={fontStackId}
              onChange={(event) => setFontStackId(event.target.value)}
            >
              {fontStacks.map((stack) => (
                <option key={stack.id} value={stack.id}>
                  {stack.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            <span>UI size</span>
            <select
              value={uiSize}
              onChange={(event) => setUiSize(Number(event.target.value))}
            >
              {uiSizes.map((size) => (
                <option key={size} value={size}>
                  {size}px
                </option>
              ))}
            </select>
          </label>
        </div>
      </section>

      <section
        className="settings-section"
        aria-labelledby="notifications-title"
      >
        <div className="settings-section-heading">
          <Bell aria-hidden="true" />
          <div>
            <h2 id="notifications-title">Background attention</h2>
            <p>
              Opt in to device notifications when a background task finishes,
              fails, or needs your input.
            </p>
          </div>
        </div>
        <div className="settings-rows">
          <button
            type="button"
            className="settings-toggle-row"
            role="switch"
            aria-checked={notificationsEnabled}
            disabled={
              !notificationsSupported ||
              notificationPermission === "denied" ||
              notificationPending
            }
            onClick={() => {
              setNotificationPending(true);
              setNotificationMessage(null);
              void onNotificationsChange(!notificationsEnabled)
                .then((enabled) => {
                  if (!enabled && !notificationsEnabled) {
                    setNotificationMessage(
                      notificationPermission === "denied"
                        ? "Notifications are blocked in browser settings."
                        : "Notification permission was not granted.",
                    );
                  }
                })
                .finally(() => setNotificationPending(false));
            }}
          >
            <span>
              <strong>
                {notificationsEnabled
                  ? "Notifications enabled"
                  : "Notify me about background work"}
              </strong>
              <small>
                {!notificationsSupported
                  ? "Notifications are unavailable in this browser."
                  : notificationPermission === "denied"
                    ? "Blocked by browser settings."
                    : "Task titles only; no prompt or tool content is included."}
              </small>
            </span>
            {notificationsEnabled ? (
              <Bell aria-hidden="true" />
            ) : (
              <BellOff aria-hidden="true" />
            )}
          </button>
          {notificationMessage ? (
            <p className="settings-inline-message" role="status">
              {notificationMessage}
            </p>
          ) : null}
        </div>
      </section>

      {nativeSettingsUrl ? (
        <section
          className="settings-section"
          aria-labelledby="companion-settings-title"
        >
          <div className="settings-section-heading">
            <ShieldCheck aria-hidden="true" />
            <div>
              <h2 id="companion-settings-title">Companion access</h2>
              <p>
                Open the app-owned settings origin to remove this device's
                protected endpoint identity and pinned host.
              </p>
            </div>
          </div>
          <div className="settings-rows">
            <a className="settings-toggle-row" href={nativeSettingsUrl}>
              <span>
                <strong>Open native companion settings</strong>
                <small>
                  Host-provided web content cannot remove active access.
                </small>
              </span>
              <ShieldCheck aria-hidden="true" />
            </a>
          </div>
        </section>
      ) : null}
    </main>
  );
}
