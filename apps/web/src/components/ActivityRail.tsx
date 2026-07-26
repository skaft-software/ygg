import {
  Check,
  ChevronDown,
  Circle,
  File,
  FileText,
  Globe2,
  LoaderCircle,
  PanelRightClose,
  Search,
} from "lucide-react";
import type {
  OutputRef,
  ProgressStep,
  SessionSnapshot,
  SourceRef,
} from "../protocol";

interface ActivityRailProps {
  session: SessionSnapshot;
  open: boolean;
  onClose: () => void;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
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
  if (step.status === "in_progress") {
    return <LoaderCircle className="spin" aria-hidden="true" />;
  }
  return <Circle aria-hidden="true" />;
}

export function ActivityRail({
  session,
  open,
  onClose,
  onOpenOutput,
  onOpenSource,
}: ActivityRailProps) {
  const completeCount = session.progress.filter(
    (step) => step.status === "completed",
  ).length;

  return (
    <aside
      className={`activity-rail ${open ? "is-open" : ""}`}
      aria-label="Session activity"
      aria-hidden={!open}
      inert={!open}
    >
      <header className="rail-header">
        <div>
          <span>Session activity</span>
          <strong>
            {session.status === "working"
              ? "Working"
              : session.status === "needs_attention"
                ? "Needs attention"
                : "Up to date"}
          </strong>
        </div>
        <button className="icon-button" onClick={onClose}>
          <PanelRightClose aria-hidden="true" />
          <span className="sr-only">Close activity</span>
        </button>
      </header>

      <div className="rail-scroll">
        {session.progress.length ? (
          <section className="rail-section" aria-labelledby="progress-heading">
            <details open>
              <summary>
                <span id="progress-heading">Progress</span>
                <em>
                  {completeCount}/{session.progress.length}
                </em>
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

        {session.outputs.length ? (
          <section className="rail-section" aria-labelledby="outputs-heading">
            <div className="rail-section-title">
              <span id="outputs-heading">Outputs</span>
              <em>{session.outputs.length}</em>
            </div>
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
          </section>
        ) : null}

        {session.sources.length ? (
          <section className="rail-section" aria-labelledby="sources-heading">
            <div className="rail-section-title">
              <span id="sources-heading">Sources</span>
              <em>{session.sources.length}</em>
            </div>
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
            <button className="view-all-button">
              <Search aria-hidden="true" />
              Browse consulted sources
            </button>
          </section>
        ) : null}
      </div>
    </aside>
  );
}
