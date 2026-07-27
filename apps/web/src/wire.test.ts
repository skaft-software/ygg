import { describe, expect, it } from "vitest";
import eventEnvelopeGolden from "../../../extensions/ygg-serve/fixtures/event-envelope.json";
import hostBootstrapGolden from "../../../extensions/ygg-serve/fixtures/host-bootstrap.json";
import hostCommandAckGolden from "../../../extensions/ygg-serve/fixtures/host-command-ack.json";
import hostCommandGolden from "../../../extensions/ygg-serve/fixtures/host-command.json";
import liveUserDeliveryGolden from "../../../extensions/ygg-serve/fixtures/live-user-delivery.json";
import sessionCommandGolden from "../../../extensions/ygg-serve/fixtures/session-command.json";
import sessionSnapshotGolden from "../../../extensions/ygg-serve/fixtures/session-snapshot.json";
import {
  decodeWireCommandAck,
  encodeClientCommand,
  projectEventEnvelope,
  projectHostBootstrap,
  projectHostStreamEvent,
  projectSessionSnapshot,
  WireContractError,
} from "./wire";

const clone = <T,>(value: T): T => structuredClone(value);

describe("authoritative Rust wire contract", () => {
  it("projects the complete host bootstrap and embedded selected session", () => {
    const { bootstrap, selectedSession } =
      projectHostBootstrap(hostBootstrapGolden);

    expect(bootstrap.host).toEqual({
      id: "host-demo",
      name: "Achu's Mac",
      connection: "local",
    });
    expect(bootstrap.models[0]).toMatchObject({
      id: "gpt-5.6",
      available: true,
      reasoning: ["low", "medium", "high"],
      defaultReasoning: "high",
      inputModalities: ["text", "image"],
    });
    expect(bootstrap.authorityProfiles).toEqual([
      "readOnly",
      "workspace",
      "fullAccess",
    ]);
    expect(bootstrap.themes[0]?.theme.typography).toEqual({
      body_family: "system-sans",
      mono_family: "system-mono",
      body_size: 17,
      display_ratio_milli: 1235,
    });
    expect(selectedSession).toMatchObject({
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 42,
      modelId: "gpt-5.6",
      reasoning: "high",
      authority: "fullAccess",
      status: "idle",
    });
    expect(selectedSession.items[0]).toMatchObject({
      kind: "assistant_message",
      content: "Ready.",
    });
    expect(bootstrap.capabilities.sessionBranches).toBe(true);
    expect(bootstrap.capabilities.sessionExport).toBe(true);
    expect(selectedSession.branches).toEqual({
      head: "entry-42",
      entries: [
        {
          entryId: "entry-42",
          kind: "assistantMessage",
          checkoutable: true,
          label: "Ready.",
        },
      ],
      truncated: false,
    });
  });

  it("projects the standalone session snapshot against the model catalog", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const summary = bootstrap.sessions[0];
    const snapshot = projectSessionSnapshot(sessionSnapshotGolden, {
      summary,
      models: bootstrap.models,
      timestampMs: 1_721_000_000_042,
    });

    expect(snapshot.sequence).toBe(42);
    expect(snapshot.contextPercent).toBe(0);
    expect(snapshot.title).toBe("New session");
    expect(snapshot.items).toHaveLength(1);
  });

  it("rehydrates durable evidence origins and exact file handles from a snapshot", () => {
    const durable = clone(sessionSnapshotGolden) as unknown as {
      items: unknown[];
      sources?: unknown[];
      artifacts?: unknown[];
    };
    durable.items.push(
      {
        id: "item-tool-read",
        turnId: "turn-evidence",
        lifecycle: "committed",
        durableEntryId: "entry-tool",
        payload: {
          type: "toolCall",
          data: {
            name: "read",
            arguments: { path: "src/theme.ts" },
            droppedProgressBytes: 0,
          },
        },
      },
      {
        id: "item-file-change",
        turnId: "turn-evidence",
        lifecycle: "committed",
        durableEntryId: "entry-result",
        payload: {
          type: "fileChange",
          data: {
            handle: "resource-diff",
            resultHandle: "resource-result",
            displayPath: "src/theme.ts",
            additions: 8,
            deletions: 3,
          },
        },
      },
    );
    durable.sources = [
      {
        id: "source-theme",
        kind: "file",
        title: "src/theme.ts",
        handle: "resource-source",
        originItemId: "item-tool-read",
        consultedAtMs: 1_721_000_000_044,
        cited: false,
        available: true,
      },
    ];
    durable.artifacts = [
      {
        id: "artifact-theme",
        kind: "file",
        name: "theme.ts",
        mediaType: "text/plain",
        handle: "resource-result",
        byteLen: 128,
        originItemId: "item-tool-read",
        available: true,
      },
    ];
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const snapshot = projectSessionSnapshot(durable, {
      summary: bootstrap.sessions[0],
      models: bootstrap.models,
    });

    expect(snapshot.sources[0]).toMatchObject({
      id: "source-theme",
      handle: "resource-source",
      originItemId: "item-tool-read",
    });
    expect(snapshot.outputs[0]).toMatchObject({
      id: "artifact-theme",
      handle: "resource-result",
      originItemId: "item-tool-read",
    });
    expect(
      snapshot.items.find((item) => item.id === "item-file-change"),
    ).toMatchObject({
      kind: "action",
      diffHandle: "resource-diff",
      resultHandle: "resource-result",
    });
  });

  it("accepts omitted branch parents only when truncation is explicit", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const summary = bootstrap.sessions[0];
    const truncated = {
      ...clone(sessionSnapshotGolden),
      durableHead: "entry-recent",
      branches: {
        head: "entry-recent",
        entries: [
          {
            entryId: "entry-recent",
            parentEntryId: "entry-omitted",
            kind: "assistantMessage",
            checkoutable: true,
            label: "Recent answer",
          },
        ],
        truncated: true,
      },
    };
    expect(
      projectSessionSnapshot(truncated, {
        summary,
        models: bootstrap.models,
      }).branches.truncated,
    ).toBe(true);

    const complete = {
      ...truncated,
      branches: { ...truncated.branches, truncated: false },
    };
    expect(() =>
      projectSessionSnapshot(complete, {
        summary,
        models: bootstrap.models,
      }),
    ).toThrow(/parent outside the preserved graph/);
  });

  it("projects the sequenced event and nested host stream envelope", () => {
    const event = projectEventEnvelope(eventEnvelopeGolden);
    expect(event).toEqual({
      type: "item.delta",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 43,
      itemId: "item-stream",
      field: "content",
      delta: " world",
    });

    expect(
      projectHostStreamEvent({
        protocol: 1,
        hostSequence: 12,
        event: eventEnvelopeGolden,
      }),
    ).toEqual({ hostSequence: 12, event });
  });

  it("preserves live user-message delivery semantics", () => {
    const queued = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    queued.cursor.sequence = 44;
    queued.event = {
      type: "item.started",
      data: {
        item: liveUserDeliveryGolden,
      },
    };

    expect(projectEventEnvelope(queued)).toMatchObject({
      type: "item.started",
      item: {
        kind: "user_message",
        content: "Change direction",
        state: "streaming",
        delivery: "steer",
      },
    });
  });

  it("projects complete cross-client catalog summary changes", () => {
    const summary = clone(hostBootstrapGolden.sessions[0]);
    summary.id = "session-from-other-client";
    summary.title = "Created on Achu’s phone";
    summary.attention = "unreadCompletion";

    expect(
      projectHostStreamEvent(
        {
          protocol: 1,
          hostSequence: 13,
          catalog: {
            catalogCursor: 9,
            summary,
          },
        },
        { models: projectHostBootstrap(hostBootstrapGolden).bootstrap.models },
      ),
    ).toMatchObject({
      hostSequence: 13,
      event: {
        type: "catalog.summary",
        catalogRevision: 9,
        summary: {
          id: "session-from-other-client",
          title: "Created on Achu’s phone",
          unread: true,
          attentionCount: 0,
        },
      },
    });
  });

  it("preserves opaque resource handles for safe source and artifact viewers", () => {
    const source = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    source.cursor.sequence = 44;
    source.event = {
      type: "source.upserted",
      data: {
        source: {
          id: "source-theme",
          kind: "file",
          title: "theme.ts",
          handle: "resource-source-theme",
          originItemId: "item-tool-theme",
          consultedAtMs: 1_721_000_000_044,
          cited: false,
          available: true,
        },
      },
    };
    expect(projectEventEnvelope(source)).toMatchObject({
      type: "session.resources",
      sources: [
        {
          id: "source-theme",
          handle: "resource-source-theme",
          originItemId: "item-tool-theme",
          available: true,
        },
      ],
    });

    const artifact = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    artifact.cursor.sequence = 45;
    artifact.event = {
      type: "artifact.upserted",
      data: {
        artifact: {
          id: "artifact-report",
          kind: "document",
          name: "report.md",
          mediaType: "text/markdown",
          handle: "resource-artifact-report",
          originItemId: "item-tool-report",
          byteLen: 128,
          available: true,
        },
      },
    };
    expect(projectEventEnvelope(artifact)).toMatchObject({
      type: "session.resources",
      outputs: [
        {
          id: "artifact-report",
          handle: "resource-artifact-report",
          originItemId: "item-tool-report",
          mimeType: "text/markdown",
          available: true,
        },
      ],
    });
  });

  it("preserves exact diff and resulting-file handles", () => {
    const changed = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    changed.cursor.sequence = 46;
    changed.event = {
      type: "item.committed",
      data: {
        item: {
          id: "item-file-change",
          turnId: "turn-change",
          lifecycle: "committed",
          durableEntryId: "entry-change",
          payload: {
            type: "fileChange",
            data: {
              handle: "resource-exact-diff",
              resultHandle: "resource-exact-result",
              displayPath: "src/theme.ts",
              additions: 8,
              deletions: 3,
            },
          },
        },
      },
    };

    expect(projectEventEnvelope(changed)).toMatchObject({
      type: "item.committed",
      item: {
        kind: "action",
        actionKind: "file_write",
        target: "src/theme.ts",
        additions: 8,
        deletions: 3,
        diffHandle: "resource-exact-diff",
        resultHandle: "resource-exact-result",
      },
    });
  });

  it("projects the exact durable branch head", () => {
    const durable = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    durable.cursor.sequence = 44;
    durable.event = {
      type: "session.durableHeadChanged",
      data: { durableEntryId: "entry-44" },
    };

    expect(projectEventEnvelope(durable)).toEqual({
      type: "session.durableHeadChanged",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      durableHead: "entry-44",
    });
  });

  it("preserves active-run identity while a session needs attention", () => {
    const waiting = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    waiting.cursor.sequence = 44;
    waiting.event = {
      type: "session.stateChanged",
      data: { state: "needsInput", activeRunId: "run-live" },
    };

    expect(projectEventEnvelope(waiting)).toEqual({
      type: "session.updated",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      patch: {
        status: "needs_attention",
        activeRunId: "run-live",
      },
    });
  });

  it("projects and encodes durable session metadata changes", () => {
    const metadata = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    metadata.cursor.sequence = 44;
    metadata.event = {
      type: "session.metadataChanged",
      data: {
        title: "Renamed session",
        pinned: true,
        archived: false,
      },
    };
    expect(projectEventEnvelope(metadata)).toEqual({
      type: "session.updated",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      patch: { title: "Renamed session" },
    });

    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-rename",
          type: "session.rename",
          sessionId: "session-demo",
          title: "Renamed session",
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.rename",
        data: { title: "Renamed session" },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-pin",
          type: "session.pin",
          sessionId: "session-demo",
          pinned: true,
        },
        context,
      ),
    ).toMatchObject({
      command: { type: "session.pin", data: { pinned: true } },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-archive",
          type: "session.archive",
          sessionId: "session-demo",
          archived: true,
        },
        context,
      ),
    ).toMatchObject({
      command: { type: "session.archive", data: { archived: true } },
    });
  });

  it("projects and answers typed user-input requests", () => {
    const request = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      timestampMs: number;
      event: unknown;
    };
    request.cursor.sequence = 44;
    request.timestampMs = 1_721_000_000_044;
    request.event = {
      type: "request.changed",
      data: {
        request: {
          id: "request-input",
          actorGeneration: 3,
          kind: {
            type: "userInput",
            data: {
              prompt: "Which layout should I keep?",
              choices: ["Compact", "Comfortable"],
            },
          },
          state: "pending",
        },
      },
    };

    expect(projectEventEnvelope(request)).toMatchObject({
      type: "item.committed",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      item: {
        id: "request-request-input",
        kind: "user_input_request",
        requestId: "request-input",
        prompt: "Which layout should I keep?",
        choices: ["Compact", "Comfortable"],
        state: "streaming",
      },
    });

    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-input-choice",
          type: "userInput.resolve",
          sessionId: "session-demo",
          requestId: "request-input",
          answer: { type: "choice", choice: "Compact" },
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: {
        type: "session.answerRequest",
        data: {
          requestId: "request-input",
          answer: {
            type: "choice",
            data: { choice: "Compact" },
          },
        },
      },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-input-text",
          type: "userInput.resolve",
          sessionId: "session-demo",
          requestId: "request-input",
          answer: { type: "text", text: "Use the denser option." },
        },
        context,
      ),
    ).toMatchObject({
      command: {
        type: "session.answerRequest",
        data: {
          answer: {
            type: "text",
            data: { text: "Use the denser option." },
          },
        },
      },
    });
  });

  it("coalesces linked tool results into their action cell", () => {
    const toolCall = {
      id: "tool-call-1",
      turnId: "turn-tool",
      lifecycle: "committed",
      durableEntryId: "entry-tool-call",
      payload: {
        type: "toolCall",
        data: {
          name: "shell",
          arguments: { command: "npm test" },
          progress: "Running tests",
          droppedProgressBytes: 0,
        },
      },
    };
    const toolResult = {
      id: "tool-result-1",
      turnId: "turn-tool",
      lifecycle: "committed",
      durableEntryId: "entry-tool-result",
      payload: {
        type: "toolResult",
        data: {
          toolCallItemId: "tool-call-1",
          content: "43 tests passed",
          isError: false,
        },
      },
    };
    const snapshot = {
      ...sessionSnapshotGolden,
      items: [toolCall, toolResult],
    };

    expect(
      projectSessionSnapshot(snapshot, {
        summary: projectHostBootstrap(hostBootstrapGolden).bootstrap.sessions[0],
        models: projectHostBootstrap(hostBootstrapGolden).bootstrap.models,
      }).items,
    ).toEqual([
      expect.objectContaining({
        id: "tool-call-1",
        kind: "action",
        label: "shell",
        detail: "43 tests passed",
        state: "committed",
      }),
    ]);

    const resultEvent = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    resultEvent.cursor.sequence = 44;
    resultEvent.event = {
      type: "item.committed",
      data: { item: toolResult },
    };
    expect(projectEventEnvelope(resultEvent)).toMatchObject({
      type: "item.tool_result",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      itemId: "tool-call-1",
      resultItemId: "tool-result-1",
      detail: "43 tests passed",
      state: "committed",
    });
  });

  it("encodes the exact host command golden", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const encoded = encodeClientCommand(
      {
        id: "command-create",
        type: "session.create",
        projectId: "project-ygg",
        modelId: "gpt-5.6",
        reasoning: "high",
        authority: "fullAccess",
      },
      {
        hostId: "host-demo",
        deviceId: "device-browser",
        issuedAtMs: 1_721_000_000_060,
        actorGenerationBySession: {},
        modelIdBySession: {},
        models: bootstrap.models,
      },
    );

    expect(encoded).toEqual(hostCommandGolden);
  });

  it("encodes the exact generation-bound session command golden", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const encoded = encodeClientCommand(
      {
        id: "command-submit",
        type: "session.submit",
        sessionId: "session-demo",
        prompt: "Review this image",
        attachments: [
          {
            id: "image-1",
            handle: "upload:image-1",
            name: "alignment.png",
            mediaType: "image/png",
            size: 98_765,
          },
        ],
      },
      {
        hostId: "host-demo",
        deviceId: "device-browser",
        issuedAtMs: 1_721_000_000_050,
        actorGenerationBySession: { "session-demo": 3 },
        modelIdBySession: { "session-demo": "gpt-5.6" },
        models: bootstrap.models,
      },
    );

    expect(encoded).toEqual(sessionCommandGolden);
  });

  it("encodes explicit active-run steer and follow-up commands", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_052,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    const input = {
      text: "Use the smaller layout",
      attachments: [],
    };

    expect(
      encodeClientCommand(
        {
          id: "command-steer",
          type: "session.steer",
          sessionId: "session-demo",
          prompt: input.text,
          attachments: [],
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: { type: "session.steer", data: { input } },
    });
    expect(
      encodeClientCommand(
        {
          id: "command-follow-up",
          type: "session.followUp",
          sessionId: "session-demo",
          prompt: input.text,
          attachments: [],
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: { type: "session.followUp", data: { input } },
    });
  });

  it("encodes the exact durable checkout command and replacement signal", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_053,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };
    expect(
      encodeClientCommand(
        {
          id: "command-checkout",
          type: "session.checkout",
          sessionId: "session-demo",
          entryId: "entry-17",
        },
        context,
      ),
    ).toMatchObject({
      expectedActorGeneration: 3,
      command: {
        type: "session.checkout",
        data: { entryId: "entry-17" },
      },
    });

    const replacement = clone(eventEnvelopeGolden) as unknown as {
      cursor: { sequence: number };
      event: unknown;
    };
    replacement.cursor.sequence = 44;
    replacement.event = {
      type: "session.projectionReplaced",
      data: { durableEntryId: "entry-17" },
    };
    expect(projectEventEnvelope(replacement)).toEqual({
      type: "session.projectionReplaced",
      sessionId: "session-demo",
      actorGeneration: 3,
      sequence: 44,
      durableHead: "entry-17",
    });
  });

  it("decodes the exact host acknowledgement golden and session acks", () => {
    expect(decodeWireCommandAck(hostCommandAckGolden)).toEqual({
      commandId: "command-create",
      accepted: true,
      createdSessionId: "session-created",
    });
    expect(
      decodeWireCommandAck({
        protocol: 1,
        sessionId: "session-demo",
        commandId: "command-submit",
        acknowledgedAtMs: 1_721_000_000_051,
        cursor: { actorGeneration: 3, sequence: 43 },
        disposition: { status: "accepted", runId: "run-1" },
      }),
    ).toEqual({
      commandId: "command-submit",
      accepted: true,
      createdSessionId: undefined,
    });
  });

  it("accepts provider-defined bounded reasoning from the host catalog", () => {
    const golden = clone(hostBootstrapGolden);
    golden.models[0]!.reasoning = ["off", "on", "budget=8192"];
    golden.models[0]!.defaultReasoning = "budget=8192";
    golden.sessions[0]!.model.reasoning = "budget=8192";
    golden.selectedSession.model.reasoning = "budget=8192";

    const { bootstrap, selectedSession } = projectHostBootstrap(golden);
    expect(bootstrap.models[0]?.reasoning).toEqual([
      "off",
      "on",
      "budget=8192",
    ]);
    expect(selectedSession.reasoning).toBe("budget=8192");
  });

  it("rejects unknown wire fields and catalog-invalid selections", () => {
    expect(() =>
      projectHostBootstrap({
        ...hostBootstrapGolden,
        injected: true,
      }),
    ).toThrow(WireContractError);

    const invalid = clone(hostBootstrapGolden);
    invalid.selectedSession.model.reasoning = "provider-secret";
    expect(() => projectHostBootstrap(invalid)).toThrow(
      /is not advertised by the selected model/,
    );
  });

  it("fails honestly for approval scopes absent from the Rust contract", () => {
    const { bootstrap } = projectHostBootstrap(hostBootstrapGolden);
    const context = {
      hostId: "host-demo",
      deviceId: "device-browser",
      issuedAtMs: 1_721_000_000_060,
      actorGenerationBySession: { "session-demo": 3 },
      modelIdBySession: { "session-demo": "gpt-5.6" },
      models: bootstrap.models,
    };

    expect(() =>
      encodeClientCommand(
        {
          id: "command-approval",
          type: "approval.resolve",
          sessionId: "session-demo",
          requestId: "request-1",
          decision: "allowed_session",
        },
        context,
      ),
    ).toThrow(/one-shot approval only/);
  });
});
