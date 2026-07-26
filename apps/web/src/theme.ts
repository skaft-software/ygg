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
  root.style.setProperty("--font-display", `${size + 7}px`);
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
  return fallback;
}

export function applyTheme(theme: ThemeDto): void {
  const root = document.documentElement;
  followSystemScheme(root);
  root.dataset.density = theme.density;
  root.dataset.motion = theme.motion;
  root.style.setProperty(
    "--theme-pigment",
    themeColorToCss(theme.colors.accent, "#168f91"),
  );
  root.style.setProperty(
    "--theme-foreground",
    themeColorToCss(theme.colors.foreground, "#edf3f2"),
  );
  root.style.setProperty(
    "--theme-muted",
    themeColorToCss(theme.colors.muted, "#778384"),
  );
}
