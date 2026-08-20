/// <reference types="vite/client" />

import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureSessions } from "../fixtures";
import { ActivityRail } from "./ActivityRail";

describe("activity rail", () => {
  afterEach(cleanup);

  it("explains when a session has no activity", () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    session.items = [];
    session.progress = [];
    session.outputs = [];
    session.sources = [];

    render(
      <ActivityRail
        session={session}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    expect(
      screen.getByText("No activity yet. The agent's work will appear here."),
    ).toBeVisible();
  });

  it("keeps work detail in the transcript while exposing session resources", () => {
    render(
      <ActivityRail
        session={structuredClone(fixtureSessions["session-live"]!)}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    expect(screen.getByText("Progress")).toBeVisible();
    expect(
      screen.getByText("Verifying keyboard and touch behavior"),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: /Onboarding preview/ }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: /OnboardingFlow\.tsx/ }),
    ).toBeVisible();

    expect(screen.queryByText("Activity")).toBeNull();
    expect(screen.queryByText("Read onboarding flow")).toBeNull();
    expect(screen.queryByText("Checking the narrow layout")).toBeNull();
  });

  it("exposes durable redacted command history without inventing output links", () => {
    render(
      <ActivityRail
        session={structuredClone(fixtureSessions["session-done"]!)}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onOpenResource={vi.fn()}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    const heading = screen.getByText("Command history");
    fireEvent.click(heading);
    const section = heading.closest("details");
    expect(section).not.toBeNull();
    expect(
      within(section!).getByText("cargo test --workspace"),
    ).toBeVisible();
    expect(within(section!).getByText(/83[,.]?240ms · exit 0/)).toBeVisible();
    expect(within(section!).queryByRole("button", { name: "Output" })).toBeNull();
  });
  it("renders generic extension activity, detail, and declared actions", async () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    session.outputs[0]!.handle = "resource-handle-1";
    session.extensionPresentations = [
      {
        extension: "ygg-subagents",
        extensionInstanceId: "instance-subagents",
        generation: 3,
        snapshot: {
          revision: 4,
          status: { state: "active", label: "1 worker" },
          activities: [
            {
              id: "worker:1",
              kind: "delegation",
              state: "running",
              summary: "Reviewing tests",
              provenance: "local child",
              references: [
                {
                  kind: "session",
                  id: "session-child",
                  label: "Open worker session",
                },
                {
                  kind: "url",
                  id: "https://example.com/evidence",
                  label: "Evidence",
                },
              ],
            },
          ],
          collection: {
            kind: "tree",
            title: "Workers",
            nodes: [
              {
                id: "worker:1",
                state: "running",
                label: "test-review",
                actionIds: ["stop"],
                references: [
                  {
                    kind: "artifact",
                    id: session.outputs[0]!.id,
                    label: "Worker artifact",
                  },
                  {
                    kind: "resource",
                    id: "resource-handle-1",
                    label: "Worker log",
                  },
                ],
              },
              {
                id: "worker:2",
                state: "succeeded",
                label: "source-review",
                actionIds: ["inspect"],
                references: [
                  {
                    kind: "resource",
                    id: "agent-session:agent-2",
                    label: "Opaque worker session reference",
                  },
                ],
              },
            ],
            selectedNodeId: "worker:1",
            detail: {
              nodeId: "worker:1",
              title: "test-review",
              body: "Running in a bounded child session.",
              references: [],
            },
          },
          actions: [
            {
              id: "stop",
              label: "Stop worker",
              command: "workers",
              arguments: ["stop", "worker:1"],
              destructive: false,
            },
            {
              id: "inspect",
              label: "Inspect worker",
              command: "workers",
              arguments: ["inspect", "worker:2"],
              destructive: false,
            },
          ],
        },
      },
    ];
    const invoke = vi.fn().mockResolvedValue(undefined);
    const openSession = vi.fn();
    const onClose = vi.fn();
    const onOpenOutput = vi.fn();
    const onOpenSource = vi.fn();
    const onOpenResource = vi.fn();
    const onRestoreFocus = vi.fn();
    const rendered = render(
      <ActivityRail
        session={session}
        open
        onClose={onClose}
        onOpenOutput={onOpenOutput}
        onOpenSource={onOpenSource}
        onOpenResource={onOpenResource}
        onOpenSession={openSession}
        onInvokeExtensionAction={invoke}
        modal={false}
        onRestoreFocus={onRestoreFocus}
        resourcesAvailable
      />,
    );

    expect(screen.getByText("Reviewing tests")).toBeVisible();
    expect(
      screen.getByText("Running in a bounded child session."),
    ).toBeVisible();
    expect(screen.getByRole("link", { name: "Evidence" })).toHaveAttribute(
      "href",
      "https://example.com/evidence",
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Open worker session" }),
    );
    expect(openSession).toHaveBeenCalledWith("session-child");
    fireEvent.click(screen.getByRole("button", { name: "Worker artifact" }));
    expect(onOpenOutput).toHaveBeenCalledWith(session.outputs[0]!.id);
    fireEvent.click(screen.getByRole("button", { name: "Worker log" }));
    expect(onOpenResource).toHaveBeenCalledWith(
      "resource-handle-1",
      "Worker log",
      "text",
    );
    const firstNode = screen.getByRole("button", { name: "Stop worker" }).closest("li");
    const secondNode = screen.getByRole("button", { name: "Inspect worker" }).closest("li");
    expect(firstNode).not.toBeNull();
    expect(secondNode).not.toBeNull();
    expect(within(firstNode!).getByRole("button", { name: "Stop worker" })).toBeVisible();
    expect(within(firstNode!).queryByRole("button", { name: "Inspect worker" })).toBeNull();
    expect(within(secondNode!).getByRole("button", { name: "Inspect worker" })).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Opaque worker session reference" }),
    ).toBeNull();
    expect(screen.getByText(/Opaque worker session reference:/)).toBeVisible();
    expect(screen.getAllByRole("button", { name: "Stop worker" })).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Stop worker" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "ygg-subagents",
        "instance-subagents",
        3,
        4,
        "stop",
        false,
      ),
    );

    const updated = structuredClone(session);
    updated.extensionPresentations![0]!.snapshot.revision = 5;
    updated.extensionPresentations![0]!.snapshot.status = {
      state: "active",
      label: "2 workers",
    };
    rendered.rerender(
      <ActivityRail
        session={updated}
        open
        onClose={onClose}
        onOpenOutput={onOpenOutput}
        onOpenSource={onOpenSource}
        onOpenResource={onOpenResource}
        onOpenSession={openSession}
        onInvokeExtensionAction={invoke}
        modal={false}
        onRestoreFocus={onRestoreFocus}
        resourcesAvailable
      />,
    );
    expect(screen.getByText("2 workers")).toBeVisible();
  });
  it("requires a deliberate second click for destructive extension actions", async () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    session.extensionPresentations = [
      {
        extension: "ygg-browse",
        extensionInstanceId: "instance-browse",
        generation: 1,
        snapshot: {
          revision: 1,
          activities: [],
          actions: [
            {
              id: "reset",
              label: "Reset profile",
              command: "browse",
              arguments: ["reset-profile"],
              destructive: true,
            },
          ],
        },
      },
    ];
    const invoke = vi.fn().mockResolvedValue(undefined);
    render(
      <ActivityRail
        session={session}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onInvokeExtensionAction={invoke}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Reset profile" }));
    expect(invoke).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm Reset profile" }),
    );
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "ygg-browse",
        "instance-browse",
        1,
        1,
        "reset",
        true,
      ),
    );
  });
});
