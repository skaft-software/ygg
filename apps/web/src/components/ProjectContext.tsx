import {
  AlertTriangle,
  FileText,
  GitBranch,
  LoaderCircle,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";
import { useId } from "react";
import type {
  ContextRefreshState,
  ContextRefreshStatus,
  RepositoryContextSnapshot,
} from "../protocol";

type FolderInstructionFile =
  RepositoryContextSnapshot["instructions"]["files"][number];

export type ProjectContextError =
  | "trustRequired"
  | "rootChanged"
  | "unavailable";

export interface ProjectContextProps {
  snapshot: RepositoryContextSnapshot | null;
  loading: boolean;
  refreshing?: boolean;
  error?: ProjectContextError | null;
  onRefresh: () => void;
}

const refreshStateLabels: Record<ContextRefreshState, string> = {
  current: "Current",
  partial: "Partial",
  notApplicable: "Not applicable",
  unavailable: "Unavailable",
  timedOut: "Timed out",
};

const worktreeLabels: Record<
  RepositoryContextSnapshot["repository"]["worktree"],
  string
> = {
  present: "Git worktree",
  notRepository: "Not a Git repository",
  unknown: "Unknown",
};

const branchStateLabels: Record<
  RepositoryContextSnapshot["repository"]["branchState"],
  string
> = {
  named: "Named",
  detached: "Detached HEAD",
  unborn: "Unborn",
  unknown: "Unknown",
};

function safeProjectErrorMessage(error: ProjectContextError): string {
  switch (error) {
    case "trustRequired":
      return "Project trust is required before repository context can be loaded.";
    case "rootChanged":
      return "The trusted project folder changed or is unavailable. Review project trust before refreshing.";
    case "unavailable":
      return "Project context is temporarily unavailable. Try refreshing.";
  }
}

function safeInstructionErrorMessage(code: string): string {
  switch (code) {
    case "directoryUnavailable":
      return "This folder could not be checked for instructions.";
    case "unsupportedName":
      return "An instruction location has a name that cannot be displayed safely.";
    case "symlinkRejected":
      return "This instruction was ignored because symbolic links are not allowed.";
    case "notRegularFile":
      return "This instruction was ignored because it is not a regular file.";
    case "hardLinkRejected":
      return "This instruction was ignored because multiply-linked files are not allowed.";
    case "fileTooLarge":
      return "This instruction exceeded the per-file safety limit.";
    case "aggregateLimitReached":
      return "This instruction was omitted after the total instruction limit was reached.";
    case "changedDuringRead":
      return "This instruction changed while it was being checked.";
    case "invalidUtf8":
      return "This instruction was ignored because it is not valid UTF-8 text.";
    case "binaryContent":
      return "This instruction was ignored because it contains binary content.";
    case "discoveryLimitReached":
      return "Instruction discovery stopped at a bounded safety limit.";
    default:
      return "An instruction file could not be loaded safely.";
  }
}

function repositorySourceLabel(source: string): string {
  return source === "gitStatusPorcelainV2"
    ? "Git status (porcelain v2)"
    : "Bounded repository status";
}

function instructionSourceLabel(source: string): string {
  return source === "projectAgentsMdV1"
    ? "Project AGENTS.md discovery"
    : "Bounded folder-instruction discovery";
}

function timestamp(timestampMs: number): {
  dateTime?: string;
  label: string;
} {
  const date = new Date(timestampMs);
  if (!Number.isFinite(timestampMs) || Number.isNaN(date.getTime())) {
    return { label: "Unknown" };
  }
  const dateTime = date.toISOString();
  return {
    dateTime,
    label: `${dateTime.slice(0, 10)} ${dateTime.slice(11, 19)} UTC`,
  };
}

function byteCount(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "Unknown";
  if (bytes < 1_024) return `${bytes} bytes`;
  return `${(bytes / 1_024).toFixed(1)} KiB`;
}

function RefreshDetails({
  label,
  source,
  refresh,
}: {
  label: string;
  source: string;
  refresh: ContextRefreshStatus;
}) {
  const refreshedAt = timestamp(refresh.refreshedAtUnixMs);
  return (
    <div className="project-context-refresh">
      <span
        className={`project-context-refresh-state is-${refresh.state}`}
        data-state={refresh.state}
      >
        {refreshStateLabels[refresh.state]}
      </span>
      <dl aria-label={`${label} refresh details`}>
        <div>
          <dt>Source</dt>
          <dd>{source}</dd>
        </div>
        <div>
          <dt>Refreshed</dt>
          <dd>
            <time dateTime={refreshedAt.dateTime}>{refreshedAt.label}</time>
          </dd>
        </div>
        <div>
          <dt>Duration</dt>
          <dd>{refresh.durationMs} ms</dd>
        </div>
      </dl>
    </div>
  );
}

function RepositoryCard({
  repository,
  headingId,
}: {
  repository: RepositoryContextSnapshot["repository"];
  headingId: string;
}) {
  return (
    <article
      className="project-context-card project-context-repository"
      aria-labelledby={headingId}
    >
      <header className="project-context-card-header">
        <span className="project-context-card-icon" aria-hidden="true">
          <GitBranch />
        </span>
        <div>
          <h3 id={headingId}>Git repository</h3>
          <p>
            Repository branches shown here are separate from conversation
            branches.
          </p>
        </div>
        <RefreshDetails
          label="Repository"
          source={repositorySourceLabel(repository.source)}
          refresh={repository.refresh}
        />
      </header>

      <dl className="project-context-facts">
        <div>
          <dt>Worktree</dt>
          <dd>{worktreeLabels[repository.worktree]}</dd>
        </div>
        <div>
          <dt>HEAD</dt>
          <dd>
            {repository.head ? (
              <code>{repository.head}</code>
            ) : repository.branchState === "unborn" ? (
              "No commit yet"
            ) : (
              "Unavailable"
            )}
          </dd>
        </div>
        <div>
          <dt>Git branch</dt>
          <dd>
            {repository.branch ??
              (repository.branchState === "detached"
                ? "Detached"
                : "Unavailable")}
          </dd>
        </div>
        <div>
          <dt>Branch state</dt>
          <dd>{branchStateLabels[repository.branchState]}</dd>
        </div>
        <div>
          <dt>Dirty</dt>
          <dd>
            {repository.dirty === true
              ? "Yes — changes present"
              : repository.dirty === false
                ? "No — clean"
                : "Unknown"}
          </dd>
        </div>
        <div>
          <dt>Ahead of upstream</dt>
          <dd>{repository.ahead ?? "Unavailable"}</dd>
        </div>
        <div>
          <dt>Behind upstream</dt>
          <dd>{repository.behind ?? "Unavailable"}</dd>
        </div>
      </dl>

      {repository.refresh.truncated ? (
        <p className="project-context-truncation" role="status">
          Repository status was limited by a bounded safety check.
        </p>
      ) : null}
    </article>
  );
}

function InstructionFileCard({
  file,
  headingId,
}: {
  file: FolderInstructionFile;
  headingId: string;
}) {
  return (
    <article
      className="project-context-instruction"
      data-truncated={file.contentTruncated}
      aria-labelledby={headingId}
    >
      <header>
        <div>
          <FileText aria-hidden="true" />
          <h4 id={headingId}>
            <code>{file.origin.relativePath}</code>
          </h4>
        </div>
        <span className="project-context-precedence">
          Precedence {file.precedence}
        </span>
      </header>

      <dl className="project-context-instruction-facts">
        <div>
          <dt>Scope</dt>
          <dd>
            <code>{file.origin.scope}</code>
            {file.origin.scope === "." ? " (project root)" : null}
          </dd>
        </div>
        <div>
          <dt>Size</dt>
          <dd>{byteCount(file.byteLen)}</dd>
        </div>
      </dl>

      <div className="project-context-instruction-summary">
        <span>Summary</span>
        <p>{file.summary || "No non-empty summary"}</p>
      </div>

      <details className="project-context-instruction-content">
        <summary>View instruction content</summary>
        <pre>{file.visibleContent || "(Empty file)"}</pre>
        {file.contentTruncated ? (
          <p className="project-context-truncation" role="status">
            Content preview truncated at the server safety limit.
          </p>
        ) : null}
      </details>
    </article>
  );
}

function InstructionsCard({
  instructions,
  headingId,
  fileHeadingPrefix,
}: {
  instructions: RepositoryContextSnapshot["instructions"];
  headingId: string;
  fileHeadingPrefix: string;
}) {
  return (
    <article
      className="project-context-card project-context-instructions"
      aria-labelledby={headingId}
    >
      <header className="project-context-card-header">
        <span className="project-context-card-icon" aria-hidden="true">
          <FileText />
        </span>
        <div>
          <h3 id={headingId}>Folder instructions</h3>
          <p>
            Loaded root-first. A deeper scope takes precedence for files in
            that scope.
          </p>
        </div>
        <RefreshDetails
          label="Folder instructions"
          source={instructionSourceLabel(instructions.source)}
          refresh={instructions.refresh}
        />
      </header>

      <p className="project-context-instruction-total">
        {instructions.files.length}{" "}
        {instructions.files.length === 1 ? "file" : "files"} loaded ·{" "}
        {byteCount(instructions.loadedBytes)}
      </p>

      {instructions.files.length ? (
        <ol className="project-context-instruction-list">
          {instructions.files.map((file, index) => (
            <li key={`${file.precedence}:${file.origin.relativePath}`}>
              <InstructionFileCard
                file={file}
                headingId={`${fileHeadingPrefix}-${index}`}
              />
            </li>
          ))}
        </ol>
      ) : (
        <p className="project-context-empty">
          No AGENTS.md instruction files were loaded.
        </p>
      )}

      {instructions.errors.length ? (
        <section
          className="project-context-instruction-errors"
          aria-labelledby={`${headingId}-errors`}
        >
          <header>
            <AlertTriangle aria-hidden="true" />
            <h4 id={`${headingId}-errors`}>Instructions not loaded</h4>
          </header>
          <ul>
            {instructions.errors.map((error, index) => (
              <li
                key={`${error.origin?.relativePath ?? "unknown"}:${error.code}:${index}`}
              >
                <strong>
                  {error.origin?.relativePath ?? "Project instructions"}
                </strong>
                <span>{safeInstructionErrorMessage(error.code)}</span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {instructions.omittedErrors > 0 ? (
        <p className="project-context-truncation" role="status">
          {instructions.omittedErrors} additional{" "}
          {instructions.omittedErrors === 1 ? "error was" : "errors were"}{" "}
          omitted at the server safety limit.
        </p>
      ) : null}

      {instructions.refresh.truncated ? (
        <p className="project-context-truncation" role="status">
          Some folder-instruction context was omitted by bounded safety limits.
        </p>
      ) : null}
    </article>
  );
}

export function ProjectContext({
  snapshot,
  loading,
  refreshing = false,
  error = null,
  onRefresh,
}: ProjectContextProps) {
  const id = useId();
  const titleId = `${id}-title`;
  const repositoryHeadingId = `${id}-repository`;
  const instructionsHeadingId = `${id}-instructions`;
  const busy = loading || refreshing;

  return (
    <section
      className="project-context-panel"
      aria-labelledby={titleId}
      aria-busy={busy}
      data-trust={snapshot?.trust ?? "unavailable"}
    >
      <header className="project-context-header">
        <div>
          <span className="project-context-trust">
            {snapshot ? (
              <>
                <ShieldCheck aria-hidden="true" />
                Verified project snapshot
              </>
            ) : (
              <>
                <FileText aria-hidden="true" />
                Project diagnostics
              </>
            )}
          </span>
          <h2 id={titleId}>Project context</h2>
          <p>
            Read-only, path-free repository facts and effective AGENTS.md
            instructions.
          </p>
        </div>
        <button
          className="project-context-refresh-button"
          type="button"
          onClick={onRefresh}
          disabled={busy}
        >
          {busy ? (
            <LoaderCircle className="is-spinning" aria-hidden="true" />
          ) : (
            <RefreshCw aria-hidden="true" />
          )}
          {refreshing ? "Refreshing…" : loading ? "Loading…" : "Refresh"}
        </button>
      </header>

      {error ? (
        <div className="project-context-error" role="alert">
          <AlertTriangle aria-hidden="true" />
          <span>{safeProjectErrorMessage(error)}</span>
        </div>
      ) : null}

      {loading && !snapshot ? (
        <div className="project-context-loading" role="status">
          <LoaderCircle className="is-spinning" aria-hidden="true" />
          <span>Loading project context…</span>
        </div>
      ) : snapshot ? (
        <div className="project-context-cards">
          {refreshing ? (
            <span className="sr-only" role="status">
              Refreshing project context…
            </span>
          ) : null}
          <RepositoryCard
            repository={snapshot.repository}
            headingId={repositoryHeadingId}
          />
          <InstructionsCard
            instructions={snapshot.instructions}
            headingId={instructionsHeadingId}
            fileHeadingPrefix={`${id}-instruction`}
          />
        </div>
      ) : !error ? (
        <p className="project-context-empty" role="status">
          Project context has not been loaded yet.
        </p>
      ) : null}
    </section>
  );
}
