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
    models: [
      {
        provider: "anthropic",
        model: "claude-sonnet-4-6",
        promptTokens: 90 * multiplier,
        completionTokens: 60 * multiplier,
        cacheReadTokens: 30 * multiplier,
        cacheWriteTokens: 15 * multiplier,
        cacheWriteOneHourTokens: 4 * multiplier,
        reasoningTokens: 12 * multiplier,
        totalTokens: 195 * multiplier,
        requestCount: 2 * multiplier,
      },
      {
        provider: "unknown",
        model: "unknown",
        promptTokens: 30 * multiplier,
        completionTokens: 20 * multiplier,
        cacheReadTokens: 10 * multiplier,
        cacheWriteTokens: 5 * multiplier,
        cacheWriteOneHourTokens: 1 * multiplier,
        reasoningTokens: 4 * multiplier,
        totalTokens: 65 * multiplier,
        requestCount: 1 * multiplier,
      },
    ],
    modelsTruncated: false,
  };
}

describe("usage page", () => {
  afterEach(cleanup);

  it("shows compact totals, model usage, activity, and an all-time range", async () => {
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

    const today = await screen.findByRole("region", {
      name: "Today usage summary",
    });
    expect(within(today).getByText("260")).toBeVisible();
    expect(
      within(today).getByText("Fresh input").closest("div"),
    ).toHaveTextContent("120");
    expect(screen.getByRole("button", { name: "Today" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    const models = screen.getByRole("table", {
      name: "Today usage by model",
    });
    expect(within(models).getByText("claude-sonnet-4-6")).toBeVisible();
    expect(within(models).getByText("Unknown model")).toBeVisible();
    expect(within(models).getByText("Unknown provider")).toBeVisible();
    expect(within(models).getByText("75%")).toBeVisible();
    expect(
      screen.getByRole("grid", {
        name: "Daily token activity for the last 53 weeks",
      }),
    ).toBeVisible();
    expect(screen.queryByLabelText("Usage streaks")).toBeNull();
    expect(screen.queryByRole("heading", { name: "Lifetime" })).toBeNull();
    expect(loadLifetime).toHaveBeenCalledOnce();
    expect(loadActivity).toHaveBeenCalledOnce();

    await user.click(screen.getByRole("button", { name: "7 days" }));

    await waitFor(() => expect(loadStats).toHaveBeenLastCalledWith("weekly"));
    const weekly = await screen.findByRole("region", {
      name: "Last 7 days usage summary",
    });
    expect(
      within(weekly).getByText("Fresh input").closest("div"),
    ).toHaveTextContent("240");
    expect(screen.getByRole("button", { name: "7 days" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    await user.click(screen.getByRole("button", { name: "All time" }));

    const allTime = await screen.findByRole("region", {
      name: "All time usage summary",
    });
    expect(within(allTime).getByText("520")).toBeVisible();
    expect(allTime).toHaveTextContent("Jan 1, 2025 – Jan 3, 2025");
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
    expect(
      await screen.findByRole("region", { name: "Today usage summary" }),
    ).toHaveTextContent("Fresh input");
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
