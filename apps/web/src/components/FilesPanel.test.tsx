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
      { name: "src", kind: "directory", size: 0, gitStatus: [
        { kind: "modified" },
        { kind: "added" },
        { kind: "deleted" },
        { kind: "renamed", oldPath: "old-src" },
        { kind: "untracked" },
      ] },
      { name: "README.md", kind: "file", size: 24, gitStatus: [{ kind: "modified" }] },
      { name: "main.ts", kind: "file", size: 24 },
    ],
    truncated: false,
    gitStatusTruncated: false,
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
  const codeRead: ProjectFileRead = {
    path: "main.ts",
    content: "export const answer = 42;\n",
    startLine: 1,
    endLine: 1,
    lineCount: 1,
    truncated: false,
    sha256: version,
  };
  const getTree = vi.fn(async () => tree);
  const readFile = vi.fn(async (_projectId: string, path: string) =>
    path === codeRead.path ? codeRead : read,
  );
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
    expect(screen.getByRole("button", { name: "Copy file" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Download file" })).toBeTruthy();

    const writeText = vi.fn().mockResolvedValue(undefined);
    const originalClipboard = navigator.clipboard;
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    try {
      fireEvent.click(screen.getByRole("button", { name: "Copy file" }));
      await waitFor(() => {
        expect(writeText).toHaveBeenCalledWith(expect.stringContaining("# Fixture"));
      });
    } finally {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: originalClipboard,
      });
    }

    const originalCreateObjectURL = URL.createObjectURL;
    const originalRevokeObjectURL = URL.revokeObjectURL;
    const createObjectURL = vi.fn(() => "blob:fixture");
    const revokeObjectURL = vi.fn();
    const anchorClick = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(() => undefined);
    Object.defineProperty(URL, "createObjectURL", {
      configurable: true,
      value: createObjectURL,
    });
    Object.defineProperty(URL, "revokeObjectURL", {
      configurable: true,
      value: revokeObjectURL,
    });
    try {
      fireEvent.click(screen.getByRole("button", { name: "Download file" }));
      expect(createObjectURL).toHaveBeenCalledWith(expect.any(Blob));
      expect(anchorClick).toHaveBeenCalledOnce();
      await waitFor(() => expect(revokeObjectURL).toHaveBeenCalledWith("blob:fixture"));
    } finally {
      Object.defineProperty(URL, "createObjectURL", {
        configurable: true,
        value: originalCreateObjectURL,
      });
      Object.defineProperty(URL, "revokeObjectURL", {
        configurable: true,
        value: originalRevokeObjectURL,
      });
      anchorClick.mockRestore();
    }

    expect(
      screen.queryByRole("textbox", { name: "Contents of README.md" }),
    ).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Edit Markdown" }));
    const editor = await screen.findByRole("textbox", {
      name: "Contents of README.md",
    });
    expect(document.querySelector(".files-code-editor.is-numbered")).toBeTruthy();
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

  it("shows line numbers for code files", async () => {
    renderFilesPanel();

    fireEvent.click(await screen.findByRole("button", { name: /main\.ts/ }));
    expect(
      await screen.findByRole("textbox", { name: "Contents of main.ts" }),
    ).toBeTruthy();
    expect(document.querySelector(".files-code-editor.is-numbered")).toBeTruthy();
  });

  it("shows accessible Git status indicators for tree entries", async () => {
    renderFilesPanel();

    expect(
      await screen.findByRole("img", {
        name: "Git status: Modified, Added, Deleted, Renamed from old-src, Untracked",
      }),
    ).toBeTruthy();
    expect(screen.getByTitle("Git status: Modified")).toBeTruthy();
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
