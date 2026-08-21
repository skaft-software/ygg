/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap, fixtureSessions } from "./fixtures";

vi.mock("@xterm/xterm", () => ({ Terminal: class {} }));

import { SessionHeader, SessionSelectionErrorBanner } from "./App";
import { resolveDelegatedParentSessionId } from "./delegated-session";

function renderHeader(
  sessionExportAvailable: boolean,
  terminalAvailable = false,
  terminalOpen = false,
  sessionTitle = "Safe session",
) {
  const onToggleTerminal = vi.fn();
  const onRename = vi.fn();
  return {
    onRename,
    onToggleTerminal,
    ...render(
      <SessionHeader
        sidebarOpen
        sessionId="session-safe"
        sessionTitle={sessionTitle}
        projectName="Local project"
        status="idle"
        activityAvailable={false}
        activityOpen={false}
        terminalAvailable={terminalAvailable}
        terminalOpen={terminalOpen}
        pinned={false}
        archived={false}
        sessionActionsAvailable
        metadataActionsAvailable
        branchHistoryAvailable={false}
        sessionExportAvailable={sessionExportAvailable}
        activityButtonRef={createRef<HTMLButtonElement>()}
        sidebarButtonRef={createRef<HTMLButtonElement>()}
        onOpenSidebar={vi.fn()}
        onToggleActivity={vi.fn()}
        onToggleTerminal={onToggleTerminal}
        onRename={onRename}
        onPin={vi.fn()}
        onArchive={vi.fn()}
        onOpenBranchHistory={vi.fn()}
      />,
    ),
  };
}

describe("session header safe export", () => {
  afterEach(cleanup);

  it("shows the terminal control only when the host advertises it", async () => {
    const user = userEvent.setup();
    const unavailable = renderHeader(false);
    expect(
      screen.queryByRole("button", { name: "Open terminal" }),
    ).toBeNull();
    unavailable.unmount();

    const available = renderHeader(false, true, false);
    const terminal = screen.getByRole("button", { name: "Open terminal" });
    expect(terminal).toHaveAttribute("aria-pressed", "false");
    await user.click(terminal);
    expect(available.onToggleTerminal).toHaveBeenCalledOnce();
  });

  it("keeps a provisional session title internal until the user renames the task", async () => {
    const user = userEvent.setup();
    const header = renderHeader(false, false, false, "New session");

    expect(screen.getByText("New task", { exact: true })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Task actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Rename" }));
    expect(screen.getByRole("textbox", { name: "Task title" })).toHaveValue(
      "New task",
    );
    await user.keyboard("{Enter}");
    expect(header.onRename).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Task actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Rename" }));
    const title = screen.getByRole("textbox", { name: "Task title" });
    await user.clear(title);
    await user.type(title, "Release prep");
    await user.keyboard("{Enter}");
    expect(header.onRename).toHaveBeenCalledWith("Release prep");
  });

  it("exposes a direct same-origin download only when the host advertises it", async () => {
    expect(fixtureBootstrap.capabilities.sessionExport).toBe(false);
    const user = userEvent.setup();
    const unavailable = renderHeader(false);
    await user.click(screen.getByRole("button", { name: "Task actions" }));
    expect(
      screen.queryByRole("menuitem", { name: "Download safe export" }),
    ).toBeNull();
    unavailable.unmount();

    renderHeader(true);
    await user.click(screen.getByRole("button", { name: "Task actions" }));
    const download = screen.getByRole("menuitem", {
      name: "Download safe export",
    });
    expect(download).toHaveAttribute(
      "href",
      "/api/v1/sessions/session-safe/export",
    );
    expect(download).toHaveAttribute("download");
    expect(download.tagName).toBe("A");
  });
});

describe("delegated parent navigation", () => {
  it("prefers authoritative snapshot metadata over stale inferred history", () => {
    const delegated = {
      ...fixtureSessions["session-fresh"],
      sessionId: `agent-session:${"a".repeat(64)}`,
      delegatedParentSessionId: "parent-two",
    };

    expect(resolveDelegatedParentSessionId(delegated, "parent-one")).toBe(
      "parent-two",
    );
    expect(
      resolveDelegatedParentSessionId(
        { ...delegated, delegatedParentSessionId: undefined },
        "parent-one",
      ),
    ).toBe("parent-one");
    expect(
      resolveDelegatedParentSessionId(
        { ...delegated, sessionId: "ordinary-session" },
        "parent-one",
      ),
    ).toBeNull();
  });
});

describe("session selection errors", () => {
  afterEach(cleanup);

  it("surfaces the failure and offers a retry", async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    render(
      <SessionSelectionErrorBanner
        message="Session failed with 500"
        onRetry={onRetry}
      />,
    );

    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("Could not open that session.");
    expect(alert).toHaveTextContent("Session failed with 500");
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });
});
