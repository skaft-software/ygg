/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap } from "./fixtures";
import { SessionHeader } from "./App";

function renderHeader(sessionExportAvailable: boolean) {
  return render(
    <SessionHeader
      sidebarOpen
      sessionId="session-safe"
      sessionTitle="Safe session"
      projectName="Local project"
      status="idle"
      activityAvailable={false}
      activityOpen={false}
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
      onRename={vi.fn()}
      onPin={vi.fn()}
      onArchive={vi.fn()}
      onOpenBranchHistory={vi.fn()}
    />,
  );
}

describe("session header safe export", () => {
  afterEach(cleanup);

  it("exposes a direct same-origin download only when the host advertises it", async () => {
    expect(fixtureBootstrap.capabilities.sessionExport).toBe(false);
    const user = userEvent.setup();
    const unavailable = renderHeader(false);
    await user.click(screen.getByRole("button", { name: "Session actions" }));
    expect(
      screen.queryByRole("menuitem", { name: "Download safe export" }),
    ).toBeNull();
    unavailable.unmount();

    renderHeader(true);
    await user.click(screen.getByRole("button", { name: "Session actions" }));
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
