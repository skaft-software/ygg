import {
  Check,
  ChevronRight,
  FolderClock,
  Palette,
  ShieldCheck,
  SlidersHorizontal,
} from "lucide-react";
import type { CSSProperties } from "react";
import type { ThemeOption } from "../protocol";
import { themeColorToCss } from "../theme";

interface SettingsViewProps {
  themes: ThemeOption[];
  selectedThemeId: string;
  selectionAvailable: boolean;
  onThemeChange: (themeId: string) => void;
}

function themeDescription(option: ThemeOption): string {
  const { scheme, density, source } = option.theme;
  const palette = scheme === "unknown" ? "Adaptive" : scheme;
  return `${palette} · ${density} · ${source}`;
}

export function SettingsView({
  themes,
  selectedThemeId,
  selectionAvailable,
  onThemeChange,
}: SettingsViewProps) {
  return (
    <main className="utility-view" aria-labelledby="settings-title">
      <header className="utility-header">
        <span>Ygg preferences</span>
        <h1 id="settings-title">Settings</h1>
        <p>
          Preferences live on this device and never require a Ygg account.
        </p>
      </header>

      <section className="settings-section" aria-labelledby="appearance-title">
        <div className="settings-section-heading">
          <Palette aria-hidden="true" />
          <div>
            <h2 id="appearance-title">Appearance</h2>
            <p>
              {selectionAvailable
                ? "The installed Ygg catalog recolors the same stable interface."
                : "This theme is selected by the connected Ygg host."}
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
            const pigment = themeColorToCss(
              option.theme.colors.accent,
              "#168f91",
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
