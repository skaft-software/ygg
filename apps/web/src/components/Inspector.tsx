import {
  Check,
  File,
  FileText,
  Globe2,
  X,
} from "lucide-react";
import { useEffect, useRef } from "react";
import { getFixturePreviewMarkup } from "../fixtures";
import type { OutputRef, PreviewRef, SessionSnapshot, SourceRef } from "../protocol";

export type InspectorSelection =
  | { type: "output"; id: string }
  | { type: "source"; id: string };

interface InspectorProps {
  session: SessionSnapshot;
  selection: InspectorSelection | null;
  previewsAvailable: boolean;
  onRestoreFocus: () => void;
  onClose: () => void;
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
  const fixtureMarkup = preview.fixtureId
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
        <span className="document-kicker">Ygg output</span>
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

function SourceInspector({ source }: { source: SourceRef }) {
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
      <section className="source-excerpt">
        <span>Why it appears here</span>
        <p>
          Ygg consulted this source while working in the selected session. It is
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
  previewsAvailable,
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
          'button:not([disabled]), [href], [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );
    window.requestAnimationFrame(() => focusable().at(-1)?.focus());
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (event.key !== "Tab") return;
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
      onRestoreFocus();
      if (
        document.activeElement === document.body &&
        originalTarget &&
        !originalTarget.closest("[inert]")
      ) {
        originalTarget.focus();
      }
    };
  }, [onRestoreFocus, selection]);

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
  const title = output?.title ?? source?.title ?? "Inspector";

  return (
    <aside
      ref={inspectorRef}
      className="inspector"
      aria-label={`${title} inspector`}
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
                  : output?.subtitle}
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
        ) : output ? (
          <DocumentPreview output={output} />
        ) : source ? (
          <SourceInspector source={source} />
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
