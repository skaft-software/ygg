/// <reference types="vite/client" />

import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length() {
    return this.values.size;
  }

  clear() {
    this.values.clear();
  }

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  key(index: number) {
    return Array.from(this.values.keys())[index] ?? null;
  }

  removeItem(key: string) {
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

describe("App transcript search workflow", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/?transport=fixture");
    vi.stubGlobal("localStorage", new MemoryStorage());
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: vi.fn((query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addListener: vi.fn(),
        removeListener: vi.fn(),
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        dispatchEvent: vi.fn(() => true),
      })),
    });
    Object.defineProperty(window, "requestAnimationFrame", {
      configurable: true,
      value: (callback: FrameRequestCallback) =>
        window.setTimeout(() => callback(performance.now()), 0),
    });
    Object.defineProperty(window, "cancelAnimationFrame", {
      configurable: true,
      value: (handle: number) => window.clearTimeout(handle),
    });
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: vi.fn(),
    });
  });

  afterEach(() => {
    cleanup();
    window.history.replaceState(null, "", "/");
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("searches session contents, selects another session, and jumps to the matched item", async () => {
    const user = userEvent.setup();
    const { default: App } = await import("./App");
    render(<App />);

    const query = await screen.findByRole("searchbox", {
      name: "Search tasks and transcripts",
    });
    await user.type(query, "release candidate");

    const result = await screen.findByRole("button", {
      name: "Open User message result from Review release readiness",
    });
    expect(
      screen.queryByRole("button", { name: "Search conversation contents" }),
    ).toBeNull();
    expect(
      within(result).getByText("release candidate", { selector: "strong" }),
    ).toBeVisible();
    await user.click(result);

    expect(
      screen.queryByRole("dialog", { name: "Search conversations" }),
    ).toBeNull();
    await waitFor(() => {
      expect(
        document.querySelector(".session-header strong"),
      ).toHaveTextContent("Review release readiness");
      const target = document.getElementById(
        "transcript-item-done-user",
      );
      expect(target).not.toBeNull();
      expect(target).toHaveFocus();
      expect(target).toHaveClass("is-search-target");
      expect(target?.scrollIntoView).toHaveBeenCalledWith({
        block: "center",
        behavior: "smooth",
      });
    });
    expect(window.location.pathname).toBe("/session/session-done");
  });
});
