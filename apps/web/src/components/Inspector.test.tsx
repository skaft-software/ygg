/// <reference types="vite/client" />

import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureSessions } from "../fixtures";
import { Inspector } from "./Inspector";

describe("resource inspector", () => {
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("fetches a session-scoped diff and renders it as inert text", async () => {
    const diff =
      "--- a/src/theme.ts\n+++ b/src/theme.ts\n@@ -1 +1 @@\n-old\n+new\n";
    const fetchMock = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(diff, {
        status: 200,
        headers: {
          "Content-Type": "text/plain",
          "Content-Length": String(new TextEncoder().encode(diff).byteLength),
        },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const resourceContentUrl = vi
      .fn()
      .mockReturnValue(
        "/api/v1/sessions/session-live/resources/resource-diff",
      );

    render(
      <Inspector
        session={structuredClone(fixtureSessions["session-live"]!)}
        selection={{
          type: "resource",
          handle: "resource-diff",
          title: "src/theme.ts changes",
          presentation: "diff",
        }}
        closing={false}
        modal={false}
        previewsAvailable={false}
        resourceContentUrl={resourceContentUrl}
        onRestoreFocus={() => {}}
        onClose={() => {}}
      />,
    );

    expect(resourceContentUrl).toHaveBeenCalledWith(
      "session-live",
      "resource-diff",
    );
    expect(await screen.findByText("+new")).toHaveClass("is-addition");
    expect(screen.getByText("-old")).toHaveClass("is-deletion");
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/sessions/session-live/resources/resource-diff",
      expect.objectContaining({
        credentials: "same-origin",
        cache: "no-store",
        redirect: "error",
      }),
    );
    expect(document.querySelector(".opaque-resource iframe")).toBeNull();
  });

  it("turns a gone resource into a bounded user-facing state", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        new Response("", { status: 410 }),
      ),
    );

    render(
      <Inspector
        session={structuredClone(fixtureSessions["session-live"]!)}
        selection={{
          type: "resource",
          handle: "resource-gone",
          title: "src/theme.ts changes",
          presentation: "diff",
        }}
        closing={false}
        modal={false}
        previewsAvailable={false}
        resourceContentUrl={(sessionId, handle) =>
          `/api/v1/sessions/${sessionId}/resources/${handle}`
        }
        onRestoreFocus={() => {}}
        onClose={() => {}}
      />,
    );

    await waitFor(() =>
      expect(
        screen.getByText("This resource is no longer available."),
      ).toBeVisible(),
    );
  });

  it("switches between unified and split diffs with bounded hunk navigation", async () => {
    const diff = [
      "--- a/src/theme.ts",
      "+++ b/src/theme.ts",
      "@@ -1,2 +1,2 @@",
      "-old first",
      "+new first",
      " shared",
      "@@ -10 +10 @@",
      "-old second",
      "+new second",
      "",
    ].join("\n");
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        new Response(diff, {
          status: 200,
          headers: {
            "Content-Length": String(
              new TextEncoder().encode(diff).byteLength,
            ),
          },
        }),
      ),
    );

    render(
      <Inspector
        session={structuredClone(fixtureSessions["session-live"]!)}
        selection={{
          type: "resource",
          handle: "resource-two-hunks",
          title: "theme changes",
          presentation: "diff",
        }}
        closing={false}
        modal={false}
        previewsAvailable={false}
        resourceContentUrl={(sessionId, handle) =>
          `/api/v1/sessions/${sessionId}/resources/${handle}`
        }
        onRestoreFocus={() => {}}
        onClose={() => {}}
      />,
    );

    expect(await screen.findByText("Change 1 of 2")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Split" }));
    expect(screen.getByRole("button", { name: "Split" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByText("old first")).toHaveClass("is-deletion");
    expect(screen.getByText("new first")).toHaveClass("is-addition");

    fireEvent.click(screen.getByRole("button", { name: "Next change" }));
    expect(screen.getByText("Change 2 of 2")).toBeVisible();
    expect(screen.getByRole("button", { name: "Next change" })).toBeDisabled();
    expect(
      screen.getByRole("button", { name: "Previous change" }),
    ).toBeEnabled();
  });

  it("bounds pathological tiny-line diffs without hiding the download", async () => {
    const diff = `${"+\n".repeat(5_001)}tail`;
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        new Response(diff, {
          status: 200,
          headers: {
            "Content-Length": String(
              new TextEncoder().encode(diff).byteLength,
            ),
          },
        }),
      ),
    );

    render(
      <Inspector
        session={structuredClone(fixtureSessions["session-live"]!)}
        selection={{
          type: "resource",
          handle: "resource-huge-line-count",
          title: "many-lines.diff",
          presentation: "diff",
        }}
        closing={false}
        modal={false}
        previewsAvailable={false}
        resourceContentUrl={(sessionId, handle) =>
          `/api/v1/sessions/${sessionId}/resources/${handle}`
        }
        onRestoreFocus={() => {}}
        onClose={() => {}}
      />,
    );

    expect(
      await screen.findByText(
        "Preview limited to the first 5,000 lines. Download the diff to inspect the rest.",
      ),
    ).toBeVisible();
    expect(
      document.querySelectorAll(".opaque-resource-text.is-diff > span"),
    ).toHaveLength(5_000);
    expect(
      screen.getByRole("link", { name: "Download many-lines.diff" }),
    ).toBeVisible();
    expect(screen.queryByText("tail")).toBeNull();
  });
});
