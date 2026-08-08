import {
  AlertTriangle,
  Check,
  ChevronRight,
  Folder,
  GitPullRequest,
  LoaderCircle,
  Plus,
  Search,
} from "lucide-react";
import { useMemo, useState } from "react";
import type { ProjectSummary, SessionSummary } from "../protocol";
import {
  formatTaskAge,
  taskNeedsAttention,
  taskNeedsReview,
} from "../fleet-status";
import { displaySessionTitle } from "../session-title";

type FleetView = "all" | "attention" | "working" | "review" | "complete";
type SignalTone = "attention" | "danger" | "working" | "review" | "complete" | "quiet";

interface FleetOverviewProps {
  sessions: SessionSummary[];
  projects: ProjectSummary[];
  selectedSessionId: string | null;
  onNewTask: () => void;
  onSelectTask: (sessionId: string) => void;
}

interface FleetMetric {
  view: Exclude<FleetView, "all">;
  label: string;
  description: string;
  count: number;
  icon: typeof AlertTriangle;
}

interface TaskSignal {
  label: string;
  tone: SignalTone;
}

function taskIsComplete(session: SessionSummary): boolean {
  return session.status === "done" || session.pullRequest?.state === "merged";
}

function taskSignal(session: SessionSummary): TaskSignal {
  if (session.status === "failed") {
    return { label: "Failed", tone: "danger" };
  }
  if (session.status === "disconnected") {
    return { label: "Reconnecting", tone: "attention" };
  }
  if (session.status === "needs_attention" || session.attentionCount > 0) {
    return { label: "Needs you", tone: "attention" };
  }
  if (session.pullRequest?.state === "ready") {
    return { label: "Review ready", tone: "review" };
  }
  if (session.status === "working") {
    return { label: "Working", tone: "working" };
  }
  if (session.pullRequest?.state === "merged") {
    return { label: "Merged", tone: "complete" };
  }
  if (session.status === "done" && session.unread) {
    return { label: "New result", tone: "review" };
  }
  if (session.status === "done") {
    return { label: "Complete", tone: "complete" };
  }
  if (session.status === "stopped") {
    return { label: "Stopped", tone: "quiet" };
  }
  return { label: "Ready", tone: "quiet" };
}

function taskPriority(session: SessionSummary): number {
  if (taskNeedsAttention(session)) return 0;
  if (taskNeedsReview(session)) return 1;
  if (session.status === "working") return 2;
  if (!taskIsComplete(session)) return 3;
  return 4;
}

function matchesView(session: SessionSummary, view: FleetView): boolean {
  if (view === "attention") return taskNeedsAttention(session);
  if (view === "working") return session.status === "working";
  if (view === "review") return taskNeedsReview(session);
  if (view === "complete") return taskIsComplete(session);
  return true;
}

function viewHeading(view: FleetView): string {
  if (view === "attention") return "Needs you";
  if (view === "working") return "In progress";
  if (view === "review") return "Ready to review";
  if (view === "complete") return "Completed work";
  return "Priority queue";
}

function plural(count: number, singular: string, multiple = `${singular}s`) {
  return `${count} ${count === 1 ? singular : multiple}`;
}

export function FleetOverview({
  sessions,
  projects,
  selectedSessionId,
  onNewTask,
  onSelectTask,
}: FleetOverviewProps) {
  const [view, setView] = useState<FleetView>("all");
  const [query, setQuery] = useState("");
  const activeSessions = useMemo(
    () => sessions.filter((session) => session.lifecycle === "active"),
    [sessions],
  );
  const projectsById = useMemo(
    () => new Map(projects.map((project) => [project.id, project])),
    [projects],
  );
  const metrics = useMemo<FleetMetric[]>(
    () => [
      {
        view: "attention",
        label: "Needs you",
        description: "Waiting on you",
        count: activeSessions.filter(taskNeedsAttention).length,
        icon: AlertTriangle,
      },
      {
        view: "working",
        label: "Working",
        description: "Running now",
        count: activeSessions.filter((session) => session.status === "working")
          .length,
        icon: LoaderCircle,
      },
      {
        view: "review",
        label: "Review",
        description: "Ready for review",
        count: activeSessions.filter(taskNeedsReview).length,
        icon: GitPullRequest,
      },
      {
        view: "complete",
        label: "Complete",
        description: "Finished tasks",
        count: activeSessions.filter(taskIsComplete).length,
        icon: Check,
      },
    ],
    [activeSessions],
  );
  const pullRequestMetrics = useMemo(() => {
    const pullRequests = activeSessions.flatMap((session) =>
      session.pullRequest ? [session.pullRequest] : [],
    );
    return {
      created: pullRequests.length,
      inProgress: pullRequests.filter(
        (pullRequest) => pullRequest.state === "in_progress",
      ).length,
      ready: pullRequests.filter((pullRequest) => pullRequest.state === "ready")
        .length,
      merged: pullRequests.filter((pullRequest) => pullRequest.state === "merged")
        .length,
    };
  }, [activeSessions]);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const visibleSessions = useMemo(
    () =>
      activeSessions
        .filter((session) => matchesView(session, view))
        .filter((session) => {
          if (!normalizedQuery) return true;
          const projectName = projectsById.get(session.projectId)?.name ?? "";
          return [
            displaySessionTitle(session.title),
            session.preview,
            projectName,
          ].some((value) => value.toLocaleLowerCase().includes(normalizedQuery));
        })
        .sort(
          (left, right) =>
            taskPriority(left) - taskPriority(right) ||
            Number(right.pinned) - Number(left.pinned) ||
            Date.parse(right.updatedAt) - Date.parse(left.updatedAt),
        ),
    [activeSessions, normalizedQuery, projectsById, view],
  );
  const activeProjectCount = new Set(
    activeSessions.map((session) => session.projectId),
  ).size;
  const nextTask = visibleSessions[0];

  return (
    <main className="fleet-overview" aria-labelledby="fleet-title">
      <div className="fleet-overview-scroll">
        <section className="fleet-hero">
          <div className="fleet-hero-copy">
            <span className="fleet-eyebrow">Agent command center</span>
            <h1 id="fleet-title">All agents. One clear queue.</h1>
            <p>
              Exceptions rise to the top while healthy work stays visible and
              out of your way.
            </p>
          </div>
          <div className="fleet-hero-actions">
            <span className="fleet-live-summary">
              <span aria-hidden="true" />
              {plural(activeSessions.length, "active task")}
              {activeProjectCount > 0
                ? ` across ${plural(activeProjectCount, "project")}`
                : ""}
            </span>
            <button className="primary-button fleet-new-task" onClick={onNewTask}>
              <Plus aria-hidden="true" />
              New task
            </button>
          </div>
        </section>

        <section className="fleet-metrics" aria-label="Task status overview">
          {metrics.map((metric) => {
            const Icon = metric.icon;
            return (
              <button
                key={metric.view}
                className={`fleet-metric is-${metric.view} ${view === metric.view ? "is-selected" : ""}`}
                type="button"
                aria-pressed={view === metric.view}
                aria-label={`Show ${metric.label.toLocaleLowerCase()} tasks, ${metric.count}`}
                onClick={() =>
                  setView((current) =>
                    current === metric.view ? "all" : metric.view,
                  )
                }
              >
                <span className="fleet-metric-icon" aria-hidden="true">
                  <Icon className={metric.view === "working" ? "spin" : ""} />
                </span>
                <strong>{metric.count}</strong>
                <span>{metric.label}</span>
                <small>{metric.description}</small>
              </button>
            );
          })}
        </section>

        <section
          className="fleet-pull-request-summary"
          aria-label="Pull request status overview"
        >
          <div className="fleet-pull-request-heading">
            <GitPullRequest aria-hidden="true" />
            <strong>Pull requests</strong>
          </div>
          <dl>
            <div className="is-created">
              <dt>Created</dt>
              <dd>{pullRequestMetrics.created}</dd>
              <small>{pullRequestMetrics.inProgress} in progress</small>
            </div>
            <div className="is-ready">
              <dt>Review ready</dt>
              <dd>{pullRequestMetrics.ready}</dd>
              <small>Open for review</small>
            </div>
            <div className="is-merged">
              <dt>Merged</dt>
              <dd>{pullRequestMetrics.merged}</dd>
              <small>Completed PRs</small>
            </div>
          </dl>
        </section>

        <section className="fleet-queue" aria-labelledby="fleet-queue-title">
          <header className="fleet-queue-header">
            <div>
              <span className="fleet-eyebrow">Work queue</span>
              <h2 id="fleet-queue-title">{viewHeading(view)}</h2>
            </div>
            <div className="fleet-queue-actions">
              <label className="fleet-search">
                <Search aria-hidden="true" />
                <span className="sr-only">Search active tasks</span>
                <input
                  type="search"
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  placeholder="Search tasks or projects"
                />
              </label>
              {view !== "all" || normalizedQuery ? (
                <button
                  className="fleet-clear-filter"
                  type="button"
                  onClick={() => {
                    setView("all");
                    setQuery("");
                  }}
                >
                  Show all
                </button>
              ) : null}
              <button
                className="secondary-button fleet-open-next"
                type="button"
                disabled={!nextTask}
                onClick={() => nextTask && onSelectTask(nextTask.id)}
              >
                Open next
                <ChevronRight aria-hidden="true" />
              </button>
            </div>
          </header>

          <div className="fleet-list-heading" aria-hidden="true">
            <span>Task</span>
            <span>Project</span>
            <span>State</span>
            <span>Updated</span>
            <span />
          </div>
          {visibleSessions.length ? (
            <ul className="fleet-task-list" aria-label={viewHeading(view)}>
              {visibleSessions.map((session) => {
                const projectName =
                  projectsById.get(session.projectId)?.name ??
                  "Unassigned project";
                const signal = taskSignal(session);
                const displayTitle = displaySessionTitle(session.title);
                return (
                  <li key={session.id}>
                    <button
                      className={`fleet-task-row ${session.id === selectedSessionId ? "is-current" : ""}`}
                      type="button"
                      aria-current={
                        session.id === selectedSessionId ? "true" : undefined
                      }
                      aria-label={`Open task ${displayTitle}, ${signal.label}, ${projectName}`}
                      onClick={() => onSelectTask(session.id)}
                    >
                      <span className="fleet-task-primary">
                        <span
                          className={`fleet-task-signal is-${signal.tone}`}
                          aria-hidden="true"
                        />
                        <span className="fleet-task-copy">
                          <strong>{displayTitle}</strong>
                          <small>{session.preview}</small>
                        </span>
                      </span>
                      <span className="fleet-task-project">
                        <Folder aria-hidden="true" />
                        {projectName}
                      </span>
                      <span className={`fleet-task-state is-${signal.tone}`}>
                        {signal.tone === "working" ? (
                          <LoaderCircle className="spin" aria-hidden="true" />
                        ) : null}
                        {signal.label}
                      </span>
                      <time
                        dateTime={session.updatedAt}
                        title={new Date(session.updatedAt).toLocaleString()}
                      >
                        {formatTaskAge(session.updatedAt)}
                      </time>
                      <ChevronRight aria-hidden="true" />
                    </button>
                  </li>
                );
              })}
            </ul>
          ) : (
            <div className="fleet-empty">
              <Search aria-hidden="true" />
              <strong>No tasks match this view</strong>
              <p>Clear the filters or start a new task.</p>
              <div>
                <button
                  className="secondary-button"
                  type="button"
                  onClick={() => {
                    setView("all");
                    setQuery("");
                  }}
                >
                  Clear filters
                </button>
                <button className="primary-button" type="button" onClick={onNewTask}>
                  <Plus aria-hidden="true" />
                  New task
                </button>
              </div>
            </div>
          )}
        </section>
      </div>
    </main>
  );
}
