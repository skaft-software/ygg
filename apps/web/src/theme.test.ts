/// <reference types="vite/client" />

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ThemeDto } from "./protocol";
import {
  applyStoredTypePreferences,
  applyTheme,
  themeColorToCss,
} from "./theme";

describe("stored type preferences", () => {
  const values = new Map<string, string>();

  beforeEach(() => {
    values.clear();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
      clear: () => values.clear(),
    });
  });

  afterEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("style");
    vi.unstubAllGlobals();
  });

  it("keeps chat compact while preserving prompt and title hierarchy", () => {
    localStorage.setItem("ygg.ui.size", "13");

    applyStoredTypePreferences();

    const style = document.documentElement.style;
    expect(style.getPropertyValue("--font-body")).toBe("13px");
    expect(style.getPropertyValue("--font-meta")).toBe("12px");
    expect(style.getPropertyValue("--font-chat")).toBe("14px");
    expect(style.getPropertyValue("--font-prompt")).toBe("16px");
    expect(style.getPropertyValue("--font-display")).toBe("16px");
  });

  it("resolves the real host catalog through semantic role tokens", () => {
    vi.stubGlobal("matchMedia", () => ({
      matches: true,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    }));
    const theme: ThemeDto = {
      name: "Signal Noir",
      source: "bundled",
      revision: 1,
      scheme: "dark",
      density: "comfortable",
      motion: "full",
      typography: {
        body_family: "system-ui",
        mono_family: "ui-monospace",
        body_size: 17,
        display_ratio_milli: 1235,
      },
      colors: {
        "role.0.foreground": {
          kind: "rgb",
          red: 181,
          green: 44,
          blue: 58,
        },
        "role.1.foreground": { kind: "ansi", index: 250 },
        "role.2.foreground": { kind: "ansi", index: 245 },
      },
      roles: {
        accent: {
          foreground: "role.0.foreground",
          bold: false,
          dim: false,
          italic: false,
          underline: false,
          strikethrough: false,
        },
        text: {
          foreground: "role.1.foreground",
          bold: false,
          dim: false,
          italic: false,
          underline: false,
          strikethrough: false,
        },
        muted: {
          foreground: "role.2.foreground",
          bold: false,
          dim: false,
          italic: false,
          underline: false,
          strikethrough: false,
        },
      },
    };

    applyTheme(theme);

    const style = document.documentElement.style;
    expect(style.getPropertyValue("--theme-pigment")).toBe("rgb(181 44 58)");
    expect(style.getPropertyValue("--theme-foreground")).toBe("rgb(188 188 188)");
    expect(style.getPropertyValue("--theme-muted")).toBe("rgb(138 138 138)");
    expect(document.documentElement.dataset.colorScheme).toBe("dark");
  });

  it("projects ANSI cube colors instead of silently using the fallback", () => {
    expect(themeColorToCss({ kind: "ansi", index: 33 }, "#000")).toBe(
      "rgb(0 135 255)",
    );
  });
});
