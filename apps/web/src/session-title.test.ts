import { describe, expect, it } from "vitest";
import {
  deriveSessionTitle,
  isUntitledSession,
} from "./session-title";

describe("session titles", () => {
  it("recognizes provisional host labels", () => {
    expect(isUntitledSession("New session")).toBe(true);
    expect(isUntitledSession("Session")).toBe(true);
    expect(isUntitledSession("Refine onboarding")).toBe(false);
  });

  it("derives a compact title from the first prompt", () => {
    expect(
      deriveSessionTitle(
        "  Inspect   this project\nwithout changing anything and report back.  ",
      ),
    ).toBe("Inspect this project without changing anything and report ba…");
  });

  it("uses an attachment name for attachment-only turns", () => {
    expect(deriveSessionTitle("", "layout.png")).toBe("layout.png");
  });

  it("bounds long titles without splitting Unicode code points", () => {
    expect(deriveSessionTitle("🌲".repeat(80))).toBe(`${"🌲".repeat(60)}…`);
  });
});
