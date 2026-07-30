import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";

function basename(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

export function isMarkdownPath(path: string): boolean {
  const filename = basename(path).toLowerCase();
  return (
    filename === "readme" ||
    filename.endsWith(".md") ||
    filename.endsWith(".markdown")
  );
}

export function languageForPath(path: string): LanguageDescription | null {
  const filename = basename(path);
  return (
    LanguageDescription.matchFilename(languages, filename) ??
    LanguageDescription.matchFilename(languages, filename.toLowerCase())
  );
}

export function languageNameForPath(path: string): string {
  return languageForPath(path)?.name ?? "Plain text";
}
