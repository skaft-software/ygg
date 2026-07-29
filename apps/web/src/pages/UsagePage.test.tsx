/// <reference types="vite/client" />

import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { UsagePeriod, UsageStats } from "../protocol";
import { UsagePage } from "./UsagePage";

function stats(period: UsagePeriod): UsageStats {
  const multiplier = period === "daily" ? 1 : 2;
  return {
    period,
    promptTokens: 120 * multiplier,
    completionTokens: 80 * multiplier,
    cacheReadTokens: 40 * multiplier,
    cacheWriteTokens: 20 * multiplier,
    cacheWriteOneHourTokens: 5 * multiplier,
    reasoningTokens: 16 * multiplier,
    totalTokens: 260 * multiplier,
    requestCount: 3 * multiplier,
  };
}

describe("usage page", () => {
  afterEach(cleanup);

  it("renders lifetime activity and switches the token breakdown period", async () => {
    const user = userEvent.setup();
    const loadStats = vi.fn(async (period: UsagePeriod) => stats(period));
    const loadLifetime = vi.fn().mockResolvedValue({
      ...stats("weekly"),
      firstRequestAtMs: Date.UTC(2025, 0, 1),
      lastRequestAtMs: Date.UTC(2025, 0, 3),
    });
    const loadActivity = vi.fn().mockResolvedValue({
      days: [
        { date: "2025-01-02", tokens: 100, requestCount: 1 },
        { date: "2025-01-03", tokens: 160, requestCount: 2 },
      ],
      currentStreak: 2,
      longestStreak: 5,
    });

    render(
      <UsagePage
        loadStats={loadStats}
        loadLifetime={loadLifetime}
        loadActivity={loadActivity}
      />,
    );

    expect(await screen.findByRole("heading", { name: "Today" })).toBeVisible();
    const freshInput = screen.getByText("Fresh input").closest("article");
    expect(freshInput).not.toBeNull();
    expect(within(freshInput!).getByText("120")).toBeVisible();
    expect(screen.getByLabelText("Usage streaks")).toHaveTextContent(
      "2 current",
    );
    expect(
      screen.getByRole("grid", {
        name: "Daily token activity for the last 53 weeks",
      }),
    ).toBeVisible();
    expect(screen.getByRole("heading", { name: "Lifetime" })).toBeVisible();
    expect(loadLifetime).toHaveBeenCalledOnce();
    expect(loadActivity).toHaveBeenCalledOnce();

    await user.click(screen.getByRole("button", { name: "Weekly" }));

    await waitFor(() => expect(loadStats).toHaveBeenLastCalledWith("weekly"));
    expect(
      await screen.findByRole("heading", { name: "Trailing seven days" }),
    ).toBeVisible();
    expect(screen.getByRole("button", { name: "Weekly" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByText("Fresh input").closest("article")).toHaveTextContent(
      "240",
    );
    expect(loadStats.mock.calls.map(([period]) => period)).toEqual([
      "daily",
      "weekly",
    ]);
  });

  it("offers a retry when a usage projection cannot be loaded", async () => {
    const user = userEvent.setup();
    const loadStats = vi
      .fn<(period: UsagePeriod) => Promise<UsageStats>>()
      .mockRejectedValueOnce(new Error("Usage unavailable"))
      .mockResolvedValueOnce(stats("daily"));

    render(
      <UsagePage
        loadStats={loadStats}
        loadLifetime={vi.fn().mockResolvedValue({
          ...stats("daily"),
          firstRequestAtMs: undefined,
          lastRequestAtMs: undefined,
        })}
        loadActivity={vi.fn().mockResolvedValue({
          days: [],
          currentStreak: 0,
          longestStreak: 0,
        })}
      />,
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Usage unavailable",
    );
    await user.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(loadStats).toHaveBeenCalledTimes(2));
    expect(await screen.findByText("Standard-rate prompt tokens")).toBeVisible();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
