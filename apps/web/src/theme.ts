import type { ThemeColor, ThemeDto } from "./protocol";

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
  root.dataset.colorScheme = theme.scheme === "light" ? "light" : "dark";
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
