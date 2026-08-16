/// <reference types="vite/client" />

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionDraftStore } from "../drafts";
import { fixtureBootstrap, fixtureSessions } from "../fixtures";
import type {
  CommandDiscovery,
  CompletionReview,
  TrustedFileEntry,
} from "../protocol";
import { Conversation } from "./Conversation";

const noOp = async () => {};
const mentionFiles: TrustedFileEntry[] = [
  {
    id: "file-readme",
    relativePath: "docs/README.md",
    displayName: "README.md",
    kind: "documentation",
    byteLen: 1_536,
  },
  {
    id: "file-config",
    relativePath: "config/application.toml",
    displayName: "application.toml",
    kind: "configuration",
    byteLen: 768,
  },
];
const slashDiscovery: CommandDiscovery = {
  commands: [
    {
      name: "compact",
      usage: "/compact",
      description: "compact conversation context",
      acceptsArgument: false,
      kind: "builtIn",
    },
    {
      name: "review",
      usage: "/review [focus]",
      description: "prompt · review the implementation",
      argumentHint: "[focus]",
      acceptsArgument: true,
      kind: "prompt",
    },
    {
      name: "skills",
      usage: "/skills [subcommand]",
      description: "manage and view agent skills",
      acceptsArgument: true,
      kind: "builtIn",
    },
  ],
  skills: [
    {
      id: "testing",
      name: "Testing",
      description: "Run focused tests and interpret failures.",
      active: false,
    },
  ],
};
class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length() {
    return this.values.size;
  }

  clear() {
    this.values.clear();
  }

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  key(index: number) {
    return Array.from(this.values.keys())[index] ?? null;
  }

  removeItem(key: string) {
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}
const completionReview = (
  summary: string,
  durationMs: number,
): CompletionReview => ({
  summary,
  durationMs,
  actionCount: 0,
  phases: [],
  changedFileItemIds: [],
  verificationActionItemIds: [],
  failedActionItemIds: [],
  warningActionItemIds: [],
  sourceIds: [],
  outputIds: [],
  testResults: [],
  evidenceCoverage: "none",
  openQuestions: [],
});

describe("conversation composer", () => {
  beforeEach(() => {
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: new MemoryStorage(),
    });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("executes /goal commands without sending them as prompts", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const onGoalCommand = vi.fn().mockResolvedValue("Goal set: ship the release");
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onGoalCommand={onGoalCommand}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
      />,
    );

    const composer = screen.getByRole("textbox", { name: "Message ygg" });
    await user.type(composer, "/goal ship the release");
    await user.keyboard("{Enter}");

    await waitFor(() =>
      expect(onGoalCommand).toHaveBeenCalledWith({
        type: "set",
        objective: "ship the release",
      }),
    );
    expect(onSubmit).not.toHaveBeenCalled();
    expect(composer).toHaveValue("");
    expect(screen.getByText("Goal set: ship the release")).toBeVisible();
  });

  it("wires host document and trusted-file context into the composer", async () => {
    const user = userEvent.setup();
    const onListProjectFiles = vi.fn().mockResolvedValue({
      summary: { indexedFiles: 0, ignoredEntries: 0, truncated: false },
      files: [],
    });
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onIngestAttachment={vi.fn()}
        onIngestDocument={vi.fn()}
        onListProjectFiles={onListProjectFiles}
        onSearchProjectFiles={vi.fn()}
        onReadProjectFile={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Context" }));

    expect(
      screen.getByRole("dialog", { name: "Add prompt context" }),
    ).toBeVisible();
    await waitFor(() => expect(onListProjectFiles).toHaveBeenCalledOnce());
  });

  it("adds fuzzy @ references as trusted project-file context without editing prompt text", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const onListProjectFiles = vi.fn().mockResolvedValue({
      summary: { indexedFiles: mentionFiles.length, ignoredEntries: 0, truncated: false },
      files: mentionFiles,
    });
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onListProjectFiles={onListProjectFiles}
        onSearchProjectFiles={vi.fn()}
        onReadProjectFile={vi.fn()}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "Please inspect @RDM");
    expect(
      await screen.findByRole("option", { name: /README\.md.*docs\/README\.md/i }),
    ).toBeVisible();
    await user.keyboard("{Enter}");

    expect(composer).toHaveValue("Please inspect ");
    expect(screen.getByLabelText("Referenced trusted project files")).toHaveTextContent(
      "docs/README.md",
    );
    await user.type(composer, "next");
    await user.click(screen.getByRole("button", { name: "Send message" }));

    expect(onSubmit).toHaveBeenCalledWith(
      "Please inspect next",
      [],
      undefined,
      expect.any(String),
      [],
      [mentionFiles[0]],
    );
  });

  it("keeps @ completion available in absolute-path prompts", async () => {
    const user = userEvent.setup();
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onListProjectFiles={vi.fn().mockResolvedValue({
          summary: { indexedFiles: mentionFiles.length, ignoredEntries: 0, truncated: false },
          files: mentionFiles,
        })}
        onSearchProjectFiles={vi.fn()}
        onReadProjectFile={vi.fn()}
      />,
    );

    await user.type(screen.getByLabelText("Message ygg"), "/workspace/src @RDM");

    expect(
      await screen.findByRole("option", { name: /README\.md.*docs\/README\.md/i }),
    ).toBeVisible();
  });

  it("dismisses @ completion without submitting the draft", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onListProjectFiles={vi.fn().mockResolvedValue({
          summary: { indexedFiles: mentionFiles.length, ignoredEntries: 0, truncated: false },
          files: mentionFiles,
        })}
        onSearchProjectFiles={vi.fn()}
        onReadProjectFile={vi.fn()}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "@");
    expect(
      await screen.findByRole("listbox", { name: "Trusted project files" }),
    ).toBeVisible();
    await user.keyboard("{Escape}");

    expect(
      screen.queryByRole("listbox", { name: "Trusted project files" }),
    ).toBeNull();
    expect(composer).toHaveValue("@");
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("discovers slash commands, keeps web-local commands available, and invokes skills through the host", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const onInvokeSlashCommand = vi.fn().mockResolvedValue(undefined);
    const onOpenRuntimeStatus = vi.fn();
    const bootstrap = structuredClone(fixtureBootstrap);
    bootstrap.capabilities.sessionExport = true;
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.branches = {
      head: "entry-forkable",
      entries: [
        {
          entryId: "entry-forkable",
          kind: "assistantMessage",
          checkoutable: true,
          label: "Ready",
        },
      ],
      truncated: false,
    };
    render(
      <Conversation
        session={session}
        bootstrap={bootstrap}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onGetCommandDiscovery={vi.fn().mockResolvedValue(slashDiscovery)}
        onInvokeSlashCommand={onInvokeSlashCommand}
        onExportSession={vi.fn()}
        onForkSession={vi.fn().mockResolvedValue(undefined)}
        onOpenRuntimeStatus={onOpenRuntimeStatus}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "/");
    expect(
      await screen.findByRole("option", { name: /\/compact/ }),
    ).toBeVisible();
    expect(screen.getByRole("option", { name: /\/export/ })).toBeVisible();
    expect(screen.getByRole("option", { name: /\/fork/ })).toBeVisible();
    expect(screen.getByRole("option", { name: /\/status/ })).toBeVisible();

    await user.clear(composer);
    await user.type(composer, "/status");
    await user.keyboard("{Enter}");
    expect(onOpenRuntimeStatus).toHaveBeenCalledOnce();
    expect(onInvokeSlashCommand).not.toHaveBeenCalled();

    await user.type(composer, "/com");
    expect(
      await screen.findByRole("option", { name: /\/compact/ }),
    ).toBeVisible();
    await user.keyboard("{Tab}");
    expect(composer).toHaveValue("/compact");
    await user.keyboard("{Enter}");
    await waitFor(() =>
      expect(onInvokeSlashCommand).toHaveBeenCalledWith(
        "/compact",
        expect.any(String),
      ),
    );

    await user.type(composer, "/skills load ");
    expect(
      await screen.findByRole("option", { name: /Testing.*focused tests/i }),
    ).toBeVisible();
    await user.keyboard("{Enter}");
    expect(composer).toHaveValue("/skills load testing");
    await user.keyboard("{Enter}");
    await waitFor(() =>
      expect(onInvokeSlashCommand).toHaveBeenLastCalledWith(
        "/skills load testing",
        expect.any(String),
      ),
    );

    await user.type(composer, "/export archive");
    await user.keyboard("{Enter}");
    await waitFor(() =>
      expect(onInvokeSlashCommand).toHaveBeenLastCalledWith(
        "/export archive",
        expect.any(String),
      ),
    );

    await user.type(composer, "Explain /compact");
    expect(screen.queryByRole("listbox", { name: "Slash commands" })).toBeNull();
    await user.keyboard("{Enter}");
    expect(onSubmit).toHaveBeenCalledWith(
      "Explain /compact",
      [],
      undefined,
      expect.any(String),
      [],
      [],
    );
  });

  it("keeps slash commands out of active steer and follow-up prompts", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const onInvokeSlashCommand = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-live"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onInvokeSlashCommand={onInvokeSlashCommand}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "/compact ");
    expect(screen.queryByRole("listbox", { name: "Slash commands" })).toBeNull();
    await user.keyboard("{Enter}");

    expect(onSubmit).not.toHaveBeenCalled();
    expect(onInvokeSlashCommand).not.toHaveBeenCalled();
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Slash commands are available after current work finishes.",
    );

    await user.clear(composer);
    await user.type(composer, "/skills ");
    const listSkills = await screen.findByRole("option", {
      name: /\/skills list/i,
    });
    expect(listSkills).toBeDisabled();
    await user.click(listSkills);
    expect(composer).toHaveValue("/skills ");
  });

  it("shows the context percentage and tier-aware input cost estimate", () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    session.modelId = "claude-sonnet-4-6";
    session.contextTokens = 240_000;
    session.contextPercent = 60;
    session.context = {
      usage: {
        inputTokens: 240_000,
        outputTokens: 0,
        contextTokens: 240_000,
        contextLimit: 400_000,
      },
      compactions: 0,
      status: {
        current: {
          categories: [{ category: "other", tokens: 240_000 }],
          totalTokens: 240_000,
        },
        updatedAtMs: 1,
      },
    };
    render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
      />,
    );

    expect(screen.getByText("~$1.44")).toBeVisible();
    expect(
      screen.getByLabelText(
        "60% of context used (240,000 of 400,000 tokens); Breakdown: unattributed 240,000; Estimated next-turn input cost ~$1.44",
      ),
    ).toBeVisible();
  });

  it("omits a meaningless zero-value context cost estimate", () => {
    render(
      <Conversation
        session={structuredClone(fixtureSessions["session-fresh"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
      />,
    );

    const context = document.querySelector(".composer-context-cost");
    expect(context).not.toHaveTextContent("$0.00");
    expect(context).not.toHaveAccessibleName(/Estimated next-turn input cost/);
  });

  it("projects live context sources and compaction lifecycle in the composer", () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    const totals = {
      categories: [
        { category: "documents" as const, tokens: 20_000 },
        { category: "projectFiles" as const, tokens: 30_000 },
      ],
      totalTokens: 50_000,
    };
    session.contextTokens = 50_000;
    session.contextPercent = 25;
    session.context = {
      usage: {
        inputTokens: 50_000,
        outputTokens: 1_000,
        contextTokens: 50_000,
        contextLimit: 200_000,
      },
      compactions: 0,
      status: {
        current: totals,
        updatedAtMs: 100,
        activeCompaction: {
          id: "run-live:compaction:1",
          reason: "overflow",
          before: totals,
          startedAtMs: 101,
        },
      },
      run: {
        phase: "compacting",
        responsesStarted: 1,
        responsesFinished: 0,
        responsesDiscarded: 1,
        responseActive: false,
        toolCallsStarted: 0,
        toolCallsFinished: 0,
        toolExecutionsStarted: 0,
        toolExecutionsFinished: 0,
        compactionsStarted: 1,
        compactionsCompleted: 0,
        compactionsFailed: 0,
      },
    };

    render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
      />,
    );

    expect(screen.getByText("Compacting")).toBeVisible();
    const context = screen.getByLabelText(/Compacting after overflow trigger/);
    expect(context).toHaveAttribute("data-run-phase", "compacting");
    expect(context).toHaveAttribute("data-compaction-active", "true");
    expect(context).toHaveAccessibleName(/documents 20,000/);
    expect(context).toHaveAccessibleName(/project files 30,000/);
  });

  it("replaces active retrying state with the terminal provider failure", () => {
    const session = structuredClone(fixtureSessions["session-live"]!);
    session.context.run = {
      phase: "retrying",
      responsesStarted: 1,
      responsesFinished: 0,
      responsesDiscarded: 1,
      responseActive: false,
      toolCallsStarted: 0,
      toolCallsFinished: 0,
      toolExecutionsStarted: 0,
      toolExecutionsFinished: 0,
      compactionsStarted: 0,
      compactionsCompleted: 0,
      compactionsFailed: 0,
    };
    const props = {
      bootstrap: structuredClone(fixtureBootstrap),
      onSubmit: noOp,
      onInterrupt: noOp,
      onConfigure: noOp,
      onResolveApproval: noOp,
      onResolveUserInput: noOp,
      onOpenOutput: vi.fn(),
      onOpenSource: vi.fn(),
    };
    const { rerender } = render(<Conversation session={session} {...props} />);

    expect(screen.getByText("Retrying")).toBeVisible();
    expect(screen.queryByRole("alert")).toBeNull();

    const failed = structuredClone(session);
    failed.status = "failed";
    failed.activeRunId = undefined;
    failed.context.run = {
      ...failed.context.run!,
      phase: "finished",
      terminalState: "failed",
      responsesStarted: 4,
      responsesDiscarded: 4,
    };
    failed.items = failed.items.map((item) => ({
      ...item,
      state: "committed" as const,
    }));
    failed.items.push({
      id: "provider-failure",
      runId: "run-live",
      turnId: "live-turn",
      kind: "run_outcome",
      outcome: "failed",
      durationMs: 1_200,
      summary: "provider=custom/e2e model=e2e-model phase=connection",
      review: completionReview("Provider request failed", 1_200),
      state: "committed",
      createdAt: new Date().toISOString(),
    });
    rerender(<Conversation session={failed} {...props} />);

    expect(screen.queryByText("Retrying")).toBeNull();
    const failure = screen.getByRole("alert");
    expect(failure).toHaveTextContent("Model response failed");
    expect(failure).toHaveTextContent(
      "provider=custom/e2e model=e2e-model phase=connection",
    );
  });

  it("edits, retries with a model, and forks only from durable checkpoints", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items = [
      {
        id: "branch-user",
        turnId: "branch-turn",
        durableEntryId: "entry-user",
        kind: "user_message",
        content: "Original request",
        state: "committed",
        createdAt: session.startedAt,
      },
      {
        id: "branch-assistant",
        turnId: "branch-turn",
        durableEntryId: "entry-assistant",
        kind: "assistant_message",
        content: "Original response",
        state: "committed",
        createdAt: session.startedAt,
      },
    ];
    const onEditUserTurn = vi.fn().mockResolvedValue(undefined);
    const onRetryResponse = vi.fn().mockResolvedValue(undefined);
    const onForkConversation = vi.fn().mockResolvedValue(undefined);
    render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onEditUserTurn={onEditUserTurn}
        onRetryResponse={onRetryResponse}
        onForkConversation={onForkConversation}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Edit this turn" }));
    expect(screen.getByText("External effects are preserved.")).toBeVisible();
    const replacement = screen.getByLabelText("Replacement message");
    await user.clear(replacement);
    await user.type(replacement, "Replacement request");
    await user.click(
      screen.getByRole("button", { name: "Create edited branch" }),
    );
    expect(onEditUserTurn).toHaveBeenCalledWith(
      "entry-user",
      "Replacement request",
    );

    await user.click(
      screen.getByRole("button", {
        name: "Retry response with another model",
      }),
    );
    await user.selectOptions(screen.getByLabelText("Model"), "gpt-5.4");
    await user.click(screen.getByRole("button", { name: "Retry with model" }));
    expect(onRetryResponse).toHaveBeenCalledWith(
      "entry-assistant",
      expect.objectContaining({ id: "gpt-5.4" }),
    );

    await user.click(
      screen.getAllByRole("button", {
        name: "Fork conversation here",
      })[0]!,
    );
    await user.click(screen.getByRole("button", { name: "Fork conversation" }));
    expect(onForkConversation).toHaveBeenCalledWith("entry-user");
  });

  it("restores an independent text and uploaded-attachment draft", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const bootstrap = structuredClone(fixtureBootstrap);
    new SessionDraftStore(window.localStorage).save(
      bootstrap.host.id,
      session.sessionId,
      {
        text: "Resume this exact draft",
        delivery: "submit",
        attachments: [
          {
            id: "attachment-draft",
            handle: "attachment-handle",
            name: "notes.md",
            mediaType: "text/markdown",
            size: 128,
          },
        ],
        updatedAt: new Date().toISOString(),
      },
    );

    render(
      <Conversation
        session={session}
        bootstrap={bootstrap}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(screen.getByLabelText("Message ygg")).toHaveValue(
      "Resume this exact draft",
    );
    expect(screen.getByText("notes.md")).toBeVisible();
  });

  it("steers with an attachment-only draft instead of replacing send with stop", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-live"]!);
    const bootstrap = structuredClone(fixtureBootstrap);
    const attachment = {
      id: "attachment-draft",
      handle: "attachment-handle",
      name: "notes.md",
      mediaType: "text/markdown",
      size: 128,
    };
    new SessionDraftStore(window.localStorage).save(
      bootstrap.host.id,
      session.sessionId,
      {
        text: "",
        delivery: "steer",
        attachments: [attachment],
        updatedAt: new Date().toISOString(),
      },
    );
    const onSubmit = vi.fn().mockResolvedValue(undefined);

    const { container } = render(
      <Conversation
        session={session}
        bootstrap={bootstrap}
        onSubmit={onSubmit}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    expect(screen.queryByRole("button", { name: "Stop ygg" })).toBeNull();
    expect(
      container.querySelectorAll(".composer-actions .submit-button"),
    ).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Steer active run" }));
    expect(onSubmit).toHaveBeenCalledWith(
      "",
      [attachment],
      "steer",
      expect.any(String),
      [],
      [],
    );
  });

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

  it("retries a recoverable send with the same idempotency key", async () => {
    const user = userEvent.setup();
    const onSubmit = vi
      .fn()
      .mockRejectedValueOnce(new TypeError("Network unavailable"))
      .mockResolvedValueOnce(undefined);
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

    await user.type(screen.getByLabelText("Message ygg"), "Retry safely");
    await user.click(screen.getByRole("button", { name: "Send message" }));
    expect(await screen.findByText("Connection interrupted")).toBeVisible();
    const firstKey = onSubmit.mock.calls[0]?.[3];
    expect(firstKey).toEqual(expect.any(String));

    await user.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(2));
    expect(onSubmit.mock.calls[1]?.[3]).toBe(firstKey);
    expect(screen.getByLabelText("Message ygg")).toHaveValue("");
  });

  it("starts a fresh slash invocation after cancelling a failed retry", async () => {
    const user = userEvent.setup();
    const onInvokeSlashCommand = vi
      .fn()
      .mockRejectedValue(new TypeError("Network unavailable"));
    render(
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
        onInvokeSlashCommand={onInvokeSlashCommand}
      />,
    );

    const composer = screen.getByLabelText("Message ygg");
    await user.type(composer, "/compact ");
    await user.keyboard("{Enter}");
    expect(await screen.findByText("Connection interrupted")).toBeVisible();
    const firstKey = onInvokeSlashCommand.mock.calls[0]?.[1];
    expect(firstKey).toEqual(expect.any(String));

    await user.click(screen.getByRole("button", { name: "Cancel retry" }));
    await user.click(composer);
    await user.keyboard("{Enter}");
    await waitFor(() => expect(onInvokeSlashCommand).toHaveBeenCalledTimes(2));
    expect(onInvokeSlashCommand.mock.calls[1]?.[1]).not.toBe(firstKey);
  });

  it("uses one action to stop, steer, and queue follow-ups", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const onInterrupt = vi.fn().mockResolvedValue(undefined);
    const { container } = render(
      <Conversation
        session={structuredClone(fixtureSessions["session-live"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={onSubmit}
        onInterrupt={onInterrupt}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    const stop = screen.getByRole("button", { name: "Stop ygg" });
    expect(stop).toBeVisible();
    expect(
      container.querySelectorAll(".composer-actions .submit-button"),
    ).toHaveLength(1);
    expect(
      screen.queryByRole("button", { name: "Steer active run" }),
    ).toBeNull();
    await user.click(stop);
    expect(onInterrupt).toHaveBeenCalledOnce();

    const delivery = screen.getByRole("button", {
      name: "While ygg is working: Steer now",
    });
    expect(
      screen.queryByRole("combobox", { name: "Active run delivery" }),
    ).toBeNull();
    await user.type(screen.getByLabelText("Message ygg"), "Steer this");
    expect(screen.queryByRole("button", { name: "Stop ygg" })).toBeNull();
    expect(
      container.querySelectorAll(".composer-actions .submit-button"),
    ).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Steer active run" }));

    expect(onSubmit).toHaveBeenCalledWith(
      "Steer this",
      [],
      "steer",
      expect.any(String),
      [],
      [],
    );

    await waitFor(() =>
      expect(screen.getByLabelText("Message ygg")).toHaveValue(""),
    );
    expect(screen.getByRole("button", { name: "Stop ygg" })).toBeVisible();
    await user.click(delivery);
    expect(
      screen.getByRole("menu", { name: "While ygg is working" }),
    ).toBeVisible();
    const followUp = screen.getByRole("menuitemradio", { name: /Follow up/ });
    expect(followUp).toBeVisible();
    await user.click(followUp);
    await waitFor(() => {
      expect(delivery).toHaveAccessibleName(
        "While ygg is working: Follow up",
      );
      expect(
        screen.queryByRole("menu", { name: "While ygg is working" }),
      ).toBeNull();
      expect(delivery).toHaveFocus();
    });

    await user.type(screen.getByLabelText("Message ygg"), "Then summarize");
    await user.click(
      await screen.findByRole("button", { name: "Queue follow-up" }),
    );
    expect(onSubmit).toHaveBeenLastCalledWith(
      "Then summarize",
      [],
      "followUp",
      expect.any(String),
      [],
      [],
    );

    await user.click(delivery);
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
    expect(screen.queryByRole("combobox", { name: "Authority" })).toBeNull();
    await user.click(authority);
    expect(screen.getByRole("menu", { name: "Authority" })).toBeVisible();
    await user.click(screen.getByLabelText("Message ygg"));
    expect(screen.queryByRole("menu", { name: "Authority" })).toBeNull();
    await user.click(authority);
    await user.click(screen.getByRole("menuitemradio", { name: /Workspace/ }));

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
      screen
        .getByRole("dialog", { name: "Model and effort" })
        .querySelector(".model-picker-setting-row")!,
    );
    await user.type(
      screen.getByRole("textbox", { name: "Search models" }),
      "gpt",
    );
    await user.click(screen.getByRole("option", { name: /GPT-5.4/ }));

    expect(onConfigure).toHaveBeenCalledWith({ modelId: "gpt-5.4" });
    expect(
      screen.getByRole("button", { name: /^Model Claude Sonnet 4\.6/ }),
    ).toBeVisible();
    expect(screen.queryByText("Speed", { exact: true })).toBeNull();
    await user.click(screen.getByRole("button", { name: /^Effort High/ }));
    await user.click(screen.getByRole("option", { name: "Low" }));
    expect(onConfigure).toHaveBeenCalledWith({ reasoning: "low" });
  });

  it("maps exact xhigh and max effort to particles and max to rainbow", async () => {
    const user = userEvent.setup();
    const cases = [
      {
        effort: "high",
        options: ["low", "medium", "high"],
        max: "false",
        overdrive: "false",
        particles: false,
      },
      {
        effort: "xhigh",
        options: ["low", "medium", "high", "xhigh"],
        max: "false",
        overdrive: "true",
        particles: true,
      },
      {
        effort: "max",
        options: ["low", "medium", "high", "xhigh", "max"],
        max: "true",
        overdrive: "false",
        particles: true,
      },
    ];

    for (const testCase of cases) {
      const bootstrap = structuredClone(fixtureBootstrap);
      const session = structuredClone(fixtureSessions["session-fresh"]!);
      const model = bootstrap.models.find(
        (candidate) => candidate.id === session.modelId,
      )!;
      model.reasoning = testCase.options;
      model.defaultReasoning = testCase.effort;
      session.reasoning = testCase.effort;

      const { container, unmount } = render(
        <Conversation
          session={session}
          bootstrap={bootstrap}
          onSubmit={noOp}
          onInterrupt={noOp}
          onConfigure={noOp}
          onResolveApproval={noOp}
          onResolveUserInput={noOp}
          onOpenOutput={() => {}}
          onOpenSource={() => {}}
        />,
      );
      await user.click(
        screen.getByRole("button", { name: /Model and effort/ }),
      );

      const slider =
        container.querySelector<HTMLElement>(".power-slider-root")!;
      expect(slider).toHaveAttribute("data-max", testCase.max);
      expect(slider).toHaveAttribute("data-overdrive", testCase.overdrive);
      expect(slider.matches('[data-overdrive="true"], [data-max="true"]')).toBe(
        testCase.particles,
      );
      expect(
        slider.querySelectorAll(".power-slider-fast-particles > span"),
      ).toHaveLength(12);
      expect(slider.style.getPropertyValue("--power-position")).toBe("100%");
      expect(slider.style.getPropertyValue("--power-thumb-position")).toBe(
        "calc(100% + -14px)",
      );
      unmount();
    }
  });

  it("renders a failed outcome without a reasoning or action group", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.status = "failed";
    session.items.push({
      id: "failed-run-outcome",
      runId: "failed-run",
      turnId: "failed-turn",
      kind: "run_outcome",
      outcome: "failed",
      durationMs: 0,
      summary: "provider=custom/e2e model=e2e-model phase=connection",
      review: completionReview("Provider request failed", 0),
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

    expect(screen.getByRole("alert")).toHaveTextContent(
      "provider=custom/e2e model=e2e-model phase=connection",
    );
  });

  it("renders a failed outcome after work items", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const createdAt = new Date().toISOString();
    session.status = "failed";
    session.items.push(
      {
        id: "failed-work",
        runId: "failed-run",
        turnId: "failed-turn",
        kind: "action",
        actionKind: "file_search",
        phase: "investigated",
        status: "succeeded",
        rawToolName: "search",
        label: "Searched files",
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
        state: "committed",
        createdAt,
      },
      {
        id: "failed-work-outcome",
        runId: "failed-run",
        turnId: "failed-turn",
        kind: "run_outcome",
        outcome: "failed",
        durationMs: 10,
        summary: "provider=custom/e2e model=e2e-model phase=connection",
        review: completionReview("Provider request failed", 10),
        state: "committed",
        createdAt,
      },
    );
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

    expect(screen.getByRole("alert")).toHaveTextContent(
      "provider=custom/e2e model=e2e-model phase=connection",
    );
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
      review: completionReview("Run completed", 0),
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

    expect(screen.queryByText("Run completed")).toBeNull();
    expect(screen.queryByText(/Worked for/)).toBeNull();
  });

  it("renders assistant markdown as readable, safe rich content", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "assistant-markdown",
      turnId: "turn-markdown",
      kind: "assistant_message",
      content:
        "## Result\n\n- One\n- Two\n\n```ts\nconst answer = 42;\n```\n\n```diff\ndiff --git a/src/result.ts b/src/result.ts\n--- a/src/result.ts\n+++ b/src/result.ts\n@@ -1,2 +1,2 @@\n const stable = true;\n-const answer = 41;\n+const answer = 42;\n```\n\n<script>alert('no')</script>",
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

    expect(screen.getByRole("heading", { name: "Result" })).toBeVisible();
    expect(screen.getByRole("list")).toHaveTextContent(/One\s+Two/);
    expect(screen.getAllByRole("button", { name: "Copy code" })).toHaveLength(
      2,
    );
    const diff = container.querySelector<HTMLElement>(
      ".markdown-code-block.is-diff",
    );
    expect(diff).not.toBeNull();
    expect(diff!.querySelector(".markdown-code-language")).toHaveTextContent(
      "src/result.ts",
    );
    expect(diff!.querySelectorAll(".markdown-diff-line.is-addition")).toHaveLength(
      1,
    );
    expect(diff!.querySelectorAll(".markdown-diff-line.is-deletion")).toHaveLength(
      1,
    );
    expect(diff!.querySelectorAll(".markdown-diff-line.is-hunk")).toHaveLength(
      1,
    );
    expect(diff!.querySelector(".markdown-diff-stats")).toHaveAccessibleName(
      "1 added, 1 removed",
    );
    expect(container.querySelector("script")).toBeNull();
    expect(screen.getByText("<script>alert('no')</script>")).toBeVisible();
  });

  it("defers Markdown parsing until a streaming response is committed", async () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "assistant-streaming-markdown",
      turnId: "turn-streaming-markdown",
      kind: "assistant_message",
      content: "## Streaming heading",
      state: "streaming",
      createdAt: new Date().toISOString(),
    });
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
    const { rerender } = render(<Conversation session={session} {...props} />);

    expect(
      screen.queryByRole("heading", { name: "Streaming heading" }),
    ).toBeNull();
    expect(screen.getByText("## Streaming heading")).toBeVisible();

    const committed = structuredClone(session);
    const response = committed.items.at(-1);
    if (response) response.state = "committed";
    rerender(<Conversation session={committed} {...props} />);

    expect(
      await screen.findByRole("heading", { name: "Streaming heading" }),
    ).toBeVisible();
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

  it("animates only the latest work group in the active run", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const createdAt = new Date().toISOString();
    session.status = "working";
    session.activeRunId = "run-current";
    session.items = [
      {
        id: "reasoning-prior",
        runId: "run-current",
        turnId: "turn-prior",
        kind: "reasoning",
        summary: "Inspecting the first pass",
        content: "The first pass is still provisional.",
        state: "streaming",
        createdAt,
      },
    ];
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

    const { container, rerender } = render(
      <Conversation session={session} {...props} />,
    );
    const priorGroup = container
      .querySelector('[data-item-id="reasoning-prior"]')
      ?.closest(".work-group");
    expect(
      screen.getByRole("button", { name: "Working" }).closest(".work-group"),
    ).toBe(priorGroup);

    const update = structuredClone(session);
    update.items.push(
      {
        id: "assistant-between",
        runId: "run-current",
        turnId: "turn-prior",
        kind: "assistant_message",
        content: "The first pass is complete. I’ll check the final state.",
        state: "streaming",
        createdAt,
      },
      {
        id: "reasoning-latest",
        runId: "run-current",
        turnId: "turn-latest",
        kind: "reasoning",
        summary: "Checking the final state",
        content: "The final check is in progress.",
        state: "streaming",
        createdAt,
      },
    );
    rerender(<Conversation session={update} {...props} />);

    const workingIndicators = screen.getAllByRole("button", {
      name: "Working",
    });
    const latestGroup = container
      .querySelector('[data-item-id="reasoning-latest"]')
      ?.closest(".work-group");

    expect(workingIndicators).toHaveLength(1);
    expect(workingIndicators[0]!.closest(".work-group")).toBe(latestGroup);
    expect(priorGroup).toHaveClass("is-complete");
    expect(priorGroup?.querySelector(".work-group-summary")).toHaveTextContent(
      "Inspecting the first pass",
    );
    expect(
      priorGroup?.querySelector(".work-group-summary .live-dots"),
    ).toBeNull();
    expect(
      container.querySelectorAll(".live-dots:not(.is-static)"),
    ).toHaveLength(1);
  });

  it("keeps prior live work collapsed and lets the user reopen it", async () => {
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

    expect(container.querySelector(".composer-running-edge-chase")).not.toBeNull();
    expect(
      container
        .querySelector<HTMLElement>(".composer")
        ?.style.getPropertyValue("--model-accent-dark"),
    ).not.toBe("");

    const historySummary = screen.getByRole("button", { name: "Working" });
    expect(historySummary).toHaveAttribute("aria-expanded", "false");
    expect(historySummary.querySelector(".work-group-glyph")).toHaveClass(
      "is-live",
    );
    expect(historySummary.querySelector(".live-dots")).not.toHaveClass(
      "is-static",
    );
    const historyContent = container.querySelector(".work-group-content-clip");
    expect(historyContent).toHaveAttribute("aria-hidden", "true");
    expect(historyContent).toHaveAttribute("inert", "");
    const liveReasoning = screen
      .getByText("Checking the narrow layout")
      .closest("details");
    expect(liveReasoning).not.toHaveAttribute("open");
    expect(liveReasoning).toBe(
      container.querySelector(".work-group-content .reasoning-block"),
    );
    expect(liveReasoning?.querySelector(".live-dots")).toHaveClass(
      "is-static",
    );
    expect(container.querySelector(".work-group-live-item")).toBeNull();

    const streamingUpdate = structuredClone(session);
    const reasoning = streamingUpdate.items.find(
      (item) => item.kind === "reasoning",
    );
    if (reasoning?.kind === "reasoning") {
      reasoning.summary = "Checking focus order and the final phone state";
      reasoning.content += " The focus order is now stable.";
    }
    rerender(<Conversation session={streamingUpdate} {...props} />);

    expect(screen.getByRole("button", { name: "Working" })).toBe(
      historySummary,
    );
    expect(container.querySelector(".work-group-content-clip")).toBe(
      historyContent,
    );

    const toolUpdate = structuredClone(streamingUpdate);
    const completedReasoning = toolUpdate.items.find(
      (item) => item.kind === "reasoning",
    );
    if (completedReasoning?.kind === "reasoning") {
      completedReasoning.state = "committed";
    }
    toolUpdate.items.push({
      id: "live-tool",
      runId: "run-live",
      turnId: "live-turn",
      kind: "action",
      actionKind: "file_search",
      phase: "investigated",
      status: "running",
      rawToolName: "search",
      label: "Searching focus styles",
      observedOutputBytes: 0,
      droppedOutputBytes: 0,
      changedPaths: [],
      sourceIds: [],
      outputIds: [],
      state: "streaming",
      createdAt: new Date().toISOString(),
    });
    rerender(<Conversation session={toolUpdate} {...props} />);

    expect(screen.getByRole("button", { name: "Working" })).toBe(
      historySummary,
    );
    expect(container.querySelector(".work-group-content-clip")).toBe(
      historyContent,
    );
    expect(screen.getByText("Searching focus styles")).toBeInTheDocument();

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
      runId: "run-live",
      turnId: "live-turn",
      kind: "run_outcome",
      outcome: "done",
      durationMs: 4_500,
      summary: "Work completed",
      review: completionReview("Work completed", 4_500),
      state: "committed",
      createdAt: new Date().toISOString(),
    });
    rerender(<Conversation session={completed} {...props} />);

    expect(container.querySelector(".composer-running-edge")).toBeNull();
    const summary = screen.getByRole("button", {
      name: "Read files, edited file, viewed preview · 5s",
    });
    expect(summary).toBe(historySummary);
    expect(summary).toHaveAttribute("aria-expanded", "false");
    const completedHistoryContent = container.querySelector(
      ".work-group-content-clip",
    );
    expect(completedHistoryContent).toBe(historyContent);
    expect(completedHistoryContent).toHaveAttribute("aria-hidden", "true");
    expect(completedHistoryContent).toHaveAttribute("inert", "");
    expect(screen.queryByText("Review details")).toBeNull();
    expect(container.querySelector(".completion-review-disclosure")).toBeNull();
    await user.click(summary);
    expect(summary).toHaveAttribute("aria-expanded", "true");
    expect(completedHistoryContent).toHaveAttribute("aria-hidden", "false");
    expect(completedHistoryContent).not.toHaveAttribute("inert");
  });

  it("groups completed Bash commands while keeping each command collapsed", async () => {
    const user = userEvent.setup();
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const timestamp = new Date().toISOString();
    session.status = "done";
    session.items = [
      {
        id: "bash-one",
        runId: "bash-run",
        turnId: "bash-turn",
        kind: "action",
        actionKind: "command",
        phase: "investigated",
        status: "succeeded",
        rawToolName: "bash",
        label: "Ran command",
        commandPreview: "git status --short",
        observedOutputBytes: 32,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
        state: "committed",
        createdAt: timestamp,
      },
      {
        id: "bash-two",
        runId: "bash-run",
        turnId: "bash-turn",
        kind: "action",
        actionKind: "command",
        phase: "verified",
        status: "succeeded",
        rawToolName: "bash",
        label: "Ran command",
        commandPreview: "npm test -- --run src/components",
        observedOutputBytes: 64,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
        state: "committed",
        createdAt: timestamp,
      },
    ];

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

    const groupSummary = screen.getByRole("button", {
      name: "Ran commands",
    });
    expect(groupSummary).toHaveAttribute("aria-expanded", "false");
    expect(container.querySelectorAll(".command-batch .action-cell")).toHaveLength(
      2,
    );
    expect(container.querySelectorAll(".command-batch > summary")).toHaveLength(
      0,
    );
    expect(container.querySelectorAll(".command-batch .action-cell[open]")).toHaveLength(
      0,
    );
    expect(container.querySelectorAll(".bash-logo")).toHaveLength(2);

    await user.click(groupSummary);
    expect(groupSummary).toHaveAttribute("aria-expanded", "true");
    const firstActionSummary = container.querySelector<HTMLElement>(
      ".command-batch .action-cell > summary",
    );
    expect(firstActionSummary).not.toBeNull();
    await user.click(firstActionSummary!);
    expect(firstActionSummary!.parentElement).toHaveAttribute("open", "");
  });

  it("flattens evidence-free one-action runs into one useful row", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const timestamp = new Date().toISOString();
    const review = completionReview("Context compacted", 83_000);
    review.actionCount = 1;
    review.phases = [
      {
        phase: "other",
        actionCount: 1,
        succeededCount: 1,
        failedCount: 0,
        stoppedCount: 0,
      },
    ];
    session.items = [
      {
        id: "compaction-action",
        runId: "compaction-run",
        turnId: "compaction-turn",
        kind: "action",
        actionKind: "analysis",
        phase: "other",
        status: "succeeded",
        rawToolName: "compaction",
        label: "Compacted session context",
        detail: "The context window reached its safe compaction boundary.",
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
        state: "committed",
        createdAt: timestamp,
      },
      {
        id: "compaction-outcome",
        runId: "compaction-run",
        turnId: "compaction-turn",
        kind: "run_outcome",
        outcome: "done",
        durationMs: 83_000,
        summary: "Context compacted",
        review,
        state: "committed",
        createdAt: timestamp,
      },
    ];
    const { container, rerender } = render(
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

    const action = screen.getByText("Compacted session context").closest(
      "summary",
    );
    expect(action).toHaveTextContent("1m 23s");
    expect(container.querySelector(".work-group.is-direct")).not.toBeNull();
    expect(container.querySelector(".work-group-summary")).toBeNull();
    expect(container.querySelector(".completion-review-disclosure")).toBeNull();
    expect(screen.queryByText("Inspected results")).toBeNull();
    expect(screen.queryByText("Review details")).toBeNull();

    const reviewed = structuredClone(session);
    const reviewedOutcome = reviewed.items.find(
      (item) => item.kind === "run_outcome",
    );
    if (reviewedOutcome?.kind === "run_outcome") {
      reviewedOutcome.review.evidenceCoverage = "partial";
      reviewedOutcome.review.openQuestions = ["Confirm the next context budget"];
    }
    rerender(
      <Conversation
        session={reviewed}
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
    expect(container.querySelector(".work-group.is-direct")).not.toBeNull();
    expect(container.querySelector(".work-group-summary")).toBeNull();
    expect(container.querySelector(".completion-review-disclosure")).toBeNull();
    expect(screen.queryByText("Open questions")).toBeNull();
  });

  it("loads inline diffs and line-numbered reads when a work group opens", async () => {
    const user = userEvent.setup();
    const diff = [
      "diff --git a/src/theme.ts b/src/theme.ts",
      "--- a/src/theme.ts",
      "+++ b/src/theme.ts",
      "@@ -1 +1 @@",
      "-old",
      "+new",
    ].join("\n");
    const sourceText = "const answer = 42;\nexport default answer;\n";
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async (input) => {
      const body = String(input).endsWith("resource-diff") ? diff : sourceText;
      return new Response(body, {
        status: 200,
        headers: {
          "Content-Length": String(new TextEncoder().encode(body).byteLength),
        },
      });
    });
    vi.stubGlobal("fetch", fetchMock);
    const resourceContentUrl = vi.fn(
      (sessionId: string, handle: string) =>
        `/api/v1/sessions/${sessionId}/resources/${handle}`,
    );
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    const timestamp = new Date().toISOString();
    session.status = "done";
    session.items = [
      {
        id: "read-theme",
        runId: "preview-run",
        turnId: "preview-turn",
        kind: "action",
        actionKind: "file_read",
        phase: "investigated",
        status: "succeeded",
        rawToolName: "read",
        label: "Read source",
        target: "src/theme.ts",
        observedOutputBytes: sourceText.length,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: ["source-theme"],
        outputIds: [],
        state: "committed",
        createdAt: timestamp,
      },
      {
        id: "write-theme",
        runId: "preview-run",
        turnId: "preview-turn",
        kind: "action",
        actionKind: "file_write",
        phase: "changed",
        status: "succeeded",
        rawToolName: "apply_patch",
        label: "Edited theme",
        target: "src/theme.ts",
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: ["src/theme.ts"],
        sourceIds: [],
        outputIds: [],
        diffHandle: "resource-diff",
        state: "committed",
        createdAt: timestamp,
      },
    ];
    session.sources = [
      {
        id: "source-theme",
        handle: "resource-source",
        kind: "file",
        title: "theme.ts",
        subtitle: "2 lines",
        consultedAt: timestamp,
        iconLabel: "SRC",
      },
    ];

    const { container } = render(
      <Conversation
        session={session}
        bootstrap={structuredClone(fixtureBootstrap)}
        resourceContentUrl={resourceContentUrl}
        onSubmit={noOp}
        onInterrupt={noOp}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
      />,
    );

    const groupSummary = screen.getByRole("button", {
      name: "Read files, edited file",
    });
    expect(groupSummary).toHaveAttribute("aria-expanded", "false");
    expect(container.querySelector(".activity-preview")).toBeNull();

    await user.click(groupSummary);
    expect(await screen.findByText("const answer = 42;")).toBeVisible();
    expect(await screen.findByText("new")).toBeVisible();
    expect(container.querySelectorAll(".activity-source-number").length).toBeGreaterThanOrEqual(2);
    expect(container.querySelectorAll(".activity-diff-line.is-addition")).toHaveLength(1);
    expect(container.querySelectorAll(".activity-diff-line.is-deletion")).toHaveLength(1);
    expect(resourceContentUrl).toHaveBeenCalledWith(
      "session-fresh",
      "resource-source",
    );
    expect(resourceContentUrl).toHaveBeenCalledWith(
      "session-fresh",
      "resource-diff",
    );
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/sessions/session-fresh/resources/resource-source",
      expect.objectContaining({
        credentials: "same-origin",
        cache: "no-store",
        redirect: "error",
      }),
    );
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
        runId: "run-evidence",
        turnId: "turn-evidence",
        kind: "action",
        actionKind: "file_write",
        phase: "changed",
        status: "running",
        rawToolName: "apply_patch",
        label: "Changed file",
        target: "src/theme.ts",
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: ["src/theme.ts"],
        sourceIds: [],
        outputIds: [],
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

    await user.click(screen.getByRole("button", { name: "Working" }));
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

  it("renders transcript file metadata for common file types", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "file-message",
      turnId: "file-turn",
      kind: "user_message",
      content: "Review these files.",
      attachments: [
        {
          id: "pdf-ref",
          handle: "pdf-handle",
          name: "architecture.pdf",
          mediaType: "application/pdf",
          size: 1_572_864,
        },
        {
          id: "archive-ref",
          handle: "archive-handle",
          name: "sources.zip",
          mediaType: "application/zip",
          size: 2_048,
        },
        {
          id: "code-ref",
          name: "src/main.rs",
          mediaType: "text/rust",
          size: 512,
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
      />,
    );

    const pdf = screen
      .getByText("architecture.pdf")
      .closest(".message-file-attachment");
    if (!pdf) throw new Error("PDF attachment was not rendered");
    expect(pdf).toHaveAttribute(
      "title",
      "Media type: application/pdf\nSource: uploaded attachment",
    );
    expect(pdf).toHaveTextContent("1.5 MB");
    expect(
      pdf.querySelector(".message-file-attachment-icon.is-pdf"),
    ).toBeInTheDocument();

    const archive = screen
      .getByText("sources.zip")
      .closest(".message-file-attachment");
    if (!archive) throw new Error("archive attachment was not rendered");
    expect(archive).toHaveTextContent("2 KB");
    expect(
      archive.querySelector(".message-file-attachment-icon.is-archive"),
    ).toBeInTheDocument();

    const code = screen
      .getByText("src/main.rs")
      .closest(".message-file-attachment");
    if (!code) throw new Error("code attachment was not rendered");
    expect(code).toHaveAttribute(
      "title",
      "Media type: text/rust\nSource: transcript record",
    );
    expect(code).toHaveTextContent("512 B");
    expect(
      code.querySelector(".message-file-attachment-icon.is-code"),
    ).toBeInTheDocument();
  });

  it("renders document type, page-count, and fidelity badges", () => {
    const session = structuredClone(fixtureSessions["session-fresh"]!);
    session.items.push({
      id: "document-message",
      turnId: "document-turn",
      kind: "user_message",
      content: "Use these documents as context.",
      documents: [
        {
          id: "document-pdf",
          displayName: "architecture.pdf",
          mediaType: "application/pdf",
          sourceByteCount: 16_384,
          extractedTextByteCount: 8_192,
          sha256: "a".repeat(64),
          fidelity: "pdfTextOnlyPartial",
          pageCount: 7,
          createdAtMs: 1,
        },
        {
          id: "document-markdown",
          displayName: "notes.md",
          mediaType: "text/markdown",
          sourceByteCount: 2_048,
          extractedTextByteCount: 2_048,
          sha256: "b".repeat(64),
          fidelity: "exactUtf8",
          createdAtMs: 1,
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
      />,
    );

    expect(screen.getByText("PDF · 7 pages")).toHaveClass(
      "document-reference-badge",
    );
    expect(screen.getByText("pdfTextOnlyPartial")).toHaveClass(
      "document-reference-badge",
      "is-fidelity",
    );
    expect(screen.getByText("Markdown")).toHaveClass(
      "document-reference-badge",
    );
    expect(screen.getByText("exactUtf8")).toHaveClass(
      "document-reference-badge",
      "is-fidelity",
    );
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

    expect(document.querySelector(".composer-running-edge")).toBeNull();
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

  it("keeps interruption available while a failed image remains retryable", async () => {
    const user = userEvent.setup();
    const onInterrupt = vi.fn().mockResolvedValue(undefined);
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
        session={structuredClone(fixtureSessions["session-live"]!)}
        bootstrap={structuredClone(fixtureBootstrap)}
        onSubmit={noOp}
        onInterrupt={onInterrupt}
        onConfigure={noOp}
        onResolveApproval={noOp}
        onResolveUserInput={noOp}
        onOpenOutput={() => {}}
        onOpenSource={() => {}}
        onIngestAttachment={onIngestAttachment}
        attachmentContentUrl={(handle) => `/api/v1/attachments/${handle}`}
      />,
    );

    const picker =
      container.querySelector<HTMLInputElement>('input[type="file"]');
    expect(picker).not.toBeNull();
    await user.upload(
      picker!,
      new File(["tiny"], "photo.png", { type: "image/png" }),
    );

    const retry = await screen.findByRole("button", {
      name: "Retry photo.png",
    });
    expect(screen.getByText("Upload failed")).toBeVisible();
    expect(
      container.querySelectorAll(".composer-actions .submit-button"),
    ).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Stop ygg" }));
    expect(onInterrupt).toHaveBeenCalledOnce();

    await user.click(retry);
    expect(await screen.findByText("Ready")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Remove photo.png" }));
    expect(
      screen.queryByRole("button", { name: "Remove photo.png" }),
    ).toBeNull();
  });
});
