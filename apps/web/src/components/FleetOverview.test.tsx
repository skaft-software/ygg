/// <reference types="vite/client" />

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap } from "../fixtures";
import { formatTaskAge } from "../fleet-status";
import type { SessionSummary } from "../protocol";
import { FleetOverview } from "./FleetOverview";

afterEach(cleanup);

describe("fleet command center", () => {
  it("summarizes real session signals and prioritizes attention", () => {
    const selectTask = vi.fn();
    render(
      <FleetOverview
        sessions={fixtureBootstrap.sessions}
        projects={fixtureBootstrap.projects}
        selectedSessionId="session-fresh"
        onNewTask={vi.fn()}
        onSelectTask={selectTask}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "All agents. One clear queue." }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Show needs you tasks, 1" }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Show working tasks, 1" }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Show review tasks, 1" }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Show complete tasks, 1" }),
    ).toBeVisible();

    const rows = screen.getAllByRole("button", { name: /^Open task/ });
    expect(rows[0]).toHaveAccessibleName(
      "Open task Prepare signed macOS build, Needs you, ygg",
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Show needs you tasks, 1" }),
    );
    expect(
      screen.getByRole("button", {
        name: "Open task Prepare signed macOS build, Needs you, ygg",
      }),
    ).toBeVisible();
    expect(
      screen.queryByRole("button", {
        name: "Open task Refine onboarding preview, Working, ygg",
      }),
    ).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Open next" }));
    expect(selectTask).toHaveBeenCalledWith("session-attention");
  });

  it("searches title, preview, and project without losing status context", () => {
    render(
      <FleetOverview
        sessions={fixtureBootstrap.sessions}
        projects={fixtureBootstrap.projects}
        selectedSessionId={null}
        onNewTask={vi.fn()}
        onSelectTask={vi.fn()}
      />,
    );

    fireEvent.change(
      screen.getByRole("searchbox", { name: "Search active tasks" }),
      { target: { value: "research notes" } },
    );

    expect(
      screen.getByRole("button", {
        name: "Open task Summarize provider notes, Ready, Research notes",
      }),
    ).toBeVisible();
    expect(screen.getAllByRole("button", { name: /^Open task/ })).toHaveLength(1);

    fireEvent.change(
      screen.getByRole("searchbox", { name: "Search active tasks" }),
      { target: { value: "no such task" } },
    );
    expect(screen.getByText("No tasks match this view")).toBeVisible();
  });

  it("keeps an exception first and searchable across 500 active tasks", () => {
    const base = fixtureBootstrap.sessions[0]!;
    const sessions: SessionSummary[] = Array.from({ length: 500 }, (_, index) => ({
      ...structuredClone(base),
      id: `fleet-task-${index}`,
      title: index === 499 ? "Critical fleet exception" : `Routine task ${index}`,
      preview: index === 314 ? "Unique search target" : "Healthy background work",
      status: index === 499 ? "failed" : "idle",
      attentionCount: index === 499 ? 1 : 0,
      updatedAt: new Date(Date.UTC(2030, 0, 1, 0, index)).toISOString(),
    }));

    render(
      <FleetOverview
        sessions={sessions}
        projects={fixtureBootstrap.projects}
        selectedSessionId={null}
        onNewTask={vi.fn()}
        onSelectTask={vi.fn()}
      />,
    );

    expect(
      screen.getByRole("button", { name: "Show needs you tasks, 1" }),
    ).toBeVisible();
    const rows = screen.getAllByRole("button", { name: /^Open task/ });
    expect(rows).toHaveLength(500);
    expect(rows[0]).toHaveAccessibleName(
      "Open task Critical fleet exception, Failed, ygg",
    );

    fireEvent.change(
      screen.getByRole("searchbox", { name: "Search active tasks" }),
      { target: { value: "Unique search target" } },
    );
    expect(screen.getAllByRole("button", { name: /^Open task/ })).toHaveLength(1);
    expect(screen.getByText("Routine task 314")).toBeVisible();
  });

  it("keeps compact task ages predictable", () => {
    const now = Date.UTC(2030, 0, 8, 12, 0, 0);
    expect(formatTaskAge(new Date(now - 15_000).toISOString(), now)).toBe("Now");
    expect(formatTaskAge(new Date(now - 12 * 60_000).toISOString(), now)).toBe(
      "12m",
    );
    expect(
      formatTaskAge(new Date(now - 3 * 60 * 60_000).toISOString(), now),
    ).toBe("3h");
    expect(
      formatTaskAge(new Date(now - 4 * 24 * 60 * 60_000).toISOString(), now),
    ).toBe("4d");
    expect(formatTaskAge("invalid", now)).toBe("Unknown");
  });
});
