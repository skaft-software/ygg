/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  ProjectCatalog,
  RepositoryContextSnapshot,
} from "../protocol";
import { ProjectsView } from "./Projects";

const catalog: ProjectCatalog = {
  host: { id: "host-local", name: "Local Mac" },
  catalogRevision: 4,
  lifecycleMutationsSupported: true,
  importSupported: false,
  projects: [
    {
      id: "prj_safe",
      name: "ygg",
      trusted: false,
      archived: false,
      available: true,
      isDefault: false,
      sessionCount: 2,
      liveSessionCount: 0,
    },
  ],
};

function repositoryContext(branch: string): RepositoryContextSnapshot {
  return {
    projectId: "prj_safe",
    trust: "verified",
    repository: {
      source: "gitStatusPorcelainV2",
      refresh: {
        state: "current",
        refreshedAtUnixMs: 1_753_626_615_000,
        durationMs: 7,
        truncated: false,
      },
      worktree: "present",
      head: "0123456789abcdef0123456789abcdef01234567",
      branchState: "named",
      branch,
      dirty: false,
      ahead: 0,
      behind: 0,
    },
    instructions: {
      source: "projectAgentsMdV1",
      refresh: {
        state: "current",
        refreshedAtUnixMs: 1_753_626_615_001,
        durationMs: 2,
        truncated: false,
      },
      files: [
        {
          origin: { relativePath: "AGENTS.md", scope: "." },
          precedence: 0,
          byteLen: 23,
          sha256: "a".repeat(64),
          summary: "# Keep changes focused",
          visibleContent: "# Keep changes focused\n",
          contentTruncated: false,
        },
      ],
      errors: [],
      omittedErrors: 0,
      loadedBytes: 23,
    },
  };
}

describe("projects", () => {
  afterEach(cleanup);

  it("requires explicit trust without exposing or accepting a host path", async () => {
    const user = userEvent.setup();
    const setTrust = vi.fn().mockResolvedValue(undefined);
    render(
      <ProjectsView
        catalog={catalog}
        onboarding
        onRename={vi.fn()}
        onSetDefault={vi.fn()}
        onSetTrust={setTrust}
        onArchive={vi.fn()}
      />,
    );

    expect(screen.getByText("No trusted project is available")).toBeVisible();
    expect(
      screen.getByText(/load that folder's instructions, skills, and extensions/),
    ).toBeVisible();
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(
      screen.getByText(/never asks you to type or transmit an absolute host path/),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Trust project" }));
    expect(setTrust).toHaveBeenCalledWith("prj_safe", true);
  });

  it("guards archive and supports an inline path-free rename", async () => {
    const user = userEvent.setup();
    const rename = vi.fn().mockResolvedValue(undefined);
    const archive = vi.fn().mockResolvedValue(undefined);
    render(
      <ProjectsView
        catalog={{ ...catalog, projects: [{ ...catalog.projects[0]!, trusted: true }] }}
        onRename={rename}
        onSetDefault={vi.fn()}
        onSetTrust={vi.fn()}
        onArchive={archive}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Rename" }));
    const input = screen.getByRole("textbox", { name: "Rename ygg" });
    await user.clear(input);
    await user.type(input, "Ygg workspace");
    await user.click(screen.getByRole("button", { name: "Save project name" }));
    expect(rename).toHaveBeenCalledWith("prj_safe", "Ygg workspace");

    await user.click(screen.getByRole("button", { name: "Archive" }));
    expect(archive).not.toHaveBeenCalled();
    await user.click(
      screen.getByRole("button", { name: "Archive and revoke" }),
    );
    expect(archive).toHaveBeenCalledWith("prj_safe");
  });

  it("loads and refreshes trusted Git and instruction context", async () => {
    const user = userEvent.setup();
    const loadContext = vi
      .fn()
      .mockResolvedValueOnce(repositoryContext("main"))
      .mockResolvedValueOnce(repositoryContext("feature/refreshed"));
    render(
      <ProjectsView
        catalog={{
          ...catalog,
          projects: [{ ...catalog.projects[0]!, trusted: true }],
        }}
        onRename={vi.fn()}
        onSetDefault={vi.fn()}
        onSetTrust={vi.fn()}
        onArchive={vi.fn()}
        onLoadContext={loadContext}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "Inspect repository context" }),
    );
    expect(loadContext).toHaveBeenNthCalledWith(1, "prj_safe");
    expect(await screen.findByText("main")).toBeVisible();
    expect(
      screen.getByText("# Keep changes focused", { selector: "p" }),
    ).toBeVisible();
    expect(
      screen.getByRole("heading", { name: "Folder instructions" }),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Refresh" }));
    expect(loadContext).toHaveBeenNthCalledWith(2, "prj_safe");
    expect(await screen.findByText("feature/refreshed")).toBeVisible();
    expect(screen.queryByText("main")).toBeNull();
  });
});
