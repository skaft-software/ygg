/// <reference types="vite/client" />

import {
  act,
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { TranscriptSearchResult } from "../protocol";
import { TranscriptSearch } from "./TranscriptSearch";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next) => {
    resolve = next;
  });
  return { promise, resolve };
}

const searchResult: TranscriptSearchResult = {
  truncated: false,
  hits: [
    {
      sessionId: "session-visible",
      itemId: "item-tool",
      kind: "tool",
      sessionTitle: "Build <script>check</script>",
      snippet: "🙂 reconnect <img src=x> safely",
      matchRanges: [{ startChar: 2, endChar: 11 }],
      titleMatchRanges: [{ startChar: 0, endChar: 5 }],
      timestampMs: Date.UTC(2026, 6, 27, 14, 30),
      score: 100,
    },
  ],
};

describe("TranscriptSearch", () => {
  afterEach(cleanup);

  it("submits bounded typed filters and safely highlights scalar ranges", async () => {
    const user = userEvent.setup();
    const pending = deferred<TranscriptSearchResult>();
    const onSearch = vi.fn(() => pending.promise);
    const onActivate = vi.fn();
    const onClose = vi.fn();
    const { container } = render(
      <TranscriptSearch
        open
        onClose={onClose}
        onSearch={onSearch}
        onActivate={onActivate}
      />,
    );

    await user.type(
      screen.getByRole("searchbox", {
        name: "Search conversation transcripts",
      }),
      "  reconnect  ",
    );
    await user.click(screen.getByRole("button", { name: "Tool" }));
    await user.click(screen.getByRole("button", { name: "Search" }));

    expect(onSearch).toHaveBeenCalledWith({
      query: "reconnect",
      filter: { kinds: ["tool"] },
      limit: 100,
    });
    expect(screen.getByRole("status")).toHaveTextContent(
      "Searching conversations",
    );

    await act(async () => {
      pending.resolve(searchResult);
      await pending.promise;
    });

    const result = await screen.findByRole("button", {
      name: "Open Tool result from Build <script>check</script>",
    });
    expect(
      within(result).getByText("reconnect", { selector: "mark" }),
    ).toBeVisible();
    expect(result).toHaveTextContent("🙂 reconnect <img src=x> safely");
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("script")).toBeNull();

    await user.click(result);
    expect(onActivate).toHaveBeenCalledWith(
      "session-visible",
      "item-tool",
    );
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("shows empty and sanitized error states", async () => {
    const user = userEvent.setup();
    const onSearch = vi
      .fn()
      .mockResolvedValueOnce({ hits: [], truncated: false })
      .mockRejectedValueOnce(new Error("internal transport detail"));
    render(
      <TranscriptSearch
        open
        onClose={vi.fn()}
        onSearch={onSearch}
        onActivate={vi.fn()}
      />,
    );

    const input = screen.getByRole("searchbox", {
      name: "Search conversation transcripts",
    });
    await user.type(input, "missing");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(
      await screen.findByText(/No conversations matched/),
    ).toHaveTextContent("missing");

    await user.clear(input);
    await user.type(input, "failure");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Conversation search could not be completed. Try again.",
    );
    expect(screen.queryByText("internal transport detail")).toBeNull();
  });

  it("focuses the query, traps focus, closes on Escape, and restores focus", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    const onSearch = vi.fn().mockResolvedValue({
      hits: [],
      truncated: false,
    });

    function Harness() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setOpen(true)}>
            Open search
          </button>
          <TranscriptSearch
            open={open}
            onClose={() => {
              onClose();
              setOpen(false);
            }}
            onSearch={onSearch}
            onActivate={vi.fn()}
          />
        </>
      );
    }

    render(<Harness />);
    const opener = screen.getByRole("button", { name: "Open search" });
    await user.click(opener);
    const query = screen.getByRole("searchbox", {
      name: "Search conversation transcripts",
    });
    await waitFor(() => expect(query).toHaveFocus());

    const close = screen.getByRole("button", {
      name: "Close conversation search",
    });
    close.focus();
    await user.tab({ shift: true });
    expect(screen.getByRole("button", { name: "Attachment" })).toHaveFocus();
    await user.tab();
    expect(close).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledOnce();
    expect(
      screen.queryByRole("dialog", { name: "Search conversations" }),
    ).toBeNull();
    await waitFor(() => expect(opener).toHaveFocus());
  });
});
