/// <reference types="vite/client" />

import { cleanup, render, screen, waitFor } from "@testing-library/react";
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

  it("queues by default and keeps Steer in a themed secondary menu", async () => {
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
    const delivery = screen.getByRole("button", {
      name: "While ygg is working: Follow up",
    });
    expect(
      screen.queryByRole("combobox", { name: "Active run delivery" }),
    ).toBeNull();
    await user.type(screen.getByLabelText("Message ygg"), "Queue this");
    await user.click(screen.getByRole("button", { name: "Queue follow-up" }));

    expect(onSubmit).toHaveBeenCalledWith("Queue this", [], "followUp");

    await user.click(delivery);
    expect(
      screen.getByRole("menu", { name: "While ygg is working" }),
    ).toBeVisible();
    expect(
      screen.getByRole("menuitemradio", { name: /Steer now/ }),
    ).toBeVisible();
    await user.keyboard("{Escape}");
    expect(
      screen.queryByRole("menu", { name: "While ygg is working" }),
    ).toBeNull();
    await waitFor(() => expect(delivery).toHaveFocus());
  });

  it("uses a themed authority menu with keyboard focus restoration", async () => {
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

    const authority = screen.getByRole("button", {
      name: "Authority: Full access",
    });
    expect(
      screen.queryByRole("combobox", { name: "Authority" }),
    ).toBeNull();
    await user.click(authority);
    expect(screen.getByRole("menu", { name: "Authority" })).toBeVisible();
    await user.click(screen.getByLabelText("Message ygg"));
    expect(screen.queryByRole("menu", { name: "Authority" })).toBeNull();
    await user.click(authority);
    await user.click(
      screen.getByRole("menuitemradio", { name: /Workspace/ }),
    );

    expect(onConfigure).toHaveBeenCalledWith({ authority: "workspace" });
    await waitFor(() => expect(authority).toHaveFocus());
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

    const picker = screen.getByRole("button", {
      name: /Model and effort: Claude Sonnet 4\.6, Max/,
    });
    expect(picker).toHaveAttribute("data-value", "claude-sonnet-4-6");
    expect(screen.queryByRole("combobox", { name: "Model" })).toBeNull();

    await user.click(picker);
    expect(
      screen.getByRole("dialog", { name: "Model and effort" }),
    ).toBeVisible();
    expect(
      screen.getByRole("slider", { name: "Reasoning effort" }),
    ).toHaveAttribute("aria-valuetext", "Max");
    expect(screen.queryByText("Speed", { exact: true })).toBeNull();
    expect(screen.queryByText("Ultra", { exact: true })).toBeNull();
    await user.click(screen.getByRole("button", { name: /Advanced/ }));
    await user.click(
      screen.getByRole("dialog", { name: "Model and effort" }).querySelector(
        ".model-picker-setting-row",
      )!,
    );
    await user.type(screen.getByRole("textbox", { name: "Search models" }), "gpt");
    await user.click(screen.getByRole("option", { name: /GPT-5.4/ }));

    expect(onConfigure).toHaveBeenCalledWith({ modelId: "gpt-5.4" });
    expect(
      screen.getByRole("button", { name: /^Model Claude Sonnet 4\.6/ }),
    ).toBeVisible();
    expect(screen.queryByText("Speed", { exact: true })).toBeNull();
    await user.click(
      screen.getByRole("button", { name: /^Effort High/ }),
    );
    await user.click(screen.getByRole("option", { name: "Low" }));
    expect(onConfigure).toHaveBeenCalledWith({ reasoning: "low" });
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

  it("copies a completed assistant response and confirms it quietly", async () => {
    const user = userEvent.setup();
    const writeText = vi
      .spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue(undefined);
    const session = structuredClone(fixtureSessions["session-recent"]!);
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

    const copy = await screen.findByRole("button", {
      name: "Copy response",
    });
    await user.click(copy);

    const answer = session.items.find(
      (item) => item.kind === "assistant_message",
    );
    expect(writeText).toHaveBeenCalledWith(
      answer?.kind === "assistant_message" ? answer.content : "",
    );
    expect(
      screen.getByRole("button", { name: "Response copied" }),
    ).toBeVisible();
  });

  it("smoothly collapses completed work and lets the user reopen it", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-live"]!);
    const props = {
      bootstrap: structuredClone(fixtureBootstrap),
      onSubmit: noOp,
      onInterrupt: noOp,
      onConfigure: noOp,
      onResolveApproval: noOp,
      onResolveUserInput: noOp,
      onOpenOutput: () => {},
      onOpenSource: () => {},
    };
    const { rerender, container } = render(
      <Conversation session={session} {...props} />,
    );

    const workingSummary = screen.getByRole("button", { name: /Working/ });
    expect(workingSummary).toHaveAttribute("aria-expanded", "true");
    expect(workingSummary.querySelector(".work-group-glyph")).toBeNull();

    const completed = structuredClone(session);
    completed.status = "done";
    completed.activeRunId = undefined;
    completed.items = completed.items.map((item) =>
      item.kind === "reasoning"
        ? { ...item, state: "committed" as const }
        : item,
    );
    completed.items.push({
      id: "live-outcome",
      turnId: "live-turn",
      kind: "run_outcome",
      outcome: "done",
      durationMs: 4_500,
      summary: "Work completed",
      state: "committed",
      createdAt: new Date().toISOString(),
    });
    rerender(<Conversation session={completed} {...props} />);

    const summary = screen.getByRole("button", { name: /Worked for/ });
    expect(summary).toHaveAttribute("aria-expanded", "false");
    expect(
      container.querySelector(".work-group-content-clip"),
    ).toHaveAttribute("aria-hidden", "true");
    await user.click(summary);
    expect(summary).toHaveAttribute("aria-expanded", "true");
  });

  it("opens durable diffs, resulting files, and origin-linked evidence", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const timestamp = new Date().toISOString();
    session.status = "working";
    session.activeRunId = "run-evidence";
    session.items = [
      {
        id: "item-tool",
        turnId: "turn-evidence",
        kind: "action",
        actionKind: "file_write",
        label: "Changed file",
        target: "src/theme.ts",
        additions: 8,
        deletions: 3,
        diffHandle: "resource-diff",
        resultHandle: "resource-result",
        state: "streaming",
        createdAt: timestamp,
      },
    ];
    session.sources = [
      {
        id: "source-theme",
        handle: "resource-source",
        originItemId: "item-tool",
        kind: "file",
        title: "theme.ts",
        subtitle: "Consulted now",
        consultedAt: timestamp,
        iconLabel: "SRC",
      },
    ];
    session.outputs = [
      {
        id: "output-theme",
        handle: "resource-output",
        originItemId: "item-tool",
        kind: "file",
        title: "theme.css",
        subtitle: "128 bytes",
        mimeType: "text/plain",
        updatedAt: timestamp,
      },
    ];
    const onOpenResource = vi.fn();
    const onOpenSource = vi.fn();
    const onOpenOutput = vi.fn();

    render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={onOpenOutput}
        onOpenSource={onOpenSource}
        onOpenResource={onOpenResource}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "View changes to src/theme.ts" }),
    );
    expect(onOpenResource).toHaveBeenLastCalledWith(
      "resource-diff",
      "src/theme.ts changes",
      "diff",
    );
    await user.click(
      screen.getByRole("button", { name: "View resulting src/theme.ts" }),
    );
    expect(onOpenResource).toHaveBeenLastCalledWith(
      "resource-result",
      "src/theme.ts",
      "text",
    );
    await user.click(
      screen.getByRole("button", { name: "Open source theme.ts" }),
    );
    expect(onOpenSource).toHaveBeenCalledWith("source-theme");
    await user.click(
      screen.getByRole("button", { name: "Open output theme.css" }),
    );
    expect(onOpenOutput).toHaveBeenCalledWith("output-theme");
  });

  it("zooms, resets, downloads, and closes an attached image preview", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "image-message",
      turnId: "image-turn",
      kind: "user_message",
      content: "Use this image.",
      attachments: [
        {
          id: "image-ref",
          handle: "image-handle",
          name: "photo.png",
          mediaType: "image/png",
          size: 4,
        },
      ],
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
        attachmentContentUrl={() => "/photo.png"}
      />,
    );

    const thumbnail = screen.getByRole("button", {
      name: "View attached image 1",
    });
    await user.click(thumbnail);
    expect(
      screen.getByRole("dialog", { name: "Preview photo.png" }),
    ).toBeVisible();
    expect(
      screen.getByRole("link", { name: "Download photo.png" }),
    ).toHaveAttribute("download", "photo.png");
    await user.keyboard("+");
    expect(screen.getByText("150%")).toBeVisible();
    await user.keyboard("0");
    expect(screen.getByText("100%")).toBeVisible();
    await user.keyboard("{Escape}");
    expect(
      screen.queryByRole("dialog", { name: "Preview photo.png" }),
    ).toBeNull();
    await waitFor(() => expect(thumbnail).toHaveFocus());
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

  it("keeps approvals one-shot while the wire only supports one-shot approval", async () => {
    const user = userEvent.setup();
    const onResolveApproval = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-attention"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={onResolveApproval}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "Allow for this session" }),
    ).toBeNull();
    await user.click(screen.getByRole("button", { name: "Allow once" }));
    expect(onResolveApproval).toHaveBeenCalledWith(
      "approval-keychain",
      "allowed_once",
    );
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
