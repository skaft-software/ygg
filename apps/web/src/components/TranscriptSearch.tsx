import { ChevronRight, LoaderCircle, Search, X } from "lucide-react";
import { Fragment, useEffect, useId, useRef, useState } from "react";
import type {
  SearchMatchRange,
  TranscriptSearchKind,
  TranscriptSearchRequest,
  TranscriptSearchResult,
} from "../protocol";

interface TranscriptSearchProps {
  open: boolean;
  onClose: () => void;
  onSearch: (
    request: TranscriptSearchRequest,
  ) => Promise<TranscriptSearchResult>;
  onActivate: (sessionId: string, itemId: string) => void;
}

const resultLimit = 100;

const kindOptions = [
  { kind: "user", label: "User" },
  { kind: "assistant", label: "Assistant" },
  { kind: "tool", label: "Tool" },
  { kind: "error", label: "Error" },
  { kind: "attachment", label: "Attachment" },
] as const satisfies readonly {
  kind: TranscriptSearchKind;
  label: string;
}[];

const kindLabels: Record<TranscriptSearchKind, string> = {
  user: "User message",
  assistant: "Assistant",
  tool: "Tool",
  error: "Error",
  attachment: "Attachment",
};

function normalizedRanges(
  textLength: number,
  ranges: readonly SearchMatchRange[],
): Array<{ start: number; end: number }> {
  const sorted = ranges
    .filter(
      (range) =>
        Number.isFinite(range.startChar) && Number.isFinite(range.endChar),
    )
    .map((range) => ({
      start: Math.max(0, Math.min(textLength, Math.trunc(range.startChar))),
      end: Math.max(0, Math.min(textLength, Math.trunc(range.endChar))),
    }))
    .filter((range) => range.end > range.start)
    .sort((left, right) => left.start - right.start || left.end - right.end);

  return sorted.reduce<Array<{ start: number; end: number }>>(
    (merged, range) => {
      const previous = merged.at(-1);
      if (previous && range.start <= previous.end) {
        previous.end = Math.max(previous.end, range.end);
      } else {
        merged.push({ ...range });
      }
      return merged;
    },
    [],
  );
}

function HighlightedText({
  text,
  ranges,
}: {
  text: string;
  ranges: readonly SearchMatchRange[];
}) {
  const characters = Array.from(text);
  const highlights = normalizedRanges(characters.length, ranges);
  if (!highlights.length) return text;

  const parts = [];
  let cursor = 0;
  for (const [index, range] of highlights.entries()) {
    if (range.start > cursor) {
      parts.push(
        <Fragment key={`text-${cursor}-${range.start}`}>
          {characters.slice(cursor, range.start).join("")}
        </Fragment>,
      );
    }
    parts.push(
      <mark
        className="transcript-search-highlight"
        key={`match-${range.start}-${range.end}-${index}`}
      >
        {characters.slice(range.start, range.end).join("")}
      </mark>,
    );
    cursor = range.end;
  }
  if (cursor < characters.length) {
    parts.push(
      <Fragment key={`text-${cursor}-${characters.length}`}>
        {characters.slice(cursor).join("")}
      </Fragment>,
    );
  }
  return <>{parts}</>;
}

function timestampMetadata(
  timestampMs: number,
): { dateTime: string; label: string } | null {
  const timestamp = new Date(timestampMs);
  if (Number.isNaN(timestamp.getTime())) return null;
  return {
    dateTime: timestamp.toISOString(),
    label: new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(timestamp),
  };
}

export function TranscriptSearch({
  open,
  onClose,
  onSearch,
  onActivate,
}: TranscriptSearchProps) {
  const titleId = useId();
  const descriptionId = useId();
  const filterLabelId = useId();
  const queryRef = useRef<HTMLInputElement>(null);
  const panelRef = useRef<HTMLElement>(null);
  const onCloseRef = useRef(onClose);
  const requestSequenceRef = useRef(0);
  const [query, setQuery] = useState("");
  const [selectedKinds, setSelectedKinds] = useState<
    ReadonlySet<TranscriptSearchKind>
  >(() => new Set());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<TranscriptSearchResult | null>(null);

  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    if (!open) return;
    const restoreTarget =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    const panel = panelRef.current;
    const focusable = () =>
      Array.from(
        panel?.querySelectorAll<HTMLElement>(
          'button:not([disabled]):not([tabindex="-1"]), input:not([disabled]):not([tabindex="-1"]), select:not([disabled]):not([tabindex="-1"]), textarea:not([disabled]):not([tabindex="-1"]), a[href]:not([tabindex="-1"]), [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );

    queryRef.current?.focus();
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.isComposing) return;
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        requestSequenceRef.current += 1;
        onCloseRef.current();
        return;
      }
      if (event.key !== "Tab") return;

      const targets = focusable();
      if (!targets.length) return;
      const first = targets[0];
      const last = targets.at(-1);
      const active = document.activeElement;
      if (
        !panel?.contains(active) ||
        !(active instanceof HTMLElement) ||
        !targets.includes(active)
      ) {
        event.preventDefault();
        (event.shiftKey ? last : first)?.focus();
      } else if (event.shiftKey && active === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && active === last) {
        event.preventDefault();
        first?.focus();
      }
    };

    document.addEventListener("keydown", onKeyDown, true);
    return () => {
      document.removeEventListener("keydown", onKeyDown, true);
      requestSequenceRef.current += 1;
      if (
        restoreTarget?.isConnected &&
        !restoreTarget.closest("[inert]")
      ) {
        restoreTarget.focus();
      }
    };
  }, [open]);

  if (!open) return null;

  const close = () => {
    requestSequenceRef.current += 1;
    onCloseRef.current();
  };

  const submitSearch = async () => {
    const normalizedQuery = query.trim();
    if (!normalizedQuery || loading) return;

    const kinds = kindOptions
      .filter(({ kind }) => selectedKinds.has(kind))
      .map(({ kind }) => kind);
    const filter: TranscriptSearchRequest["filter"] =
      kinds.length > 0 ? { kinds } : {};
    const request: TranscriptSearchRequest = {
      query: normalizedQuery,
      filter,
      limit: resultLimit,
    };
    const sequence = ++requestSequenceRef.current;
    setLoading(true);
    setError(null);
    setResult(null);

    try {
      const nextResult = await onSearch(request);
      if (sequence !== requestSequenceRef.current) return;
      setResult(nextResult);
    } catch {
      if (sequence !== requestSequenceRef.current) return;
      setError("Conversation search could not be completed. Try again.");
    } finally {
      if (sequence === requestSequenceRef.current) setLoading(false);
    }
  };

  const toggleKind = (kind: TranscriptSearchKind) => {
    setSelectedKinds((current) => {
      const next = new Set(current);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
    setError(null);
  };

  return (
    <div
      className="branch-sheet-backdrop transcript-search-overlay"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) close();
      }}
    >
      <section
        ref={panelRef}
        className="branch-sheet transcript-search-panel"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descriptionId}
      >
        <header>
          <div>
            <span className="branch-sheet-glyph" aria-hidden="true">
              <Search />
            </span>
            <div>
              <h2 id={titleId}>Search conversations</h2>
              <p id={descriptionId}>
                Find visible messages, tool activity, and attachments.
              </p>
            </div>
          </div>
          <button
            className="icon-button"
            type="button"
            onClick={close}
            aria-label="Close conversation search"
          >
            <X aria-hidden="true" />
          </button>
        </header>

        <form
          className="transcript-search-form"
          role="search"
          onSubmit={(event) => {
            event.preventDefault();
            void submitSearch();
          }}
        >
          <label className="sidebar-search transcript-search-query">
            <Search aria-hidden="true" />
            <span className="sr-only">Search conversation transcripts</span>
            <input
              ref={queryRef}
              type="search"
              autoComplete="off"
              maxLength={512}
              value={query}
              placeholder="Search conversations"
              onChange={(event) => {
                setQuery(event.target.value);
                setError(null);
              }}
            />
          </label>
          <button
            className="primary-button"
            type="submit"
            disabled={loading || !query.trim()}
          >
            Search
          </button>
        </form>

        <div
          className="transcript-search-kinds"
          role="group"
          aria-labelledby={filterLabelId}
        >
          <span id={filterLabelId}>Type</span>
          <button
            className="secondary-button transcript-search-kind"
            type="button"
            disabled={loading}
            aria-pressed={selectedKinds.size === 0}
            onClick={() => {
              setSelectedKinds(new Set());
              setError(null);
            }}
          >
            All
          </button>
          {kindOptions.map(({ kind, label }) => (
            <button
              className="secondary-button transcript-search-kind"
              type="button"
              key={kind}
              disabled={loading}
              aria-pressed={selectedKinds.has(kind)}
              onClick={() => toggleKind(kind)}
            >
              {label}
            </button>
          ))}
        </div>

        {loading ? (
          <div
            className="branch-history-empty transcript-search-state is-loading"
            role="status"
          >
            <LoaderCircle className="spin" aria-hidden="true" />
            <span>Searching conversations…</span>
          </div>
        ) : error ? (
          <div
            className="branch-history-empty transcript-search-state is-error"
            role="alert"
          >
            {error}
          </div>
        ) : result ? (
          result.hits.length > 0 ? (
            <>
              <div className="transcript-search-summary" role="status">
                <strong>
                  {result.hits.length}{" "}
                  {result.hits.length === 1 ? "result" : "results"}
                </strong>
                {result.truncated ? (
                  <small>Showing the first {result.hits.length} matches.</small>
                ) : null}
              </div>
              <div
                className="branch-history-list transcript-search-results"
                role="list"
                aria-label="Conversation search results"
              >
                {result.hits.map((hit) => {
                  const title = hit.sessionTitle || "Untitled session";
                  const timestamp = timestampMetadata(hit.timestampMs);
                  return (
                    <div
                      className="completion-review-list transcript-search-result-item"
                      role="listitem"
                      key={`${hit.sessionId}:${hit.itemId}`}
                    >
                      <button
                        className="transcript-search-result"
                        type="button"
                        aria-label={`Open ${kindLabels[hit.kind]} result from ${title}`}
                        onClick={() => {
                          requestSequenceRef.current += 1;
                          onActivate(hit.sessionId, hit.itemId);
                          onCloseRef.current();
                        }}
                      >
                        <Search aria-hidden="true" />
                        <span>
                          <strong>
                            <HighlightedText
                              text={title}
                              ranges={hit.titleMatchRanges}
                            />
                          </strong>
                          <small>
                            {kindLabels[hit.kind]}
                            {timestamp ? (
                              <>
                                {" · "}
                                <time dateTime={timestamp.dateTime}>
                                  {timestamp.label}
                                </time>
                              </>
                            ) : null}
                          </small>
                          <span className="transcript-search-snippet">
                            <HighlightedText
                              text={hit.snippet}
                              ranges={hit.matchRanges}
                            />
                          </span>
                        </span>
                        <ChevronRight aria-hidden="true" />
                      </button>
                    </div>
                  );
                })}
              </div>
            </>
          ) : (
            <div
              className="branch-history-empty transcript-search-state is-empty"
              role="status"
            >
              <Search aria-hidden="true" />
              <strong>No matches found</strong>
              <span>Try a different word or phrase.</span>
            </div>
          )
        ) : (
          <div
            className="branch-history-empty transcript-search-state is-empty"
            role="status"
          >
            <Search aria-hidden="true" />
            <strong>Search across your conversation history</strong>
            <span>Find visible messages, tool activity, and attachments.</span>
          </div>
        )}
      </section>
    </div>
  );
}
