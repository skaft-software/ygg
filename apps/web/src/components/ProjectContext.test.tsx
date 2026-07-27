/// <reference types="vite/client" />

import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RepositoryContextSnapshot } from "../protocol";
import { ProjectContext } from "./ProjectContext";

const refreshedAtUnixMs = Date.UTC(2026, 6, 27, 14, 30, 15);

const snapshot: RepositoryContextSnapshot = {
  projectId: "prj_ygg",
  trust: "verified",
  repository: {
    source: "gitStatusPorcelainV2",
    refresh: {
      state: "current",
      refreshedAtUnixMs,
      durationMs: 18,
      truncated: false,
    },
    worktree: "present",
    head: "0123456789abcdef0123456789abcdef01234567",
    branchState: "named",
    branch: "feature/project-context",
    dirty: true,
    ahead: 2,
    behind: 1,
  },
  instructions: {
    source: "projectAgentsMdV1",
    refresh: {
      state: "partial",
      refreshedAtUnixMs: refreshedAtUnixMs + 4,
      durationMs: 7,
      truncated: true,
    },
    files: [
      {
        origin: { relativePath: "AGENTS.md", scope: "." },
        precedence: 0,
        byteLen: 48,
        sha256: "a".repeat(64),
        summary: "# Workspace rules",
        visibleContent: "# Workspace rules\nKeep changes focused.",
        contentTruncated: false,
      },
      {
        origin: {
          relativePath: "apps/web/AGENTS.md",
          scope: "apps/web",
        },
        precedence: 1,
        byteLen: 70_000,
        sha256: "b".repeat(64),
        summary: "# Web rules",
        visibleContent: "# Web rules\nBuild accessible interfaces.",
        contentTruncated: true,
      },
    ],
    errors: [
      {
        origin: {
          relativePath: "docs/AGENTS.md",
          scope: "docs",
        },
        code: "symlinkRejected",
      },
      {
        code: "discoveryLimitReached",
      },
    ],
    omittedErrors: 2,
    loadedBytes: 70_048,
  },
};

describe("project context", () => {
  afterEach(cleanup);

  it("shows path-free Git facts and distinguishes Git from conversation branches", () => {
    render(
      <ProjectContext
        snapshot={snapshot}
        loading={false}
        onRefresh={vi.fn()}
      />,
    );

    const repository = screen
      .getByRole("heading", { name: "Git repository" })
      .closest("article");
    expect(repository).not.toBeNull();
    expect(
      within(repository!).getByText(
        /Repository branches shown here are separate from conversation branches/,
      ),
    ).toBeVisible();
    expect(within(repository!).getByText("Git worktree")).toBeVisible();
    expect(
      within(repository!).getByText(
        "0123456789abcdef0123456789abcdef01234567",
      ),
    ).toBeVisible();
    expect(
      within(repository!).getByText("feature/project-context"),
    ).toBeVisible();
    expect(within(repository!).getByText("Yes — changes present")).toBeVisible();
    expect(
      within(repository!).getByText("Ahead of upstream").nextElementSibling,
    ).toHaveTextContent("2");
    expect(
      within(repository!).getByText("Behind upstream").nextElementSibling,
    ).toHaveTextContent("1");

    expect(
      within(repository!).getByText("Git status (porcelain v2)"),
    ).toBeVisible();
    expect(
      within(repository!).getByText("2026-07-27 14:30:15 UTC"),
    ).toHaveAttribute("datetime", "2026-07-27T14:30:15.000Z");
    expect(screen.queryByText("prj_ygg")).toBeNull();
  });

  it("renders root-first AGENTS.md metadata, bounded content, and safe errors", async () => {
    const user = userEvent.setup();
    render(
      <ProjectContext
        snapshot={snapshot}
        loading={false}
        onRefresh={vi.fn()}
      />,
    );

    const instructionItems = document.querySelectorAll<HTMLElement>(
      ".project-context-instruction-list > li",
    );
    expect(instructionItems).toHaveLength(2);
    expect(within(instructionItems[0]!).getByText("AGENTS.md")).toBeVisible();
    expect(
      within(instructionItems[0]!).getByText("Precedence 0"),
    ).toBeVisible();
    expect(
      within(instructionItems[0]!).getByText("# Workspace rules"),
    ).toBeVisible();
    expect(within(instructionItems[0]!).getByText(".")).toBeVisible();
    expect(
      within(instructionItems[1]!).getByText("apps/web/AGENTS.md"),
    ).toBeVisible();
    expect(
      within(instructionItems[1]!).getByText("Precedence 1"),
    ).toBeVisible();
    expect(
      within(instructionItems[1]!).getByText("apps/web"),
    ).toBeVisible();

    await user.click(
      within(instructionItems[1]!).getByText("View instruction content"),
    );
    expect(
      instructionItems[1]!.querySelector(
        ".project-context-instruction-content pre",
      ),
    ).toHaveTextContent("# Web rules Build accessible interfaces.");
    expect(
      within(instructionItems[1]!).getByText(
        "Content preview truncated at the server safety limit.",
      ),
    ).toBeVisible();

    expect(screen.getByText("docs/AGENTS.md")).toBeVisible();
    expect(
      screen.getByText(
        "This instruction was ignored because symbolic links are not allowed.",
      ),
    ).toBeVisible();
    expect(
      screen.getByText(
        "Instruction discovery stopped at a bounded safety limit.",
      ),
    ).toBeVisible();
    expect(
      screen.getByText("2 additional errors were omitted at the server safety limit."),
    ).toBeVisible();
  });

  it("announces loading, refresh, and classified errors without raw error text", async () => {
    const user = userEvent.setup();
    const onRefresh = vi.fn();
    const { rerender } = render(
      <ProjectContext
        snapshot={null}
        loading
        onRefresh={onRefresh}
      />,
    );

    expect(screen.getByRole("status")).toHaveTextContent(
      "Loading project context…",
    );
    expect(screen.getByRole("button", { name: "Loading…" })).toBeDisabled();
    expect(screen.queryByText("Verified project snapshot")).toBeNull();

    rerender(
      <ProjectContext
        snapshot={snapshot}
        loading={false}
        refreshing
        error="rootChanged"
        onRefresh={onRefresh}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "The trusted project folder changed or is unavailable.",
    );
    expect(
      screen.getByRole("button", { name: "Refreshing…" }),
    ).toBeDisabled();
    expect(screen.getByText("Verified project snapshot")).toBeVisible();
    expect(screen.getByRole("heading", { name: "Git repository" })).toBeVisible();

    rerender(
      <ProjectContext
        snapshot={snapshot}
        loading={false}
        onRefresh={onRefresh}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    expect(onRefresh).toHaveBeenCalledOnce();
  });

  it("renders explicit non-repository and empty-instruction states", () => {
    render(
      <ProjectContext
        snapshot={{
          ...snapshot,
          repository: {
            ...snapshot.repository,
            refresh: {
              ...snapshot.repository.refresh,
              state: "notApplicable",
            },
            worktree: "notRepository",
            head: undefined,
            branchState: "unknown",
            branch: undefined,
            dirty: undefined,
            ahead: undefined,
            behind: undefined,
          },
          instructions: {
            ...snapshot.instructions,
            refresh: {
              ...snapshot.instructions.refresh,
              state: "current",
              truncated: false,
            },
            files: [],
            errors: [],
            omittedErrors: 0,
            loadedBytes: 0,
          },
        }}
        loading={false}
        onRefresh={vi.fn()}
      />,
    );

    expect(screen.getByText("Not a Git repository")).toBeVisible();
    expect(
      screen.getByText("No AGENTS.md instruction files were loaded."),
    ).toBeVisible();
  });
});
