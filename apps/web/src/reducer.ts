import type { SessionEvent, SessionSnapshot, TranscriptItem } from "./protocol";

export class SessionSequenceGapError extends Error {
  constructor(
    readonly sessionId: string,
    readonly expected: number,
    readonly received: number,
  ) {
    super(
      `Session ${sessionId} expected sequence ${expected}, received ${received}.`,
    );
    this.name = "SessionSequenceGapError";
  }
}

function upsertItem(
  items: TranscriptItem[],
  incoming: TranscriptItem,
): TranscriptItem[] {
  const existingIndex = items.findIndex((item) => item.id === incoming.id);
  if (existingIndex === -1) {
    return [...items, incoming];
  }

  const next = [...items];
  next[existingIndex] = incoming;
  return next;
}

export function reduceSessionEvent(
  snapshot: SessionSnapshot,
  event: SessionEvent,
): SessionSnapshot {
  if (event.sessionId !== snapshot.sessionId) {
    return snapshot;
  }

  if (event.type === "session.snapshot") {
    if (
      event.snapshot.sessionId !== event.sessionId ||
      event.snapshot.sequence !== event.sequence
    ) {
      return snapshot;
    }
    if (event.sequence < snapshot.sequence) {
      return snapshot;
    }
    return event.snapshot;
  }

  if (event.sequence <= snapshot.sequence) {
    return snapshot;
  }

  if (event.sequence !== snapshot.sequence + 1) {
    throw new SessionSequenceGapError(
      snapshot.sessionId,
      snapshot.sequence + 1,
      event.sequence,
    );
  }

  switch (event.type) {
    case "session.updated":
      return {
        ...snapshot,
        ...event.patch,
        sequence: event.sequence,
      };

    case "item.started":
    case "item.committed":
      return {
        ...snapshot,
        sequence: event.sequence,
        items: upsertItem(snapshot.items, event.item),
      };

    case "item.delta": {
      const items = snapshot.items.map((item) => {
        if (item.id !== event.itemId) {
          return item;
        }

        if (event.field === "content" && "content" in item) {
          return { ...item, content: `${item.content}${event.delta}` };
        }

        if (
          event.field === "detail" &&
          item.kind === "action"
        ) {
          return { ...item, detail: `${item.detail ?? ""}${event.delta}` };
        }

        return item;
      });

      return {
        ...snapshot,
        sequence: event.sequence,
        items,
      };
    }

    case "item.retracted":
      return {
        ...snapshot,
        sequence: event.sequence,
        items: snapshot.items.filter((item) => item.id !== event.itemId),
      };

    case "session.resources":
      return {
        ...snapshot,
        sequence: event.sequence,
        progress: event.progress ?? snapshot.progress,
        sources: event.sources ?? snapshot.sources,
        outputs: event.outputs ?? snapshot.outputs,
        previews: event.previews ?? snapshot.previews,
      };
  }
}
