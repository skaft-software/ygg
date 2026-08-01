import {
  Archive,
  Check,
  Folder,
  Pencil,
  ShieldAlert,
  ShieldCheck,
  Star,
  X,
} from "lucide-react";
import { useState } from "react";
import type {
  ProjectCatalog,
  ProjectSummary,
  RepositoryContextSnapshot,
} from "../protocol";
import {
  ProjectContext,
  type ProjectContextError,
} from "./ProjectContext";

interface ProjectsViewProps {
  catalog: ProjectCatalog;
  onboarding?: boolean;
  onRename: (projectId: string, name: string) => Promise<void>;
  onSetDefault: (projectId: string | null) => Promise<void>;
  onSetTrust: (projectId: string, trusted: boolean) => Promise<void>;
  onArchive: (projectId: string) => Promise<void>;
  onLoadContext?: (projectId: string) => Promise<RepositoryContextSnapshot>;
}

function projectState(project: ProjectSummary): string {
  if (project.archived) return "Archived";
  if (!project.available) return "Folder unavailable";
  if (!project.trusted) return "Trust required";
  return project.liveSessionCount > 0
    ? `${project.liveSessionCount} active`
    : "Ready";
}

export function ProjectsView({
  catalog,
  onboarding = false,
  onRename,
  onSetDefault,
  onSetTrust,
  onArchive,
  onLoadContext,
}: ProjectsViewProps) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draftName, setDraftName] = useState("");
  const [confirmArchive, setConfirmArchive] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [contextProjectId, setContextProjectId] = useState<string | null>(null);
  const [contextSnapshot, setContextSnapshot] =
    useState<RepositoryContextSnapshot | null>(null);
  const [contextLoading, setContextLoading] = useState(false);
  const [contextError, setContextError] =
    useState<ProjectContextError | null>(null);
  const runnable = catalog.projects.filter(
    (project) =>
      project.trusted && project.available && !project.archived,
  );
  const act = async (key: string, action: () => Promise<void>) => {
    setPending(key);
    setError(null);
    try {
      await action();
    } catch (reason) {
      setError(
        reason instanceof Error
          ? reason.message
          : "The project change could not be completed.",
      );
    } finally {
      setPending(null);
    }
  };
  const loadContext = async (projectId: string) => {
    if (!onLoadContext) return;
    setContextProjectId(projectId);
    setContextLoading(true);
    setContextError(null);
    try {
      setContextSnapshot(await onLoadContext(projectId));
    } catch (reason) {
      const message =
        reason instanceof Error ? reason.message.toLocaleLowerCase() : "";
      setContextSnapshot(null);
      setContextError(
        message.includes("403") || message.includes("trust")
          ? "trustRequired"
          : message.includes("changed")
            ? "rootChanged"
            : "unavailable",
      );
    } finally {
      setContextLoading(false);
    }
  };

  return (
    <main
      className={`projects-view ${onboarding ? "is-onboarding" : ""}`}
      aria-labelledby="projects-title"
    >
      <header className="projects-hero">
        <span className="projects-hero-icon" aria-hidden="true">
          <Folder />
        </span>
        <div>
          <span>{onboarding ? "Before ygg can start" : "Local workspaces"}</span>
          <h1 id="projects-title">
            {onboarding ? "Choose what this host may trust" : "Projects"}
          </h1>
          <p>
            Project trust allows ygg to load that folder&apos;s instructions,
            skills, and extensions and to start coding-agent tasks there. Agent
            authority is configured separately for each task.
          </p>
        </div>
      </header>

      {onboarding && catalog.projects.length > 0 && runnable.length === 0 ? (
        <section className="project-onboarding-note">
          <ShieldAlert aria-hidden="true" />
          <div>
            <strong>No trusted project is available</strong>
            <p>
              Review the folder label below, then grant trust explicitly. Ygg
              will not load project-controlled configuration before that.
            </p>
          </div>
        </section>
      ) : null}

      <section className="project-grid" aria-label="Registered projects">
        {catalog.projects.length === 0 ? (
          <div className="projects-empty">
            <span className="projects-empty-icon" aria-hidden="true">
              <Folder />
            </span>
            <div>
              <h2>No projects found. Open a folder to get started.</h2>
              <p>Start ygg from the folder you want to add:</p>
              <code>ygg --workspace /path/to/project serve</code>
            </div>
          </div>
        ) : null}
        {catalog.projects.map((project) => {
          const projectPending = pending?.endsWith(`:${project.id}`) ?? false;
          const canRun =
            project.available && !project.archived && project.trusted;
          const trustAction = !project.available
            ? "Restore folder and trust"
            : project.trusted
              ? "Revoke trust"
              : "Trust project";
          const nextTrust = !project.available || !project.trusted;
          return (
            <article
              className="project-card"
              data-trusted={project.trusted}
              data-available={project.available}
              data-archived={project.archived}
              key={project.id}
            >
              <header>
                <span className="project-folder" aria-hidden="true">
                  <Folder />
                </span>
                <div>
                  {editing === project.id ? (
                    <form
                      className="project-rename"
                      onSubmit={(event) => {
                        event.preventDefault();
                        const next = draftName.trim();
                        if (!next) return;
                        void act(`rename:${project.id}`, async () => {
                          await onRename(project.id, next);
                          setEditing(null);
                        });
                      }}
                    >
                      <input
                        autoFocus
                        aria-label={`Rename ${project.name}`}
                        value={draftName}
                        onChange={(event) => setDraftName(event.target.value)}
                      />
                      <button type="submit" aria-label="Save project name">
                        <Check aria-hidden="true" />
                      </button>
                      <button
                        type="button"
                        aria-label="Cancel rename"
                        onClick={() => setEditing(null)}
                      >
                        <X aria-hidden="true" />
                      </button>
                    </form>
                  ) : (
                    <h2>{project.name}</h2>
                  )}
                  <span>{projectState(project)}</span>
                </div>
                {project.isDefault ? (
                  <span className="project-default">
                    <Star aria-hidden="true" />
                    Default
                  </span>
                ) : null}
              </header>

              <div className="project-facts">
                <span>
                  <strong>{project.sessionCount}</strong>
                  {project.sessionCount === 1 ? "task" : "tasks"}
                </span>
                <span>
                  {project.trusted ? (
                    <ShieldCheck aria-hidden="true" />
                  ) : (
                    <ShieldAlert aria-hidden="true" />
                  )}
                  {project.trusted ? "Trusted" : "Untrusted"}
                </span>
              </div>

              {!project.available ? (
                <p className="project-warning">
                  The registered folder identity changed or is unavailable. If
                  this is the folder that launched the host, restore it and
                  grant trust below.
                </p>
              ) : null}

              {catalog.lifecycleMutationsSupported && !project.archived ? (
                <div className="project-actions">
                  <button
                    type="button"
                    disabled={projectPending}
                    onClick={() =>
                      void act(`trust:${project.id}`, () =>
                        onSetTrust(project.id, nextTrust),
                      )
                    }
                  >
                    {nextTrust ? (
                      <ShieldCheck aria-hidden="true" />
                    ) : (
                      <ShieldAlert aria-hidden="true" />
                    )}
                    {trustAction}
                  </button>
                  <button
                    type="button"
                    disabled={!canRun || projectPending}
                    onClick={() =>
                      void act(`default:${project.id}`, () =>
                        onSetDefault(project.isDefault ? null : project.id),
                      )
                    }
                  >
                    <Star aria-hidden="true" />
                    {project.isDefault ? "Clear default" : "Make default"}
                  </button>
                  <button
                    type="button"
                    disabled={projectPending}
                    onClick={() => {
                      setDraftName(project.name);
                      setEditing(project.id);
                    }}
                  >
                    <Pencil aria-hidden="true" />
                    Rename
                  </button>
                  {confirmArchive === project.id ? (
                    <>
                      <button
                        type="button"
                        className="is-danger"
                        disabled={projectPending}
                        onClick={() =>
                          void act(`archive:${project.id}`, () =>
                            onArchive(project.id),
                          )
                        }
                      >
                        <Archive aria-hidden="true" />
                        Archive and revoke
                      </button>
                      <button
                        type="button"
                        onClick={() => setConfirmArchive(null)}
                      >
                        Cancel
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      disabled={projectPending}
                      onClick={() => setConfirmArchive(project.id)}
                    >
                      <Archive aria-hidden="true" />
                      Archive
                    </button>
                  )}
                </div>
              ) : null}
              {onLoadContext && canRun ? (
                <div className="project-context-action">
                  <button
                    type="button"
                    aria-expanded={contextProjectId === project.id}
                    onClick={() => {
                      if (contextProjectId === project.id) {
                        setContextProjectId(null);
                        setContextSnapshot(null);
                        setContextError(null);
                      } else {
                        void loadContext(project.id);
                      }
                    }}
                  >
                    {contextProjectId === project.id
                      ? "Hide repository context"
                      : "Inspect repository context"}
                  </button>
                </div>
              ) : null}
              {contextProjectId === project.id ? (
                <ProjectContext
                  snapshot={contextSnapshot}
                  loading={contextLoading && !contextSnapshot}
                  refreshing={contextLoading && Boolean(contextSnapshot)}
                  error={contextError}
                  onRefresh={() => void loadContext(project.id)}
                />
              ) : null}
            </article>
          );
        })}
      </section>

      {catalog.projects.length > 0 && !catalog.importSupported ? (
        <p className="project-import-note">
          Additional folders can be registered only by a host-native picker.
          This browser build never asks you to type or transmit an absolute
          host path.
        </p>
      ) : null}
      {error ? (
        <p className="project-error" role="alert">
          {error}
        </p>
      ) : null}
    </main>
  );
}
