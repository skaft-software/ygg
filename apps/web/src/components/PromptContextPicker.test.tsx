/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  DocumentReference,
  TrustedFileEntry,
} from "../protocol";
import { PromptContextPicker } from "./PromptContextPicker";

const projectFile: TrustedFileEntry = {
  id: "file_11111111111111111111111111111111",
  relativePath: "docs/release.md",
  displayName: "release.md",
  kind: "documentation",
  byteLen: 128,
};

describe("PromptContextPicker", () => {
  afterEach(cleanup);

  it("uploads documents, selects trusted files, searches, and previews text", async () => {
    const user = userEvent.setup();
    const uploadDocument = vi.fn(
      async (file: File): Promise<DocumentReference> => ({
        id: "doc_22222222222222222222222222222222",
        displayName: file.name,
        mediaType: "text/markdown",
        sourceByteCount: file.size,
        extractedTextByteCount: file.size,
        sha256: "a".repeat(64),
        fidelity: "exactUtf8",
        createdAtMs: 1_753_626_615_000,
      }),
    );
    const listProjectFiles = vi.fn().mockResolvedValue({
      files: [projectFile],
      summary: {
        indexedFiles: 1,
        ignoredEntries: 2,
        truncated: false,
      },
    });
    const searchProjectFiles = vi.fn().mockResolvedValue({
      hits: [
        {
          entry: {
            ...projectFile,
            id: "file_33333333333333333333333333333333",
            relativePath: "src/release.ts",
            displayName: "release.ts",
            kind: "source",
          },
          snippet: "export const release = true;",
          line: 4,
        },
      ],
      truncated: false,
      scannedBytes: 128,
    });
    const readProjectFile = vi.fn().mockResolvedValue({
      entry: projectFile,
      text: "# Release\nSafe preview text.",
      sha256: "b".repeat(64),
    });

    function Harness() {
      const [documents, setDocuments] = useState<DocumentReference[]>([]);
      const [projectFiles, setProjectFiles] = useState<TrustedFileEntry[]>([]);
      return (
        <PromptContextPicker
          documents={documents}
          projectFiles={projectFiles}
          documentsAvailable
          projectFilesAvailable
          onUploadDocument={async (file) => {
            const document = await uploadDocument(file);
            setDocuments((current) => [...current, document]);
            return document;
          }}
          onRemoveDocument={(documentId) =>
            setDocuments((current) =>
              current.filter((document) => document.id !== documentId),
            )
          }
          onToggleProjectFile={(file) =>
            setProjectFiles((current) =>
              current.some((selected) => selected.id === file.id)
                ? current.filter((selected) => selected.id !== file.id)
                : [...current, file],
            )
          }
          onListProjectFiles={listProjectFiles}
          onSearchProjectFiles={searchProjectFiles}
          onReadProjectFile={readProjectFile}
        />
      );
    }

    const { container } = render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Context" }));
    expect(
      await screen.findByRole("dialog", { name: "Add prompt context" }),
    ).toBeVisible();
    expect(listProjectFiles).toHaveBeenCalledOnce();
    expect(await screen.findByText("docs/release.md")).toBeVisible();

    const input = container.querySelector<HTMLInputElement>(
      'input[type="file"]',
    );
    expect(input).not.toBeNull();
    const upload = new File(["# Notes\n"], "notes.md", {
      type: "text/markdown",
    });
    await user.upload(input!, upload);
    expect(uploadDocument).toHaveBeenCalledWith(upload);
    expect(await screen.findByText("notes.md")).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Remove notes.md" }),
    ).toBeVisible();

    const fileChoice = screen.getByText("docs/release.md").closest("button");
    expect(fileChoice).not.toBeNull();
    await user.click(fileChoice!);
    expect(fileChoice).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: /Context 2/ })).toBeVisible();

    await user.click(
      screen.getByRole("button", { name: "Preview docs/release.md" }),
    );
    expect(readProjectFile).toHaveBeenCalledWith(projectFile.id);
    expect(await screen.findByText(/Safe preview text/)).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Close file preview" }),
    ).toBeVisible();

    await user.type(
      screen.getByRole("textbox", { name: "Search trusted project files" }),
      "  release  ",
    );
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(searchProjectFiles).toHaveBeenCalledWith("release");
    expect(await screen.findByText("src/release.ts")).toBeVisible();
  });
});
