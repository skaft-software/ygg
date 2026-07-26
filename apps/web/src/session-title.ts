const UNTITLED_SESSION_NAMES = new Set([
  "new session",
  "session",
  "(empty session)",
]);

export function isUntitledSession(title: string): boolean {
  return UNTITLED_SESSION_NAMES.has(title.trim().toLowerCase());
}

export function deriveSessionTitle(
  prompt: string,
  attachmentName?: string,
): string {
  const source = prompt.trim() || attachmentName?.trim() || "New session";
  const normalized = source.replace(/\s+/g, " ");
  const characters = Array.from(normalized);
  return characters.length > 60
    ? `${characters.slice(0, 60).join("").trimEnd()}…`
    : normalized;
}
