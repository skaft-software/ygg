const storedFontStacks: Record<string, { ui: string; mono: string }> = {
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
    !storedStackId
      ? "system-mono"
      : storedStackId === "ibm-plex-mono"
        ? "geist"
        : storedStackId;
  const stack = storedFontStacks[stackId] ?? storedFontStacks["system-mono"];
  const sizeValue = Number(localStorage.getItem("ygg.ui.size") ?? "14");
  const size = [12, 13, 14, 15].includes(sizeValue) ? sizeValue : 14;
  root.style.setProperty("--ui-family", stack.ui);
  root.style.setProperty("--mono-family", stack.mono);
  root.style.setProperty("--font-body", `${size}px`);
  root.style.setProperty("--font-meta", `${Math.max(11, size - 2)}px`);
  root.removeAttribute("data-theme");
  root.removeAttribute("data-color-scheme");
  root.removeAttribute("data-density");
  root.removeAttribute("data-motion");
  localStorage.setItem("ygg.ui.font", stackId);
}
