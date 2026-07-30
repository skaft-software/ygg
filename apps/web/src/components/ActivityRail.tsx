import {
  Check,
  ChevronDown,
  Circle,
  File,
  FileText,
  Globe2,
  PanelRightClose,
  TerminalSquare,
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import type {
  ActionItem,
  OutputRef,
  ProgressStep,
  SessionSnapshot,
  SourceRef,
} from "../protocol";
import { CompletionReview } from "./CompletionReview";

interface ActivityRailProps {
  session: SessionSnapshot;
  open: boolean;
  onClose: () => void;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  onOpenResource?: (
    handle: string,
    title: string,
    presentation: "text" | "diff" | "image",
  ) => void;
  modal: boolean;
  onRestoreFocus: () => void;
  resourcesAvailable: boolean;
}

const outputIcon = (output: OutputRef) => {
  if (output.kind === "site") return <Globe2 aria-hidden="true" />;
  if (output.kind === "document") return <FileText aria-hidden="true" />;
  return <File aria-hidden="true" />;
};

const sourceIcon = (source: SourceRef) => {
  if (source.kind === "web") return <Globe2 aria-hidden="true" />;
  if (source.kind === "documentation") return <FileText aria-hidden="true" />;
  return <File aria-hidden="true" />;
};

function StepGlyph({ step }: { step: ProgressStep }) {
  if (step.status === "completed") return <Check aria-hidden="true" />;
  return <Circle aria-hidden="true" />;
}

function ActivityRailView({
  session,
  open,
  onClose,
  onOpenOutput,
  onOpenSource,
  onOpenResource,
  modal,
  onRestoreFocus,
  resourcesAvailable,
}: ActivityRailProps) {
  const railRef = useRef<HTMLElement>(null);
  const [openSections, setOpenSections] = useState({
    review: true,
    commands: false,
    progress: true,
    artifacts: true,
    context: true,
  });
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);
  useEffect(() => {
    if (!open || !modal) return;
    const rail = railRef.current;
    const focusable = () =>
      Array.from(
        rail?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), summary, [href], [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );
    window.requestAnimationFrame(() => focusable()[0]?.focus());
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
    };
  }, [modal, onRestoreFocus, open]);
  const completeCount = session.progress.filter(
    (step) => step.status === "completed",
  ).length;
  const latestOutcome = [...session.items]
    .reverse()
    .find((item) => item.kind === "run_outcome");
  const actions = session.items.filter((item): item is ActionItem => {
    if (item.kind !== "action" || !latestOutcome) return false;
    return latestOutcome.runId
      ? item.runId === latestOutcome.runId
      : !item.runId && item.turnId === latestOutcome.turnId;
  });
  const outputs = new Map(session.outputs.map((output) => [output.id, output]));
  const commands = session.items.filter(
    (item): item is ActionItem =>
      item.kind === "action" && item.actionKind === "command",
  );
  const hasActivity = Boolean(
    latestOutcome ||
      commands.length ||
      session.progress.length ||
      (resourcesAvailable && (session.outputs.length || session.sources.length)),
  );

  return (
    <aside
      ref={railRef}
      className={`activity-rail ${open ? "is-open" : ""}`}
      aria-label="Session activity"
      aria-hidden={!open}
      aria-modal={modal && open ? true : undefined}
      role={modal ? "dialog" : undefined}
      inert={!open}
    >
      <button className="rail-close icon-button" onClick={onClose}>
        <PanelRightClose aria-hidden="true" />
        <span className="sr-only">Close activity</span>
      </button>

      <div className="rail-scroll">
        {!hasActivity ? (
          <div className="activity-empty" role="status">
            <TerminalSquare aria-hidden="true" />
            <p>No activity yet. The agent&apos;s work will appear here.</p>
          </div>
        ) : null}
        {latestOutcome ? (
          <section className="rail-section" aria-labelledby="review-heading">
            <details
              open={openSections.review}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  review: open,
                }));
              }}
            >
              <summary>
                <span id="review-heading">Review</span>
                <em>{latestOutcome.review.evidenceCoverage}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <CompletionReview
                outcome={latestOutcome}
                actions={actions}
                outputs={outputs}
                onOpenOutput={onOpenOutput}
                onOpenResource={onOpenResource}
                compact
              />
            </details>
          </section>
        ) : null}

        {commands.length ? (
          <section className="rail-section" aria-labelledby="commands-heading">
            <details
              open={openSections.commands}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  commands: open,
                }));
              }}
            >
              <summary>
                <span id="commands-heading">Command history</span>
                <em>{commands.length}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <ol className="command-history-list">
                {[...commands].reverse().map((command) => (
                  <li key={command.id} data-status={command.status}>
                    <TerminalSquare aria-hidden="true" />
                    <span>
                      <strong>
                        {command.commandPreview ?? command.label}
                      </strong>
                      <small>
                        {[
                          command.cwd,
                          command.durationMs === undefined
                            ? undefined
                            : `${command.durationMs}ms`,
                          typeof command.exitCode === "number"
                            ? `exit ${command.exitCode}`
                            : command.status,
                        ]
                          .filter(Boolean)
                          .join(" · ")}
                      </small>
                      {command.outputSummary ? (
                        <small>{command.outputSummary}</small>
                      ) : null}
                    </span>
                    {command.outputHandle && onOpenResource ? (
                      <button
                        type="button"
                        onClick={() =>
                          onOpenResource(
                            command.outputHandle!,
                            `${command.label} output`,
                            "text",
                          )
                        }
                      >
                        Output
                      </button>
                    ) : null}
                  </li>
                ))}
              </ol>
            </details>
          </section>
        ) : null}

        {session.progress.length ? (
          <section className="rail-section" aria-labelledby="progress-heading">
            <details
              open={openSections.progress}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  progress: open,
                }));
              }}
            >
              <summary>
                <span id="progress-heading">Progress</span>
                <em>{completeCount} of {session.progress.length}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <ol className="progress-list">
                {session.progress.map((step) => (
                  <li key={step.id} data-status={step.status}>
                    <span className="progress-glyph">
                      <StepGlyph step={step} />
                    </span>
                    <span>
                      {step.status === "in_progress"
                        ? step.activeForm
                        : step.content}
                    </span>
                  </li>
                ))}
              </ol>
            </details>
          </section>
        ) : null}

        {resourcesAvailable && session.outputs.length ? (
          <section className="rail-section" aria-labelledby="outputs-heading">
            <details
              open={openSections.artifacts}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  artifacts: open,
                }));
              }}
            >
              <summary>
                <span id="outputs-heading">Artifacts</span>
                <em>{session.outputs.length}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <div className="resource-list">
                {session.outputs.map((output) => (
                  <button
                    key={output.id}
                    onClick={() => onOpenOutput(output.id)}
                  >
                    <span className="resource-icon">{outputIcon(output)}</span>
                    <span>
                      <strong>{output.title}</strong>
                      <small>{output.subtitle}</small>
                    </span>
                  </button>
                ))}
              </div>
            </details>
          </section>
        ) : null}

        {resourcesAvailable && session.sources.length ? (
          <section className="rail-section" aria-labelledby="sources-heading">
            <details
              open={openSections.context}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  context: open,
                }));
              }}
            >
              <summary>
                <span id="sources-heading">Context</span>
                <em>{session.sources.length}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <div className="context-label">Sources consulted</div>
              <div className="resource-list">
                {session.sources.map((source) => (
                  <button
                    key={source.id}
                    onClick={() => onOpenSource(source.id)}
                  >
                    <span className="resource-icon">{sourceIcon(source)}</span>
                    <span>
                      <strong>{source.title}</strong>
                      <small>{source.subtitle}</small>
                    </span>
                  </button>
                ))}
              </div>
            </details>
          </section>
        ) : null}
      </div>
    </aside>
  );
}

export const ActivityRail = memo(
  ActivityRailView,
  (previous, next) =>
    previous.session.sessionId === next.session.sessionId &&
    previous.session.progress === next.session.progress &&
    previous.session.items === next.session.items &&
    previous.session.outputs === next.session.outputs &&
    previous.session.sources === next.session.sources &&
    previous.open === next.open &&
    previous.onClose === next.onClose &&
    previous.onOpenOutput === next.onOpenOutput &&
    previous.onOpenSource === next.onOpenSource &&
    previous.onOpenResource === next.onOpenResource &&
    previous.modal === next.modal &&
    previous.onRestoreFocus === next.onRestoreFocus &&
    previous.resourcesAvailable === next.resourcesAvailable,
);
