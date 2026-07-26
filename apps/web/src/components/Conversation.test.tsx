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

    expect(screen.getByRole("button", { name: "Stop Ygg" })).toBeVisible();
    const delivery = screen.getByLabelText("Active run delivery");
    expect(delivery).toHaveValue("followUp");
    expect(delivery).toHaveTextContent("Steer now");
    await user.type(screen.getByLabelText("Message ygg"), "Queue this");
    await user.click(screen.getByRole("button", { name: "Queue follow-up" }));

    expect(onSubmit).toHaveBeenCalledWith("Queue this", [], "followUp");
  });
});
