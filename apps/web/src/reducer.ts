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

type TranscriptItemIndex = ReadonlyMap<string, number>;

const transcriptItemIndexes = new WeakMap<
  readonly TranscriptItem[],
  TranscriptItemIndex
>();

function transcriptItemIndex(
  items: readonly TranscriptItem[],
): TranscriptItemIndex {
  const cached = transcriptItemIndexes.get(items);
  if (cached) return cached;

  const index = new Map<string, number>();
  for (let itemIndex = 0; itemIndex < items.length; itemIndex += 1) {
    const item = items[itemIndex]!;
    if (!index.has(item.id)) index.set(item.id, itemIndex);
  }
  transcriptItemIndexes.set(items, index);
  return index;
}

/**
 * Builds the transcript lookup once when an authoritative snapshot enters the
 * store. Subsequent immutable item arrays inherit the index, so live deltas do
 * not scan the full transcript to locate their target.
 */
export function primeSessionItemIndex(snapshot: SessionSnapshot): void {
  transcriptItemIndex(snapshot.items);
}

function cacheTranscriptItemIndex(
  items: readonly TranscriptItem[],
  index: TranscriptItemIndex,
): void {
  transcriptItemIndexes.set(items, index);
}

function updateItem(
  items: TranscriptItem[],
  itemId: string,
  update: (item: TranscriptItem) => TranscriptItem,
): TranscriptItem[] {
  const index = transcriptItemIndex(items);
  const itemIndex = index.get(itemId);
  if (itemIndex === undefined) return items;

  const current = items[itemIndex]!;
  const updated = update(current);
  if (updated === current) return items;

  const next = items.slice();
  next[itemIndex] = updated;
  cacheTranscriptItemIndex(next, index);
  return next;
}

function upsertItem(
  items: TranscriptItem[],
  incoming: TranscriptItem,
): TranscriptItem[] {
  const index = transcriptItemIndex(items);
  const existingIndex = index.get(incoming.id);
  if (existingIndex === undefined) {
    const next = [...items, incoming];
    const nextIndex = new Map(index);
    nextIndex.set(incoming.id, items.length);
    cacheTranscriptItemIndex(next, nextIndex);
    return next;
  }

  const next = items.slice();
  next[existingIndex] = incoming;
  cacheTranscriptItemIndex(next, index);
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

function contextPercent(snapshot: SessionSnapshot["context"]): number {
  const { contextTokens, contextLimit } = snapshot.usage;
  return contextLimit && contextLimit > 0
    ? Math.min(100, Math.round((contextTokens / contextLimit) * 100))
    : 0;
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
    primeSessionItemIndex(event.snapshot);
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

    case "session.goalChanged":
      return {
        ...snapshot,
        sequence: event.sequence,
      };

    case "session.pullRequestChanged":
      return {
        ...snapshot,
        sequence: event.sequence,
      };

    case "context.updated":
      return {
        ...snapshot,
        sequence: event.sequence,
        context: event.context,
        contextTokens: event.context.usage.contextTokens,
        contextPercent: contextPercent(event.context),
      };

    case "usage.updated": {
      // Legacy usage events carry no category or compaction projection. Keep
      // their token count honest by treating it as unattributed instead of
      // retaining stale or fabricated category precision.
      const context: SessionSnapshot["context"] = {
        ...snapshot.context,
        usage: event.usage,
        status: {
          current: {
            categories:
              event.usage.contextTokens === 0
                ? []
                : [{ category: "other", tokens: event.usage.contextTokens }],
            totalTokens: event.usage.contextTokens,
          },
          updatedAtMs: event.observedAtMs,
        },
      };
      return {
        ...snapshot,
        sequence: event.sequence,
        context,
        contextTokens: event.usage.contextTokens,
        contextPercent: contextPercent(context),
      };
    }

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
      const items = updateItem(snapshot.items, event.itemId, (item) => {
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
      {
        const index = transcriptItemIndex(snapshot.items);
        const itemIndex = index.get(event.itemId);
        if (itemIndex === undefined) {
          return {
            ...snapshot,
            sequence: event.sequence,
          };
        }
        const items = [
          ...snapshot.items.slice(0, itemIndex),
          ...snapshot.items.slice(itemIndex + 1),
        ];
        const nextIndex = new Map<string, number>();
        for (const [itemId, indexValue] of index) {
          if (itemId === event.itemId) continue;
          nextIndex.set(
            itemId,
            indexValue > itemIndex ? indexValue - 1 : indexValue,
          );
        }
        cacheTranscriptItemIndex(items, nextIndex);
        return {
          ...snapshot,
          sequence: event.sequence,
          items,
        };
      }

    case "item.activity":
      return {
        ...snapshot,
        sequence: event.sequence,
        items: updateItem(snapshot.items, event.itemId, (item) =>
          item.kind === "action"
            ? {
                ...item,
                ...event.activity,
                detail:
                  event.activity.outputSummary ??
                  event.activity.summary,
                state:
                  event.activity.status === "running"
                    ? "streaming"
                    : event.activity.status === "failed"
                      ? "failed"
                      : event.activity.status === "stopped"
                        ? "stopped"
                        : "committed",
              }
            : item,
        ),
      };

    case "item.activity_result":
      return {
        ...snapshot,
        sequence: event.sequence,
        items: updateItem(snapshot.items, event.itemId, (item) =>
          item.kind === "action"
            ? {
                ...item,
                ...event.result,
                detail:
                  event.result.outputSummary ?? event.result.summary,
                state:
                  event.result.status === "running"
                    ? "streaming"
                    : event.result.status === "failed"
                      ? "failed"
                      : event.result.status === "stopped"
                        ? "stopped"
                        : "committed",
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

function sameDeltaTarget(
  left: Extract<SessionEvent, { type: "item.delta" }>,
  right: SessionEvent,
  actorGeneration: number,
): right is Extract<SessionEvent, { type: "item.delta" }> {
  return (
    right.type === "item.delta" &&
    right.sessionId === left.sessionId &&
    right.itemId === left.itemId &&
    right.field === left.field &&
    (right.actorGeneration ?? actorGeneration) === actorGeneration
  );
}

/**
 * Reduces an ordered event batch while applying adjacent deltas to the same
 * transcript field with one item-array copy. Every sequence is still
 * validated, and non-delta events retain their original ordering.
 */
export function reduceSessionEvents(
  snapshot: SessionSnapshot,
  events: readonly SessionEvent[],
): SessionSnapshot {
  let next = snapshot;
  let index = 0;

  while (index < events.length) {
    const event = events[index]!;
    const eventGeneration = event.actorGeneration ?? next.actorGeneration;
    if (
      event.type !== "item.delta" ||
      event.sessionId !== next.sessionId ||
      event.sequence !== next.sequence + 1 ||
      eventGeneration !== next.actorGeneration
    ) {
      next = reduceSessionEvent(next, event);
      index += 1;
      continue;
    }

    let lastIndex = index;
    let lastSequence = event.sequence;
    let delta = event.delta;
    let replace = Boolean(event.replace);
    while (lastIndex + 1 < events.length) {
      const candidate = events[lastIndex + 1]!;
      if (
        !sameDeltaTarget(event, candidate, eventGeneration) ||
        candidate.sequence !== lastSequence + 1
      ) {
        break;
      }
      if (candidate.replace) {
        delta = candidate.delta;
        replace = true;
      } else {
        delta += candidate.delta;
      }
      lastSequence = candidate.sequence;
      lastIndex += 1;
    }

    const reduced = reduceSessionEvent(next, {
      ...event,
      delta,
      replace,
    });
    next =
      lastSequence === event.sequence || reduced === next
        ? reduced
        : { ...reduced, sequence: lastSequence };
    index = lastIndex + 1;
  }

  return next;
}
