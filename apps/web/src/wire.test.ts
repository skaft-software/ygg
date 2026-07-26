import { describe, expect, it } from "vitest";
import eventEnvelopeGolden from "../../../extensions/ygg-serve/fixtures/event-envelope.json";
import hostBootstrapGolden from "../../../extensions/ygg-serve/fixtures/host-bootstrap.json";
import hostCommandAckGolden from "../../../extensions/ygg-serve/fixtures/host-command-ack.json";
import hostCommandGolden from "../../../extensions/ygg-serve/fixtures/host-command.json";
import sessionCommandGolden from "../../../extensions/ygg-serve/fixtures/session-command.json";
import sessionSnapshotGolden from "../../../extensions/ygg-serve/fixtures/session-snapshot.json";
import {
  decodeWireCommandAck,
  encodeClientCommand,
  projectEventEnvelope,
  projectHostBootstrap,
  projectHostStreamEvent,
  projectSessionSnapshot,
  UnsupportedWireCommandError,
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

  it("fails honestly for UI commands absent from the Rust contract", () => {
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
          id: "command-pin",
          type: "session.pin",
          sessionId: "session-demo",
          pinned: true,
        },
        context,
      ),
    ).toThrow(UnsupportedWireCommandError);
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
