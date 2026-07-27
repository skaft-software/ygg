import type { ThemeColor, ThemeDto } from "./protocol";

let systemSchemeMedia: MediaQueryList | null = null;
let systemSchemeListener: ((event: MediaQueryListEvent) => void) | null = null;

const storedFontStacks: Record<
  string,
  { ui: string; mono: string }
> = {
  geist: {
    ui: '"Geist", Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"Geist Mono", "SFMono-Regular", Menlo, Consolas, ui-monospace, monospace',
  },
  "ibm-plex": {
    ui: '"IBM Plex Sans", Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"IBM Plex Mono", "IBM Plex Mono Text", ui-monospace, monospace',
  },
  "jetbrains-nerd": {
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"JetBrainsMono Nerd Font", "JetBrains Mono", ui-monospace, monospace',
  },
  "iosevka-nerd": {
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"Iosevka Nerd Font", Iosevka, ui-monospace, monospace',
  },
  "firacode-nerd": {
    ui: 'Inter, ui-sans-serif, system-ui, sans-serif',
    mono: '"FiraCode Nerd Font", "Fira Code", ui-monospace, monospace',
  },
  "system-mono": {
    ui: '-apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    mono: '"SFMono-Regular", "Cascadia Mono", Menlo, Consolas, ui-monospace, monospace',
  },
};

export function applyStoredTypePreferences(): void {
  const root = document.documentElement;
  const storedStackId = localStorage.getItem("ygg.ui.font");
  const stackId =
    !storedStackId || storedStackId === "ibm-plex-mono"
      ? "geist"
      : storedStackId;
  const stack = storedFontStacks[stackId] ?? storedFontStacks.geist;
  const sizeValue = Number(localStorage.getItem("ygg.ui.size") ?? "13");
  const size = [12, 13, 14, 15].includes(sizeValue) ? sizeValue : 13;
  root.style.setProperty("--ui-family", stack.ui);
  root.style.setProperty("--mono-family", stack.mono);
  root.style.setProperty("--font-body", `${size}px`);
  root.style.setProperty("--font-meta", `${Math.max(11, size - 1)}px`);
  root.style.setProperty("--font-chat", `${size + 2}px`);
  root.style.setProperty("--font-prompt", `${size + 2}px`);
  root.style.setProperty("--font-display", `${size + 2}px`);
  localStorage.setItem("ygg.ui.font", stackId);
}

function stopFollowingSystemScheme(): void {
  if (systemSchemeMedia && systemSchemeListener) {
    systemSchemeMedia.removeEventListener("change", systemSchemeListener);
  }
  systemSchemeMedia = null;
  systemSchemeListener = null;
}

function followSystemScheme(root: HTMLElement): void {
  if (typeof window.matchMedia !== "function") {
    root.dataset.colorScheme = "light";
    return;
  }
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const apply = (matches: boolean) => {
    root.dataset.colorScheme = matches ? "dark" : "light";
  };
  stopFollowingSystemScheme();
  apply(media.matches);
  systemSchemeMedia = media;
  systemSchemeListener = (event) => apply(event.matches);
  media.addEventListener("change", systemSchemeListener);
}

export function themeColorToCss(
  color: ThemeColor | undefined,
  fallback: string,
): string {
  if (!color || color.kind === "default") return fallback;
  if (color.kind === "rgb") {
    return `rgb(${color.red} ${color.green} ${color.blue})`;
  }
  if (color.kind === "ansi") {
    const index = Math.max(0, Math.min(255, Math.trunc(color.index)));
    const ansi16 = [
      [0, 0, 0],
      [205, 49, 49],
      [13, 188, 121],
      [229, 229, 16],
      [36, 114, 200],
      [188, 63, 188],
      [17, 168, 205],
      [229, 229, 229],
      [102, 102, 102],
      [241, 76, 76],
      [35, 209, 139],
      [245, 245, 67],
      [59, 142, 234],
      [214, 112, 214],
      [41, 184, 219],
      [255, 255, 255],
    ] as const;
    let red: number;
    let green: number;
    let blue: number;
    if (index < ansi16.length) {
      [red, green, blue] = ansi16[index];
    } else if (index < 232) {
      const cube = index - 16;
      const levels = [0, 95, 135, 175, 215, 255];
      red = levels[Math.floor(cube / 36) % 6];
      green = levels[Math.floor(cube / 6) % 6];
      blue = levels[cube % 6];
    } else {
      red = green = blue = 8 + (index - 232) * 10;
    }
    return `rgb(${red} ${green} ${blue})`;
  }
  return fallback;
}

export function themeRoleColorToCss(
  theme: ThemeDto,
  roles: string[],
  fallback: string,
  legacyColorKey?: string,
): string {
  for (const role of roles) {
    const token = theme.roles[role]?.foreground;
    if (token) {
      const resolved = theme.colors[token];
      if (resolved) return themeColorToCss(resolved, fallback);
    }
  }
  return themeColorToCss(
    legacyColorKey ? theme.colors[legacyColorKey] : undefined,
    fallback,
  );
}

export function applyTheme(theme: ThemeDto, themeId?: string): void {
  const root = document.documentElement;
  followSystemScheme(root);
  if (themeId) root.dataset.theme = themeId;
  root.dataset.density = theme.density;
  root.dataset.motion = theme.motion;
  root.style.setProperty(
    "--theme-pigment",
    themeRoleColorToCss(theme, ["accent", "link"], "#168f91", "accent"),
  );
  root.style.setProperty(
    "--theme-foreground",
    themeRoleColorToCss(theme, ["text"], "#edf3f2", "foreground"),
  );
  root.style.setProperty(
    "--theme-muted",
    themeRoleColorToCss(theme, ["muted", "subtle"], "#778384", "muted"),
  );
}
