/// <reference types="vite/client" />

import { act, cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { TuiSplashLogo } from "./TuiSplashLogo";
import {
  renderTuiSplashFrame,
  TUI_SPLASH_DURATION_SECONDS,
} from "./tuiSplash";

afterEach(() => {
  cleanup();
  delete document.documentElement.dataset.motion;
});

describe("TUI startup splash", () => {
  it("uses the TUI reveal, settle, and diagonal shimmer sequence", () => {
    const start = renderTuiSplashFrame(0, "#cc785c");
    const reveal = renderTuiSplashFrame(0.4, "#cc785c");
    const shimmer = renderTuiSplashFrame(1.45, "#cc785c");
    const settled = renderTuiSplashFrame(
      TUI_SPLASH_DURATION_SECONDS,
      "#cc785c",
    );
    const visible = (frame: typeof start) =>
      frame.dark.filter((cell) => cell.color !== null).length;

    expect(visible(start)).toBe(0);
    expect(visible(reveal)).toBeGreaterThan(0);
    expect(visible(reveal)).toBeLessThan(visible(settled));
    expect(shimmer.dark.map((cell) => cell.color)).not.toEqual(
      settled.dark.map((cell) => cell.color),
    );
    expect(
      settled.dark.some((cell) => /[\u2801-\u28ff]/u.test(cell.glyph)),
    ).toBe(true);
  });

  it("keeps the braille geometry stable while adapting its colors by model", () => {
    const openAi = renderTuiSplashFrame(
      TUI_SPLASH_DURATION_SECONDS,
      "#1f1f1f",
    );
    const anthropic = renderTuiSplashFrame(
      TUI_SPLASH_DURATION_SECONDS,
      "#cc785c",
    );

    expect(openAi.dark.map((cell) => cell.glyph)).toEqual(
      anthropic.dark.map((cell) => cell.glyph),
    );
    expect(openAi.dark.map((cell) => cell.color)).not.toEqual(
      anthropic.dark.map((cell) => cell.color),
    );
  });

  it("shows the final frame without animation when motion is reduced", () => {
    document.documentElement.dataset.motion = "reduced";
    const { container } = render(<TuiSplashLogo modelAccent="#cc785c" />);

    expect(container.querySelector(".tui-splash-logo")).toHaveAttribute(
      "data-animation",
      "settled",
    );
    expect(
      container.querySelectorAll(
        '.tui-splash-cell:not([style*="transparent"])',
      ).length,
    ).toBeGreaterThan(0);
  });

  it("starts the sequence again when a new session key remounts it", () => {
    let nextFrame: FrameRequestCallback | undefined;
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      nextFrame = callback;
      return 1;
    });
    vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => {});
    const { container, rerender } = render(
      <TuiSplashLogo key="session-one" modelAccent="#cc785c" />,
    );

    expect(container.querySelector(".tui-splash-logo")).toHaveAttribute(
      "data-animation",
      "animating",
    );
    act(() => nextFrame?.(performance.now() + 2_300));
    expect(container.querySelector(".tui-splash-logo")).toHaveAttribute(
      "data-animation",
      "settled",
    );

    rerender(<TuiSplashLogo key="session-two" modelAccent="#34a853" />);
    expect(container.querySelector(".tui-splash-logo")).toHaveAttribute(
      "data-animation",
      "animating",
    );
  });
});
