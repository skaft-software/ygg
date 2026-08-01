import { describe, expect, it } from "vitest";
import { isMarkdownPath, languageNameForPath } from "./fileLanguage";

describe("file language detection", () => {
  it("matches common extensions and extensionless project files", () => {
    expect(languageNameForPath("src/main.rs")).toBe("Rust");
    expect(languageNameForPath("web/App.TSX")).toBe("TSX");
    expect(languageNameForPath("Dockerfile")).toBe("Dockerfile");
    expect(languageNameForPath("config/settings.toml")).toBe("TOML");
    expect(languageNameForPath("BUILD")).toBe("Python");
  });

  it("detects Markdown extensions and README conventions case-insensitively", () => {
    expect(isMarkdownPath("docs/guide.md")).toBe(true);
    expect(isMarkdownPath("docs/guide.MARKDOWN")).toBe(true);
    expect(isMarkdownPath("README")).toBe(true);
    expect(isMarkdownPath("README.txt")).toBe(false);
  });

  it("keeps unknown files as plain text", () => {
    expect(languageNameForPath("notes/example.unknown")).toBe("Plain text");
  });
});
