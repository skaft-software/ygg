import type {
  SessionBranchEntry,
  SessionEvent,
  SessionSnapshot,
  TranscriptItem,
} from "./protocol";

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

export class SessionGenerationMismatchError extends Error {
  constructor(
    readonly sessionId: string,
    readonly expected: number,
    readonly received: number,
  ) {
    super(
      `Session ${sessionId} expected actor generation ${expected}, received ${received}.`,
    );
    this.name = "SessionGenerationMismatchError";
  }
}

export class SessionBranchGraphError extends Error {
  constructor(readonly sessionId: string, message: string) {
    super(`Session ${sessionId} branch graph ${message}.`);
    this.name = "SessionBranchGraphError";
  }
}

export class SessionProjectionReplacementRequiredError extends Error {
  constructor(readonly sessionId: string) {
    super(`Session ${sessionId} requires a complete projection replacement.`);
    this.name = "SessionProjectionReplacementRequiredError";
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

function appendBranchEntries(
  snapshot: SessionSnapshot,
  entries: SessionBranchEntry[],
): SessionSnapshot["branches"] {
  const ids = new Set(snapshot.branches.entries.map((entry) => entry.entryId));
  for (const entry of entries) {
    if (ids.has(entry.entryId)) {
      throw new SessionBranchGraphError(
        snapshot.sessionId,
        `contains duplicate entry ${entry.entryId}`,
      );
    }
    ids.add(entry.entryId);
  }
  for (const entry of entries) {
    if (
      !snapshot.branches.truncated &&
      entry.parentEntryId !== undefined &&
      !ids.has(entry.parentEntryId)
    ) {
      throw new SessionBranchGraphError(
        snapshot.sessionId,
        `is missing parent ${entry.parentEntryId}`,
      );
    }
  }
  const combined = [...snapshot.branches.entries, ...entries];
  if (combined.length <= 2_048) {
    return { ...snapshot.branches, entries: combined };
  }
  const selectedHead = combined.find(
    (entry) => entry.entryId === snapshot.branches.head,
  );
  const recent = combined.slice(-2_048);
  if (
    selectedHead !== undefined &&
    !recent.some((entry) => entry.entryId === selectedHead.entryId)
  ) {
    recent.shift();
    recent.unshift(selectedHead);
  }
  return {
    ...snapshot.branches,
    entries: recent,
    truncated: true,
  };
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
      event.snapshot.sequence !== event.sequence ||
      (event.actorGeneration !== undefined &&
        event.snapshot.actorGeneration !== event.actorGeneration)
    ) {
      return snapshot;
    }
    if (event.snapshot.actorGeneration < snapshot.actorGeneration) {
      return snapshot;
    }
    if (
      event.snapshot.actorGeneration === snapshot.actorGeneration &&
      event.sequence < snapshot.sequence
    ) {
      return snapshot;
    }
    return event.snapshot;
  }

  if (event.sequence <= snapshot.sequence) {
    return snapshot;
  }

  const eventGeneration =
    event.actorGeneration ?? snapshot.actorGeneration;
  if (eventGeneration < snapshot.actorGeneration) {
    return snapshot;
  }
  if (eventGeneration > snapshot.actorGeneration) {
    throw new SessionGenerationMismatchError(
      snapshot.sessionId,
      snapshot.actorGeneration,
      eventGeneration,
    );
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

    case "session.branchEntriesAppended":
      return {
        ...snapshot,
        sequence: event.sequence,
        branches: appendBranchEntries(snapshot, event.entries),
      };

    case "session.durableHeadChanged":
      if (
        event.durableHead !== undefined &&
        !snapshot.branches.entries.some(
          (entry) => entry.entryId === event.durableHead,
        )
      ) {
        throw new SessionBranchGraphError(
          snapshot.sessionId,
          `cannot select missing head ${event.durableHead}`,
        );
      }
      return {
        ...snapshot,
        sequence: event.sequence,
        branches: {
          ...snapshot.branches,
          head: event.durableHead,
        },
      };

    case "session.projectionReplaced":
      throw new SessionProjectionReplacementRequiredError(snapshot.sessionId);

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
          return {
            ...item,
            content: event.replace
              ? event.delta
              : `${item.content}${event.delta}`,
          };
        }

        if (
          event.field === "detail" &&
          item.kind === "action"
        ) {
          return {
            ...item,
            detail: event.replace
              ? event.delta
              : `${item.detail ?? ""}${event.delta}`,
          };
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

    case "item.tool_result":
      return {
        ...snapshot,
        sequence: event.sequence,
        items: snapshot.items.map((item) =>
          item.id === event.itemId && item.kind === "action"
            ? {
                ...item,
                detail: event.detail,
                state: event.state,
              }
            : item,
        ),
      };

    case "session.resources":
      if (event.merge) {
        const mergeById = <T extends { id: string }>(
          current: T[],
          incoming: T[] | undefined,
        ) => {
          if (!incoming) return current;
          const merged = new Map(current.map((item) => [item.id, item]));
          for (const item of incoming) merged.set(item.id, item);
          return [...merged.values()];
        };
        return {
          ...snapshot,
          sequence: event.sequence,
          progress: mergeById(snapshot.progress, event.progress),
          sources: mergeById(snapshot.sources, event.sources),
          outputs: mergeById(snapshot.outputs, event.outputs),
          previews: mergeById(snapshot.previews, event.previews),
        };
      }
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
