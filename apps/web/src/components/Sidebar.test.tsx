/// <reference types="vite/client" />

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap } from "../fixtures";
import type { SessionSummary } from "../protocol";
import { Sidebar } from "./Sidebar";

const baseSession = fixtureBootstrap.sessions[0]!;

function activeSession(
  overrides: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    ...structuredClone(baseSession),
    id: "session-active",
    title: "Active investigation",
    archived: false,
    lifecycle: "active",
    retention: undefined,
    pinned: false,
    ...overrides,
  };
}

function archivedSession(
  overrides: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    ...activeSession(),
    id: "session-archived",
    title: "Archived investigation",
    archived: true,
    lifecycle: "archived",
    ...overrides,
  };
}

function trashedSession(
  overrides: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    ...activeSession(),
    id: "session-trashed",
    title: "Deleted investigation",
    archived: true,
    lifecycle: "trash",
    retention: {
      trashedAtMs: Date.UTC(2030, 0, 1, 12),
      purgeAfterMs: Date.UTC(2030, 1, 1, 12),
      permanentDeleteRequiresConfirmation: true,
    },
    ...overrides,
  };
}

function sidebarProps(
  overrides: Partial<ComponentProps<typeof Sidebar>> = {},
): ComponentProps<typeof Sidebar> {
  return {
    open: true,
    blocked: false,
    sessions: [activeSession(), archivedSession(), trashedSession()],
    projects: fixtureBootstrap.projects,
    selectedSessionId: "session-active",
    surface: "session",
    devicesAvailable: false,
    sessionTrashAvailable: true,
    onRestoreFocus: vi.fn(),
    onClose: vi.fn(),
    onNewSession: vi.fn(),
    onSelectSession: vi.fn(),
    onRestoreSession: vi.fn(),
    onSetSessionLifecycle: vi.fn(),
    onDeleteSessionPermanently: vi.fn(),
    onOpenProjects: vi.fn(),
    onOpenDevices: vi.fn(),
    onOpenSettings: vi.fn(),
    ...overrides,
  };
}

describe("sidebar session lifecycle", () => {
  beforeEach(() => {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: vi.fn().mockReturnValue({
        matches: false,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      }),
    });
  });
  afterEach(cleanup);

  it("shows title-only rows and gates pull-request marks on structured evidence", () => {
    const sessions = [
      activeSession({ id: "session-none", title: "No PR evidence" }),
      activeSession({
        id: "session-progress",
        title: "Draft review",
        pullRequest: { state: "in_progress" },
      }),
      activeSession({
        id: "session-ready",
        title: "Ready review",
        pullRequest: { state: "ready" },
      }),
      activeSession({
        id: "session-merged",
        title: "Merged review",
        pullRequest: { state: "merged" },
      }),
    ];

    const { container } = render(
      <Sidebar {...sidebarProps({ sessions, selectedSessionId: "session-none" })} />,
    );

    const noEvidence = screen.getByRole("button", {
      name: "Open session No PR evidence, Ready",
    });
    expect(noEvidence).toHaveTextContent("No PR evidence");
    expect(noEvidence.querySelector("svg")).toBeNull();
    expect(screen.getByTitle("Pull request in progress")).toBeVisible();
    expect(screen.getByTitle("Pull request ready for review")).toBeVisible();
    expect(screen.getByTitle("Pull request merged")).toBeVisible();
    expect(container.querySelectorAll(".session-pull-request")).toHaveLength(3);
    expect(screen.queryByText(/GPT|Claude|Qwen/)).toBeNull();
  });

  it("browses active and archived sessions and restores through lifecycle active", () => {
    const setLifecycle = vi.fn();

    render(
      <Sidebar
        {...sidebarProps({ onSetSessionLifecycle: setLifecycle })}
      />,
    );

    expect(
      screen.getByRole("tab", { name: "Active" }),
    ).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("Active investigation")).toBeVisible();
    expect(screen.queryByText("Archived investigation")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Archive/ }));
    expect(screen.getByText("Archived investigation")).toBeVisible();
    expect(screen.queryByText("Active investigation")).toBeNull();

    fireEvent.click(
      screen.getByRole("button", {
        name: "Restore session Archived investigation",
      }),
    );
    expect(setLifecycle).toHaveBeenCalledWith("session-archived", "active");
  });

  it("preserves the existing archive restore callback as a compatibility fallback", () => {
    const restore = vi.fn();

    render(
      <Sidebar
        {...sidebarProps({
          sessions: [archivedSession()],
          sessionTrashAvailable: false,
          onSetSessionLifecycle: undefined,
          onDeleteSessionPermanently: undefined,
          onRestoreSession: restore,
        })}
      />,
    );

    fireEvent.click(screen.getByRole("tab", { name: /Archive/ }));
    fireEvent.click(
      screen.getByRole("button", {
        name: "Restore session Archived investigation",
      }),
    );
    expect(restore).toHaveBeenCalledWith("session-archived");
  });

  it("moves an archived session to trash only through the lifecycle callback", () => {
    const setLifecycle = vi.fn();

    render(
      <Sidebar
        {...sidebarProps({ onSetSessionLifecycle: setLifecycle })}
      />,
    );

    fireEvent.click(screen.getByRole("tab", { name: /Archive/ }));
    fireEvent.click(
      screen.getByRole("button", {
        name: "Move session Archived investigation to trash",
      }),
    );
    expect(setLifecycle).toHaveBeenCalledWith("session-archived", "trash");
  });

  it("restores trash and exposes its host-owned purge timing", () => {
    const setLifecycle = vi.fn();
    const session = trashedSession();

    const { container } = render(
      <Sidebar
        {...sidebarProps({
          sessions: [session],
          onSetSessionLifecycle: setLifecycle,
        })}
      />,
    );

    fireEvent.click(screen.getByRole("tab", { name: /Trash/ }));
    expect(screen.getByText("Deleted investigation")).toBeVisible();
    expect(screen.getByText(/Automatic purge/)).toBeVisible();
    expect(
      container.querySelector(
        `time[datetime="${new Date(session.retention!.purgeAfterMs).toISOString()}"]`,
      ),
    ).not.toBeNull();

    fireEvent.click(
      screen.getByRole("button", {
        name: "Restore session Deleted investigation",
      }),
    );
    expect(setLifecycle).toHaveBeenCalledWith("session-trashed", "active");
  });

  it("requires the exact retention-bound phrase before permanent deletion", () => {
    const deletePermanently = vi.fn();
    const session = trashedSession();

    render(
      <Sidebar
        {...sidebarProps({
          sessions: [session],
          onDeleteSessionPermanently: deletePermanently,
        })}
      />,
    );

    fireEvent.click(screen.getByRole("tab", { name: /Trash/ }));
    fireEvent.click(
      screen.getByRole("button", {
        name: "Permanently delete session Deleted investigation",
      }),
    );

    const confirm = screen.getByRole("button", {
      name: "Delete permanently",
    });
    const input = screen.getByRole("textbox", {
      name: "Confirmation phrase for Deleted investigation",
    });
    expect(screen.getByText("permanently delete session-trashed")).toBeVisible();
    expect(confirm).toBeDisabled();

    fireEvent.change(input, {
      target: { value: "permanently delete a different session" },
    });
    expect(confirm).toBeDisabled();
    fireEvent.click(confirm);
    expect(deletePermanently).not.toHaveBeenCalled();

    const phrase = "permanently delete session-trashed";
    fireEvent.change(input, { target: { value: phrase } });
    expect(confirm).toBeEnabled();
    fireEvent.click(confirm);
    expect(deletePermanently).toHaveBeenCalledWith(
      "session-trashed",
      session.retention!.trashedAtMs,
      phrase,
    );
  });
});
