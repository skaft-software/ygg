/// <reference types="vite/client" />

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap } from "../fixtures";
import type {
  ProjectFileRead,
  ProjectFileTree,
  ProjectFileWriteRequest,
} from "../protocol";
import { ProjectFileConflictError } from "../transport";
import { FilesPanel } from "./FilesPanel";

const project = fixtureBootstrap.projects.find((candidate) => candidate.trusted)!;
const version = "a".repeat(64);

function renderFilesPanel(
  overrides: Partial<ComponentProps<typeof FilesPanel>> = {},
) {
  const tree: ProjectFileTree = {
    path: "",
    entries: [
      { name: "src", kind: "directory", size: 0 },
      { name: "README.md", kind: "file", size: 24 },
    ],
    truncated: false,
  };
  const read: ProjectFileRead = {
    path: "README.md",
    content: `# Fixture

**Bold** and [a link](https://example.com).

- one
- two

| Name | Value |
| --- | --- |
| Ygg | Serve |

\`\`\`ts
const answer = 42;
\`\`\`
`,
    startLine: 1,
    endLine: 14,
    lineCount: 14,
    truncated: false,
    sha256: version,
  };
  const getTree = vi.fn(async () => tree);
  const readFile = vi.fn(async () => read);
  const searchFiles = vi.fn(async () => ({
    hits: [],
    truncated: false,
    scannedBytes: 0,
  }));
  const writeFile = vi.fn(async (_projectId: string, request: ProjectFileWriteRequest) => ({
    path: request.path,
    sha256: "b".repeat(64),
  }));
  render(
    <FilesPanel
      projects={[project]}
      preferredProjectId={project.id}
      writeAvailable
      getTree={getTree}
      readFile={readFile}
      searchFiles={searchFiles}
      writeFile={writeFile}
      {...overrides}
    />,
  );
  return { getTree, readFile, searchFiles, writeFile };
}

afterEach(cleanup);

describe("FilesPanel", () => {
  it("renders Markdown in a rich preview and preserves the editable save flow", async () => {
    const { getTree, readFile, writeFile } = renderFilesPanel();

    fireEvent.click(await screen.findByRole("button", { name: /README\.md/ }));
    expect(await screen.findByRole("heading", { name: "Fixture" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "a link" })).toHaveAttribute(
      "href",
      "https://example.com",
    );
    expect(screen.getByRole("table")).toBeTruthy();
    expect(screen.getByText("const answer = 42;")).toBeTruthy();
    expect(
      screen.queryByRole("textbox", { name: "Contents of README.md" }),
    ).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Edit Markdown" }));
    const editor = await screen.findByRole("textbox", {
      name: "Contents of README.md",
    });
    fireEvent.change(editor, { target: { value: "# Updated fixture\n" } });

    expect(screen.getByText("Unsaved")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => {
      expect(writeFile).toHaveBeenCalledWith(project.id, {
        path: "README.md",
        content: "# Updated fixture\n",
        expectedSha256: version,
        force: false,
      });
    });
    expect(getTree).toHaveBeenCalledWith(project.id, "");
    expect(readFile).toHaveBeenCalledWith(project.id, "README.md");
    expect(screen.queryByText("Unsaved")).toBeNull();
  });

  it("requires an explicit overwrite after an optimistic conflict", async () => {
    const writeFile = vi
      .fn()
      .mockRejectedValueOnce(new ProjectFileConflictError())
      .mockResolvedValueOnce({
        path: "README.md",
        sha256: "b".repeat(64),
      });
    renderFilesPanel({ writeFile });

    fireEvent.click(await screen.findByRole("button", { name: /README\.md/ }));
    await screen.findByRole("heading", { name: "Fixture" });
    fireEvent.click(screen.getByRole("button", { name: "Edit Markdown" }));
    const editor = await screen.findByRole("textbox", {
      name: "Contents of README.md",
    });
    fireEvent.change(editor, { target: { value: "# Local draft\n" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("This file changed on disk.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Overwrite" }));

    await waitFor(() => {
      expect(writeFile).toHaveBeenLastCalledWith(project.id, {
        path: "README.md",
        content: "# Local draft\n",
        expectedSha256: version,
        force: true,
      });
    });
  });
});
