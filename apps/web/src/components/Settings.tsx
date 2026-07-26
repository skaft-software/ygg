import {
  Check,
  ChevronRight,
  FolderClock,
  Palette,
  ShieldCheck,
  SlidersHorizontal,
  Type,
} from "lucide-react";
import { type CSSProperties, useEffect, useState } from "react";
import type { ThemeOption } from "../protocol";
import { themeRoleColorToCss } from "../theme";

interface SettingsViewProps {
  themes: ThemeOption[];
  selectedThemeId: string;
  selectionAvailable: boolean;
  onThemeChange: (themeId: string) => void;
}

const fontStacks = [
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

function themeDescription(option: ThemeOption): string {
  return `Follows system appearance · ${option.theme.source}`;
}

export function SettingsView({
  themes,
  selectedThemeId,
  selectionAvailable,
  onThemeChange,
}: SettingsViewProps) {
  const [fontStackId, setFontStackId] = useState(() => {
    const stored = localStorage.getItem("ygg.ui.font");
    return !stored || stored === "ibm-plex-mono" ? "geist" : stored;
  });
  const [uiSize, setUiSize] = useState(
    () => Number(localStorage.getItem("ygg.ui.size") ?? "13"),
  );

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
      : 13;
    const root = document.documentElement;
    root.style.setProperty("--font-body", `${normalized}px`);
    root.style.setProperty("--font-meta", `${Math.max(11, normalized - 1)}px`);
    root.style.setProperty("--font-chat", `${normalized + 1}px`);
    root.style.setProperty("--font-prompt", `${normalized + 3}px`);
    root.style.setProperty("--font-display", `${normalized + 3}px`);
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

      <section className="settings-section" aria-labelledby="appearance-title">
        <div className="settings-section-heading">
          <Palette aria-hidden="true" />
          <div>
            <h2 id="appearance-title">Appearance</h2>
            <p>
              {selectionAvailable
                ? "The installed ygg catalog recolors the same stable interface."
                : "This theme is selected by the connected ygg host."}
            </p>
          </div>
        </div>
        <div className="theme-options">
          {themes
            .filter(
              (option) =>
                selectionAvailable || selectedThemeId === option.id,
            )
            .map((option) => {
            const selected = selectedThemeId === option.id;
            const pigment = themeRoleColorToCss(
              option.theme,
              ["accent", "link"],
              "#168f91",
              "accent",
            );
            return (
              <button
                key={option.id}
                className={selected ? "is-selected" : ""}
                onClick={
                  selectionAvailable
                    ? () => onThemeChange(option.id)
                    : undefined
                }
                disabled={!selectionAvailable}
                aria-pressed={selected}
              >
                <span
                  className="theme-swatch"
                  style={{ "--swatch": pigment } as CSSProperties}
                  aria-hidden="true"
                />
                <span>
                  <strong>{option.theme.name}</strong>
                  <small>{themeDescription(option)}</small>
                </span>
                {selected ? (
                  <Check aria-hidden="true" />
                ) : selectionAvailable ? (
                  <ChevronRight aria-hidden="true" />
                ) : null}
              </button>
            );
          })}
        </div>
      </section>

      <section className="settings-section" aria-labelledby="type-title">
        <div className="settings-section-heading">
          <Type aria-hidden="true" />
          <div>
            <h2 id="type-title">Interface type</h2>
            <p>Pair readable conversation type with precise technical text.</p>
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

      <section className="settings-section" aria-labelledby="defaults-title">
        <div className="settings-section-heading">
          <SlidersHorizontal aria-hidden="true" />
          <div>
            <h2 id="defaults-title">Session defaults</h2>
            <p>New sessions begin ready for local work.</p>
          </div>
        </div>
        <div className="settings-rows">
          <div className="settings-static-row">
            <span>
              <strong>Default authority</strong>
              <small>Full access</small>
            </span>
            <ShieldCheck aria-hidden="true" />
          </div>
          <div className="settings-static-row">
            <span>
              <strong>Default project</strong>
              <small>Use the last active folder</small>
            </span>
            <FolderClock aria-hidden="true" />
          </div>
        </div>
      </section>
    </main>
  );
}
