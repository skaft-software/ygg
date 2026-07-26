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
        onResolveUserInput={noOp}
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
        onResolveUserInput={noOp}
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

  it("uses a themed searchable model picker instead of a native select", async () => {
    const user = userEvent.setup();
    const onConfigure = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={onConfigure}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    const picker = screen.getByRole("button", { name: "Model" });
    expect(picker).toHaveAttribute("data-value", "claude-sonnet-4-6");
    expect(screen.queryByRole("combobox", { name: "Model" })).toBeNull();

    await user.click(picker);
    expect(
      screen.getByRole("dialog", { name: "Choose a model" }),
    ).toBeVisible();
    await user.type(screen.getByRole("textbox", { name: "Search models" }), "gpt");
    await user.click(screen.getByRole("option", { name: /GPT-5.4/ }));

    expect(onConfigure).toHaveBeenCalledWith({ modelId: "gpt-5.4" });
    expect(
      screen.queryByRole("dialog", { name: "Choose a model" }),
    ).toBeNull();
    expect(picker).toHaveFocus();
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
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(screen.getByText("Run completed")).toBeVisible();
    expect(screen.queryByText(/Worked for/)).toBeNull();
  });

  it("renders assistant markdown as readable, safe rich content", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "assistant-markdown",
      turnId: "turn-markdown",
      kind: "assistant_message",
      content:
        "## Result\n\n- One\n- Two\n\n```ts\nconst answer = 42;\n```\n\n<script>alert('no')</script>",
      state: "committed",
      createdAt: new Date().toISOString(),
    });
    const { container } = render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "Result" }),
    ).toBeVisible();
    expect(screen.getByRole("list")).toHaveTextContent(/One\s+Two/);
    expect(screen.getByRole("button", { name: "Copy code" })).toBeVisible();
    expect(container.querySelector("script")).toBeNull();
    expect(screen.getByText("<script>alert('no')</script>")).toBeVisible();
  });

  it("answers a private tool-input request without adding it to prose", async () => {
    const user = userEvent.setup();
    const onResolveUserInput = vi.fn().mockResolvedValue(undefined);
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.status = "needs_attention";
    session.activeRunId = "run-private-input";
    session.items.push({
      id: "request-private-input",
      turnId: "turn-private-input",
      kind: "user_input_request",
      requestId: "private-input",
      prompt: "Enter the deployment token",
      choices: [],
      state: "streaming",
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
        onResolveUserInput={onResolveUserInput}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    const field = screen.getByLabelText("Private answer");
    expect(field).toHaveAttribute("type", "password");
    await user.type(field, "secret-value");
    await user.click(screen.getByRole("button", { name: "Send securely" }));
    expect(onResolveUserInput).toHaveBeenCalledWith("private-input", {
      type: "text",
      text: "secret-value",
    });
    expect(field).toHaveValue("");
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
        onResolveUserInput={noOp}
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
