import {
  Check,
  Download,
  File,
  FileText,
  Globe2,
  LoaderCircle,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { getFixturePreviewMarkup } from "../fixtures";
import type { OutputRef, PreviewRef, SessionSnapshot, SourceRef } from "../protocol";

export type InspectorSelection =
  | { type: "output"; id: string }
  | { type: "source"; id: string }
  | {
      type: "resource";
      handle: string;
      title: string;
      presentation: "text" | "diff" | "image";
    };

interface InspectorProps {
  session: SessionSnapshot;
  selection: InspectorSelection | null;
  closing: boolean;
  modal: boolean;
  previewsAvailable: boolean;
  resourceContentUrl: (sessionId: string, handle: string) => string;
  onRestoreFocus: () => void;
  onClose: () => void;
}

const maxRenderedDiffLines = 5_000;

function boundedDiffLines(text: string): {
  lines: string[];
  truncated: boolean;
} {
  const lines: string[] = [];
  let start = 0;
  while (lines.length < maxRenderedDiffLines) {
    const newline = text.indexOf("\n", start);
    if (newline === -1) {
      lines.push(text.slice(start));
      return { lines, truncated: false };
    }
    lines.push(text.slice(start, newline));
    start = newline + 1;
  }
  return { lines, truncated: start < text.length };
}

interface SplitDiffRow {
  kind: "context" | "change" | "hunk" | "meta";
  oldNumber?: number;
  newNumber?: number;
  oldText?: string;
  newText?: string;
  hunkIndex?: number;
}

function splitDiffRows(lines: string[]): SplitDiffRow[] {
  const rows: SplitDiffRow[] = [];
  let oldNumber = 0;
  let newNumber = 0;
  let hunkIndex = -1;
  let removals: Array<{ number: number; text: string }> = [];
  let additions: Array<{ number: number; text: string }> = [];
  const flushChanges = () => {
    const count = Math.max(removals.length, additions.length);
    for (let index = 0; index < count; index += 1) {
      const removal = removals[index];
      const addition = additions[index];
      rows.push({
        kind: "change",
        oldNumber: removal?.number,
        newNumber: addition?.number,
        oldText: removal?.text,
        newText: addition?.text,
      });
    }
    removals = [];
    additions = [];
  };

  for (const line of lines) {
    if (line.startsWith("@@")) {
      flushChanges();
      const coordinates = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      oldNumber = Number(coordinates?.[1] ?? 0);
      newNumber = Number(coordinates?.[2] ?? 0);
      hunkIndex += 1;
      rows.push({
        kind: "hunk",
        oldText: line,
        newText: line,
        hunkIndex,
      });
      continue;
    }
    if (line.startsWith("-") && !line.startsWith("---")) {
      removals.push({ number: oldNumber, text: line.slice(1) });
      oldNumber += 1;
      continue;
    }
    if (line.startsWith("+") && !line.startsWith("+++")) {
      additions.push({ number: newNumber, text: line.slice(1) });
      newNumber += 1;
      continue;
    }

    flushChanges();
    if (line.startsWith(" ")) {
      rows.push({
        kind: "context",
        oldNumber,
        newNumber,
        oldText: line.slice(1),
        newText: line.slice(1),
      });
      oldNumber += 1;
      newNumber += 1;
    } else {
      rows.push({ kind: "meta", oldText: line, newText: line });
    }
  }
  flushChanges();
  return rows;
}

function DiffPreview({ text }: { text: string }) {
  const preview = useMemo(() => boundedDiffLines(text), [text]);
  const [mode, setMode] = useState<"unified" | "split">("unified");
  const [activeHunk, setActiveHunk] = useState(0);
  const hunks = useMemo(
    () =>
      preview.lines.flatMap((line, lineIndex) =>
        line.startsWith("@@") ? [{ label: line, lineIndex }] : [],
      ),
    [preview.lines],
  );
  const splitRows = useMemo(
    () => splitDiffRows(preview.lines),
    [preview.lines],
  );
  const hunkByLine = useMemo(
    () => new Map(hunks.map((hunk, index) => [hunk.lineIndex, index])),
    [hunks],
  );
  const visitHunk = (index: number) => {
    const next = Math.max(0, Math.min(index, hunks.length - 1));
    setActiveHunk(next);
    document
      .getElementById(`diff-hunk-${next}`)
      ?.scrollIntoView?.({ block: "center" });
  };

  return (
    <>
      <div className="diff-toolbar" aria-label="Diff controls">
        <div className="diff-mode" role="group" aria-label="Diff layout">
          <button
            type="button"
            aria-pressed={mode === "unified"}
            onClick={() => setMode("unified")}
          >
            Unified
          </button>
          <button
            type="button"
            aria-pressed={mode === "split"}
            onClick={() => setMode("split")}
          >
            Split
          </button>
        </div>
        {hunks.length ? (
          <div className="diff-hunk-navigation">
            <button
              type="button"
              disabled={activeHunk === 0}
              onClick={() => visitHunk(activeHunk - 1)}
              aria-label="Previous change"
            >
              Previous
            </button>
            <span aria-live="polite">
              Change {activeHunk + 1} of {hunks.length}
            </span>
            <button
              type="button"
              disabled={activeHunk === hunks.length - 1}
              onClick={() => visitHunk(activeHunk + 1)}
              aria-label="Next change"
            >
              Next
            </button>
          </div>
        ) : null}
      </div>
      {mode === "unified" ? (
        <pre className="opaque-resource-text is-diff">
          {preview.lines.map((line, index) => {
            const hunkIndex = hunkByLine.get(index);
            return (
              <span
                key={index}
                id={
                  hunkIndex === undefined
                    ? undefined
                    : `diff-hunk-${hunkIndex}`
                }
                className={
                  line.startsWith("+") && !line.startsWith("+++")
                    ? "is-addition"
                    : line.startsWith("-") && !line.startsWith("---")
                      ? "is-deletion"
                      : line.startsWith("@@")
                        ? "is-hunk"
                        : undefined
                }
              >
                {line || " "}
                {"\n"}
              </span>
            );
          })}
        </pre>
      ) : (
        <div className="split-diff-scroll">
          <table className="split-diff">
            <thead className="sr-only">
              <tr>
                <th>Old line</th>
                <th>Before</th>
                <th>New line</th>
                <th>After</th>
              </tr>
            </thead>
            <tbody>
              {splitRows.map((row, index) =>
                row.kind === "hunk" || row.kind === "meta" ? (
                  <tr
                    key={index}
                    id={
                      row.hunkIndex === undefined
                        ? undefined
                        : `diff-hunk-${row.hunkIndex}`
                    }
                    className={`is-${row.kind}`}
                  >
                    <td colSpan={4}>{row.oldText || " "}</td>
                  </tr>
                ) : (
                  <tr key={index} className={`is-${row.kind}`}>
                    <td className="line-number">{row.oldNumber ?? ""}</td>
                    <td className={row.oldText === undefined ? "is-empty" : "is-deletion"}>
                      {row.oldText ?? ""}
                    </td>
                    <td className="line-number">{row.newNumber ?? ""}</td>
                    <td className={row.newText === undefined ? "is-empty" : "is-addition"}>
                      {row.newText ?? ""}
                    </td>
                  </tr>
                ),
              )}
            </tbody>
          </table>
        </div>
      )}
      {preview.truncated ? (
        <p className="opaque-resource-truncation" role="status">
          Preview limited to the first {maxRenderedDiffLines.toLocaleString()}{" "}
          lines. Download the diff to inspect the rest.
        </p>
      ) : null}
    </>
  );
}

function PreviewToolbar({ preview }: { preview: PreviewRef }) {
  return (
    <div className="preview-browser-bar">
      <div className="preview-address">
        <span className={`preview-live-dot is-${preview.status}`} />
        <span>{preview.urlLabel ?? preview.title}</span>
      </div>
    </div>
  );
}

function WebPreview({ preview }: { preview: PreviewRef }) {
  // Fixture HTML exists only for local development and E2E. The compile-time
  // guard lets the production bundler remove the resolver and its fixture data.
  const fixtureMarkup = import.meta.env.DEV && preview.fixtureId
    ? getFixturePreviewMarkup(preview.fixtureId)
    : undefined;
  return (
    <div className="web-preview">
      <PreviewToolbar preview={preview} />
      {fixtureMarkup ? (
        <iframe
          title={preview.title}
          sandbox=""
          referrerPolicy="no-referrer"
          srcDoc={fixtureMarkup}
        />
      ) : (
        <div className="preview-unavailable">
          <Globe2 aria-hidden="true" />
          <strong>Preview connected</strong>
          <span>{preview.urlLabel}</span>
        </div>
      )}
    </div>
  );
}

function DocumentPreview({ output }: { output: OutputRef }) {
  return (
    <div className="document-preview">
      <article>
        <span className="document-kicker">ygg output</span>
        <div className="document-content">
          {(output.content ?? "This output is ready to inspect.")
            .split("\n")
            .map((line, index) =>
              line.startsWith("# ") ? (
                <h1 key={`${line}-${index}`}>{line.slice(2)}</h1>
              ) : line.startsWith("- ") ? (
                <p key={`${line}-${index}`} className="document-list-row">
                  <Check aria-hidden="true" />
                  {line.slice(2)}
                </p>
              ) : line ? (
                <p key={`${line}-${index}`}>{line}</p>
              ) : null,
            )}
        </div>
      </article>
    </div>
  );
}

function ResourceContent({
  title,
  mediaType,
  url,
  sessionId,
  handle,
  presentation = "text",
}: {
  title: string;
  mediaType?: string;
  url: string;
  sessionId: string;
  handle: string;
  presentation?: "text" | "diff" | "image";
}) {
  const [preview, setPreview] = useState<
    | { state: "loading" }
    | { state: "ready"; text: string }
    | { state: "unavailable"; message: string }
  >({ state: "loading" });
  useEffect(() => {
    if (presentation === "image") return;
    const controller = new AbortController();
    const maxPreviewBytes = 8 * 1024 * 1024;
    const publish = (
      next:
        | { state: "ready"; text: string }
        | { state: "unavailable"; message: string },
    ) => {
      if (!controller.signal.aborted) setPreview(next);
    };
    void (async () => {
      try {
        const response = await fetch(
          `/api/v1/sessions/${encodeURIComponent(sessionId)}/resources/${encodeURIComponent(handle)}`,
          {
            credentials: "same-origin",
            cache: "no-store",
            redirect: "error",
            signal: controller.signal,
            headers: {
              Accept: "text/plain, application/octet-stream;q=0.8",
            },
          },
        );
        if (!response.ok) {
          const message =
            response.status === 401
              ? "Reconnect to ygg to inspect this resource."
              : response.status === 404
                ? "This resource is not part of the selected task."
                : response.status === 410
                  ? "This resource is no longer available."
                  : "This resource could not be loaded.";
          publish({ state: "unavailable", message });
          return;
        }
        const declaredHeader = response.headers.get("content-length");
        const declaredLength =
          declaredHeader === null ? null : Number(declaredHeader);
        if (
          declaredLength !== null &&
          Number.isFinite(declaredLength) &&
          declaredLength > maxPreviewBytes
        ) {
          publish({
            state: "unavailable",
            message: "This file is too large to preview. Download it instead.",
          });
          return;
        }
        const reader = response.body?.getReader();
        const chunks: Uint8Array[] = [];
        let byteLength = 0;
        if (reader) {
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            byteLength += value.byteLength;
            if (byteLength > maxPreviewBytes) {
              await reader.cancel();
              publish({
                state: "unavailable",
                message:
                  "This file is too large to preview. Download it instead.",
              });
              return;
            }
            chunks.push(value);
          }
        } else {
          const bytes = new Uint8Array(await response.arrayBuffer());
          if (bytes.byteLength > maxPreviewBytes) {
            publish({
              state: "unavailable",
              message:
                "This file is too large to preview. Download it instead.",
            });
            return;
          }
          chunks.push(bytes);
          byteLength = bytes.byteLength;
        }
        const bytes = new Uint8Array(byteLength);
        let offset = 0;
        for (const chunk of chunks) {
          bytes.set(chunk, offset);
          offset += chunk.byteLength;
        }
        let text: string;
        try {
          text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        } catch {
          publish({
            state: "unavailable",
            message: "This binary file is available to download.",
          });
          return;
        }
        if (bytes.includes(0)) {
          publish({
            state: "unavailable",
            message: "This binary file is available to download.",
          });
          return;
        }
        publish({ state: "ready", text });
      } catch (error) {
        if (controller.signal.aborted) return;
        publish({
          state: "unavailable",
          message:
            error instanceof TypeError
              ? "The resource connection was interrupted."
              : "This resource could not be loaded.",
        });
      }
    })();
    return () => controller.abort();
  }, [handle, presentation, sessionId]);

  return (
    <section className="opaque-resource">
      <div className="opaque-resource-heading">
        <div>
          <span>Content</span>
          <small>{mediaType ?? "Sandboxed workspace resource"}</small>
        </div>
        <a
          href={url}
          download={title}
          rel="noreferrer noopener"
          aria-label={`Download ${title}`}
        >
          <Download aria-hidden="true" />
          <span>Download</span>
        </a>
      </div>
      {presentation === "image" ? (
        <div className="opaque-resource-image">
          <img src={url} alt={title} />
        </div>
      ) : preview.state === "loading" ? (
        <div className="opaque-resource-state" role="status">
          <LoaderCircle className="is-spinning" aria-hidden="true" />
          Loading resource…
        </div>
      ) : preview.state === "unavailable" ? (
        <div className="opaque-resource-state" role="status">
          <FileText aria-hidden="true" />
          {preview.message}
        </div>
      ) : presentation === "diff" ? (
        <DiffPreview text={preview.text} />
      ) : (
        <pre className="opaque-resource-text">{preview.text}</pre>
      )}
    </section>
  );
}

function SourceInspector({
  source,
  resourceUrl,
  sessionId,
}: {
  source: SourceRef;
  resourceUrl?: string;
  sessionId: string;
}) {
  return (
    <div className="source-inspector">
      <div className="source-hero">
        <span className="source-file-icon">
          {source.kind === "web" ? (
            <Globe2 aria-hidden="true" />
          ) : source.kind === "documentation" ? (
            <FileText aria-hidden="true" />
          ) : (
            <File aria-hidden="true" />
          )}
        </span>
        <div>
          <span>Consulted source</span>
          <h2>{source.title}</h2>
          <p>{source.subtitle}</p>
        </div>
      </div>
      {resourceUrl ? (
        <ResourceContent
          key={resourceUrl}
          title={source.title}
          url={resourceUrl}
          sessionId={sessionId}
          handle={source.handle!}
        />
      ) : null}
      <section className="source-excerpt">
        <span>Why it appears here</span>
        <p>
          ygg consulted this source while working in the selected session. It is
          shown here from structured tool evidence, not inferred from the final
          response.
        </p>
      </section>
      {source.excerpt ? (
        <section className="source-excerpt">
          <span>Excerpt</span>
          <pre>{source.excerpt}</pre>
        </section>
      ) : null}
    </div>
  );
}

export function Inspector({
  session,
  selection,
  closing,
  modal,
  previewsAvailable,
  resourceContentUrl,
  onRestoreFocus,
  onClose,
}: InspectorProps) {
  const inspectorRef = useRef<HTMLElement>(null);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    if (!selection) return;
    const inspector = inspectorRef.current;
    const originalTarget =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    const focusable = () =>
      Array.from(
        inspector?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), iframe, [href], [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );
    if (modal || originalTarget?.closest("[inert]")) {
      window.requestAnimationFrame(() => focusable()[0]?.focus());
    }
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (!modal || event.key !== "Tab") return;
      const targets = focusable();
      if (!targets.length) return;
      const first = targets[0];
      const last = targets.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      if (modal) onRestoreFocus();
      if (
        modal &&
        document.activeElement === document.body &&
        originalTarget &&
        !originalTarget.closest("[inert]")
      ) {
        originalTarget.focus();
      }
    };
  }, [modal, onRestoreFocus, selection]);

  if (!selection) return null;

  const output =
    selection.type === "output"
      ? session.outputs.find((candidate) => candidate.id === selection.id)
      : undefined;
  const source =
    selection.type === "source"
      ? session.sources.find((candidate) => candidate.id === selection.id)
      : undefined;
  const preview = previewsAvailable && output?.previewId
    ? session.previews.find((candidate) => candidate.id === output.previewId)
    : undefined;
  const selectedResource =
    selection.type === "resource" ? selection : undefined;
  const title =
    output?.title ?? source?.title ?? selectedResource?.title ?? "Inspector";
  const resourceHandle =
    output?.handle ?? source?.handle ?? selectedResource?.handle;
  const resourceAvailable = (output?.available ?? source?.available) !== false;
  const resourceUrl =
    resourceHandle && resourceAvailable
      ? resourceContentUrl(session.sessionId, resourceHandle)
      : undefined;

  return (
    <aside
      ref={inspectorRef}
      className={`inspector ${closing ? "is-closing" : "is-opening"}`}
      aria-label={`${title} inspector`}
      aria-hidden={closing || undefined}
      aria-modal={modal ? true : undefined}
      role={modal ? "dialog" : undefined}
      inert={closing}
    >
      <header className="inspector-header">
        <div className="inspector-title">
          <span className="inspector-kind">
            {preview ? (
              <Globe2 aria-hidden="true" />
            ) : source ? (
              <FileText aria-hidden="true" />
            ) : (
              <File aria-hidden="true" />
            )}
          </span>
          <div>
            <strong>{title}</strong>
            <span>
              {preview?.status === "live"
                ? "Live preview"
                : source
                  ? "Source"
                  : output?.subtitle ??
                    (selectedResource?.presentation === "diff"
                      ? "Unified diff"
                      : selectedResource?.presentation === "image"
                        ? "Image"
                        : "Text resource")}
            </span>
          </div>
        </div>
        <div className="inspector-actions">
          <button aria-label="Close inspector" onClick={onClose}>
            <X aria-hidden="true" />
          </button>
        </div>
      </header>
      <div className="inspector-body">
        {preview ? (
          <WebPreview preview={preview} />
        ) : output && resourceUrl ? (
          <div className="resource-preview">
            <ResourceContent
              key={resourceUrl}
              title={output.title}
              mediaType={output.mimeType}
              url={resourceUrl}
              sessionId={session.sessionId}
              handle={output.handle!}
              presentation={output.kind === "image" ? "image" : "text"}
            />
          </div>
        ) : output ? (
          <DocumentPreview output={output} />
        ) : source ? (
          <SourceInspector
            source={source}
            resourceUrl={resourceUrl}
            sessionId={session.sessionId}
          />
        ) : selectedResource && resourceUrl ? (
          <div className="resource-preview">
            <ResourceContent
              key={resourceUrl}
              title={selectedResource.title}
              url={resourceUrl}
              sessionId={session.sessionId}
              handle={selectedResource.handle}
              presentation={selectedResource.presentation}
            />
          </div>
        ) : (
          <div className="preview-unavailable">
            <File aria-hidden="true" />
            <strong>This resource is no longer available</strong>
          </div>
        )}
      </div>
    </aside>
  );
}
