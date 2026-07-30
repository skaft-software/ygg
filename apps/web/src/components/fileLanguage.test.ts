import { describe, expect, it } from "vitest";
import { languageNameForPath } from "./fileLanguage";

describe("file language detection", () => {
  it("matches common extensions and extensionless project files", () => {
    expect(languageNameForPath("src/main.rs")).toBe("Rust");
    expect(languageNameForPath("web/App.TSX")).toBe("TSX");
    expect(languageNameForPath("Dockerfile")).toBe("Dockerfile");
    expect(languageNameForPath("config/settings.toml")).toBe("TOML");
    expect(languageNameForPath("BUILD")).toBe("Python");
  });

  it("keeps unknown files as plain text", () => {
    expect(languageNameForPath("notes/example.unknown")).toBe("Plain text");
  });
});
