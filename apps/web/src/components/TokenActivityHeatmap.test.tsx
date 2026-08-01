/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { TokenActivityHeatmap } from "./TokenActivityHeatmap";

describe("token activity heatmap", () => {
  afterEach(cleanup);

  it("renders 53 aligned weeks with accessible activity and future cells", () => {
    const { container } = render(
      <TokenActivityHeatmap
        today={new Date("2025-01-08T18:00:00.000Z")}
        days={[
          { date: "2024-01-01", tokens: 1_000_000, requestCount: 10 },
          { date: "2025-01-07", tokens: 100, requestCount: 1 },
          { date: "2025-01-08", tokens: 1_000, requestCount: 2 },
        ]}
      />,
    );

    const grid = screen.getByRole("grid", {
      name: "Daily token activity for the last 53 weeks",
    });
    expect(grid).toHaveAttribute("aria-rowcount", "7");
    expect(grid).toHaveAttribute("aria-colcount", "53");
    expect(screen.getAllByRole("gridcell")).toHaveLength(53 * 7);

    const active = container.querySelector<HTMLElement>(
      '[data-date="2025-01-08"]',
    );
    expect(active).not.toBeNull();
    expect(active).toHaveAttribute("data-level", "4");
    expect(active).toHaveAttribute("aria-rowindex", "4");
    expect(active).toHaveAttribute("aria-colindex", "53");
    expect(active?.getAttribute("aria-label")).toMatch(
      /1,000 tokens across 2 requests/,
    );

    const future = container.querySelector<HTMLElement>(
      '[data-date="2025-01-09"]',
    );
    expect(future).toHaveClass("is-future");
    expect(future).toHaveAttribute("data-level", "0");
    expect(future?.getAttribute("aria-label")).toMatch(/future date/);
    expect(screen.getByText("Peak 1K tokens")).toBeVisible();
  });
});
