/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureBootstrap, fixtureSessions } from "../fixtures";
import { Conversation } from "./Conversation";

const noOp = async () => {};

describe("conversation composer", () => {
  afterEach(cleanup);

  it("keeps the draft and surfaces a rejected send", async () => {
    const user = userEvent.setup();
    const onSubmit = vi
      .fn()
      .mockRejectedValue(new Error("The host rejected this command."));
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "Keep this draft");
    await user.click(screen.getByRole("button", { name: "Send message" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The host rejected this command.",
    );
    expect(composer).toHaveValue("Keep this draft");
  });

  it("offers Follow up and Steer alongside Stop during an active run", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-live"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(screen.getByRole("button", { name: "Stop ygg" })).toBeVisible();
    const delivery = screen.getByLabelText("Active run delivery");
    expect(delivery).toHaveValue("followUp");
    expect(delivery).toHaveTextContent("Steer now");
    await user.type(screen.getByLabelText("Message ygg"), "Queue this");
    await user.click(screen.getByRole("button", { name: "Queue follow-up" }));

    expect(onSubmit).toHaveBeenCalledWith("Queue this", [], "followUp");
  });

  it("omits unavailable run timing instead of claiming zero work", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "run-outcome",
      turnId: "turn-outcome",
      kind: "run_outcome",
      outcome: "done",
      durationMs: 0,
      summary: "Run completed",
      state: "committed",
      createdAt: new Date().toISOString(),
    });
    render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(screen.getByText("Run completed")).toBeVisible();
    expect(screen.queryByText(/Worked for/)).toBeNull();
  });

  it("retains a failed image upload with retry and remove controls", async () => {
    const user = userEvent.setup();
    const onIngestAttachment = vi
      .fn()
      .mockRejectedValueOnce(new Error("Upload failed"))
      .mockResolvedValue({
        id: "image-handle",
        handle: "image-handle",
        name: "photo.png",
        mediaType: "image/png",
        size: 4,
      });
    const { container } = render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
        onIngestAttachment={onIngestAttachment}
        attachmentContentUrl={(handle) => `/api/v1/attachments/${handle}`}
      />,
    );

    const picker = container.querySelector<HTMLInputElement>(
      'input[type="file"]',
    );
    expect(picker).not.toBeNull();
    await user.upload(
      picker!,
      new File(["tiny"], "photo.png", { type: "image/png" }),
    );

    const retry = await screen.findByRole("button", {
      name: "Retry photo.png",
    });
    expect(screen.getByText("Upload failed")).toBeVisible();
    await user.click(retry);
    expect(await screen.findByText("Ready")).toBeVisible();
    await user.click(
      screen.getByRole("button", { name: "Remove photo.png" }),
    );
    expect(
      screen.queryByRole("button", { name: "Remove photo.png" }),
    ).toBeNull();
  });
});
