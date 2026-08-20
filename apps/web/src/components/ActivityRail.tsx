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
  ExtensionPresentationAction,
  ExtensionPresentationNode,
  ExtensionPresentationReference,
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
  onOpenSession?: (sessionId: string) => void;
  onInvokeExtensionAction?: (
    extension: string,
    extensionInstanceId: string,
    generation: number,
    revision: number,
    action: string,
    confirmed: boolean,
  ) => Promise<void>;
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

function extensionNodeDepth(
  nodes: readonly ExtensionPresentationNode[],
  node: ExtensionPresentationNode,
): number {
  const byId = new Map(nodes.map((candidate) => [candidate.id, candidate]));
  const seen = new Set<string>();
  let parentId = node.parentId;
  let depth = 0;
  while (parentId && !seen.has(parentId) && depth < 16) {
    seen.add(parentId);
    depth += 1;
    parentId = byId.get(parentId)?.parentId;
  }
  return depth;
}

function ExtensionReferences({
  references,
  onOpenSession,
  onOpenArtifact,
  onOpenResource,
  canOpenArtifact,
  canOpenResource,
  resourcesAvailable,
}: {
  references: readonly ExtensionPresentationReference[];
  onOpenSession?: (sessionId: string) => void;
  onOpenArtifact?: (artifactId: string, label: string) => void;
  onOpenResource?: (handle: string, label: string) => void;
  canOpenArtifact?: (artifactId: string) => boolean;
  canOpenResource?: (handle: string) => boolean;
  resourcesAvailable: boolean;
}) {
  if (!references.length) return null;
  return (
    <div className="extension-links">
      {references.map((reference) => {
        const label =
          reference.label ??
          (reference.kind === "url"
            ? "Open source"
            : reference.kind === "session"
              ? "Open session"
              : reference.kind === "artifact"
                ? "Artifact"
                : "Resource");
        if (reference.kind === "url") {
          return (
            <a
              key={`${reference.kind}:${reference.id}`}
              href={reference.id}
              target="_blank"
              rel="noreferrer noopener"
            >
              {label}
            </a>
          );
        }
        if (reference.kind === "session" && onOpenSession) {
          return (
            <button
              key={`${reference.kind}:${reference.id}`}
              type="button"
              onClick={() => onOpenSession(reference.id)}
            >
              {label}
            </button>
          );
        }
        if (
          reference.kind === "artifact" &&
          onOpenArtifact &&
          canOpenArtifact?.(reference.id)
        ) {
          return (
            <button
              key={`${reference.kind}:${reference.id}`}
              type="button"
              onClick={() => onOpenArtifact(reference.id, label)}
            >
              {label}
            </button>
          );
        }
        if (
          reference.kind === "resource" &&
          onOpenResource &&
          resourcesAvailable &&
          canOpenResource?.(reference.id)
        ) {
          return (
            <button
              key={`${reference.kind}:${reference.id}`}
              type="button"
              onClick={() => onOpenResource(reference.id, label)}
            >
              {label}
            </button>
          );
        }
        return (
          <span key={`${reference.kind}:${reference.id}`} title={reference.id}>
            {label}: <code>{reference.id}</code>
          </span>
        );
      })}
    </div>
  );
}

function ExtensionActionControl({
  extension,
  extensionInstanceId,
  generation,
  revision,
  action,
  pendingAction,
  confirmingAction,
  setPendingAction,
  setConfirmingAction,
  setActionError,
  onInvoke,
}: {
  extension: string;
  extensionInstanceId: string;
  generation: number;
  revision: number;
  action: ExtensionPresentationAction;
  pendingAction?: string;
  confirmingAction?: string;
  setPendingAction: (value: string | undefined) => void;
  setConfirmingAction: (value: string | undefined) => void;
  setActionError: (value: string | undefined) => void;
  onInvoke?: ActivityRailProps["onInvokeExtensionAction"];
}) {
  const actionKey = `${extension}:${extensionInstanceId}:${generation}:${revision}:${action.id}`;
  return (
    <span className="extension-action">
      <button
        type="button"
        data-destructive={action.destructive || undefined}
        disabled={pendingAction !== undefined || onInvoke === undefined}
        onClick={() => {
          if (!onInvoke) return;
          if (action.destructive && confirmingAction !== actionKey) {
            setConfirmingAction(actionKey);
            setActionError(undefined);
            return;
          }
          setConfirmingAction(undefined);
          setPendingAction(actionKey);
          setActionError(undefined);
          void onInvoke(
            extension,
            extensionInstanceId,
            generation,
            revision,
            action.id,
            action.destructive,
          )
            .catch((error: unknown) => {
              setActionError(
                error instanceof Error
                  ? error.message
                  : "The extension action failed.",
              );
            })
            .finally(() => setPendingAction(undefined));
        }}
      >
        {pendingAction === actionKey
          ? "Working…"
          : confirmingAction === actionKey
            ? `Confirm ${action.label}`
            : action.label}
      </button>
      {confirmingAction === actionKey ? (
        <button type="button" onClick={() => setConfirmingAction(undefined)}>
          Cancel
        </button>
      ) : null}
    </span>
  );
}

function ActivityRailView({
  session,
  open,
  onClose,
  onOpenOutput,
  onOpenSource,
  onOpenSession,
  onInvokeExtensionAction,
  onOpenResource,
  modal,
  onRestoreFocus,
  resourcesAvailable,
}: ActivityRailProps) {
  const railRef = useRef<HTMLElement>(null);
  const [openSections, setOpenSections] = useState({
    extensions: true,
    commands: false,
    progress: true,
    artifacts: true,
    context: true,
  });
  const [pendingAction, setPendingAction] = useState<string>();
  const [confirmingAction, setConfirmingAction] = useState<string>();
  const [actionError, setActionError] = useState<string>();
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
  const commands = session.items.filter(
    (item): item is ActionItem =>
      item.kind === "action" && item.actionKind === "command",
  );
  const extensionPresentations = session.extensionPresentations ?? [];
  const hasActivity = Boolean(
    extensionPresentations.length ||
    commands.length ||
      session.progress.length ||
      (resourcesAvailable && (session.outputs.length || session.sources.length)),
  );
  const canOpenExtensionResource = (handle: string) =>
    resourcesAvailable &&
    [...session.outputs, ...session.sources].some(
      (candidate) => candidate.handle === handle,
    );
  const canOpenExtensionArtifact = (artifactId: string) =>
    session.outputs.some((candidate) => candidate.id === artifactId) ||
    canOpenExtensionResource(artifactId);
  const openExtensionArtifact = (artifactId: string, label: string) => {
    const output = session.outputs.find((candidate) => candidate.id === artifactId);
    if (output) {
      onOpenOutput(output.id);
    } else if (resourcesAvailable && onOpenResource) {
      onOpenResource(artifactId, label, "text");
    }
  };
  const openExtensionResource = (handle: string, label: string) => {
    if (resourcesAvailable && onOpenResource) {
      onOpenResource(handle, label, "text");
    }
  };

  return (
    <aside
      ref={railRef}
      className={`activity-rail ${open ? "is-open" : ""}`}
      aria-label="Task activity"
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

        {extensionPresentations.length ? (
          <section
            className="rail-section extension-presentations"
            aria-labelledby="extensions-heading"
          >
            <details
              open={openSections.extensions}
              onToggle={(event) => {
                const open = event.currentTarget.open;
                setOpenSections((current) => ({
                  ...current,
                  extensions: open,
                }));
              }}
            >
              <summary>
                <span id="extensions-heading">Extensions</span>
                <em>{extensionPresentations.length}</em>
                <ChevronDown aria-hidden="true" />
              </summary>
              <div className="extension-presentation-list">
                {extensionPresentations.map((presentation) => {
                  const { snapshot } = presentation;
                  const actionsById = new Map(
                    snapshot.actions.map((action) => [action.id, action]),
                  );
                  const referencedActions = new Set(
                    snapshot.collection?.nodes.flatMap((node) => node.actionIds) ?? [],
                  );
                  const renderAction = (action: ExtensionPresentationAction) => (
                    <ExtensionActionControl
                      key={action.id}
                      extension={presentation.extension}
                      extensionInstanceId={presentation.extensionInstanceId}
                      generation={presentation.generation}
                      revision={snapshot.revision}
                      action={action}
                      pendingAction={pendingAction}
                      confirmingAction={confirmingAction}
                      setPendingAction={setPendingAction}
                      setConfirmingAction={setConfirmingAction}
                      setActionError={setActionError}
                      onInvoke={onInvokeExtensionAction}
                    />
                  );
                  return (
                    <article
                      key={`${presentation.extension}:${presentation.extensionInstanceId}`}
                      className="extension-presentation"
                    >
                      <header>
                        <strong>{presentation.extension}</strong>
                        {snapshot.status ? (
                          <span data-state={snapshot.status.state}>
                            {snapshot.status.label}
                          </span>
                        ) : null}
                      </header>
                      {snapshot.status?.detail ? (
                        <p>{snapshot.status.detail}</p>
                      ) : null}
                      {snapshot.activities.length ? (
                        <ol className="extension-activity-list">
                          {snapshot.activities.map((activity) => (
                            <li key={activity.id} data-state={activity.state}>
                              <Circle aria-hidden="true" />
                              <span>
                                <strong>{activity.summary}</strong>
                                <small>
                                  {[activity.kind, activity.provenance]
                                    .filter(Boolean)
                                    .join(" · ")}
                                </small>
                                <ExtensionReferences
                                  references={activity.references}
                                  onOpenSession={onOpenSession}
                                  onOpenArtifact={openExtensionArtifact}
                                  onOpenResource={openExtensionResource}
                                  canOpenArtifact={canOpenExtensionArtifact}
                                  canOpenResource={canOpenExtensionResource}
                                  resourcesAvailable={resourcesAvailable}
                                />
                              </span>
                            </li>
                          ))}
                        </ol>
                      ) : null}
                      {snapshot.collection ? (
                        <div className="extension-collection">
                          <h4>{snapshot.collection.title}</h4>
                          {snapshot.collection.nodes.length ? (
                            <ol>
                              {snapshot.collection.nodes.map((node) => (
                                <li
                                  key={node.id}
                                  data-state={node.state}
                                  data-selected={
                                    snapshot.collection?.selectedNodeId ===
                                    node.id
                                      ? "true"
                                      : undefined
                                  }
                                  style={{
                                    paddingInlineStart: `${
                                      extensionNodeDepth(
                                        snapshot.collection!.nodes,
                                        node,
                                      ) * 0.8
                                    }rem`,
                                  }}
                                >
                                  <span>{node.label}</span>
                                  {node.secondary ? (
                                    <small>{node.secondary}</small>
                                  ) : null}
                                  <ExtensionReferences
                                    references={node.references}
                                    onOpenSession={onOpenSession}
                                    onOpenArtifact={openExtensionArtifact}
                                    onOpenResource={openExtensionResource}
                                    canOpenArtifact={canOpenExtensionArtifact}
                                    canOpenResource={canOpenExtensionResource}
                                    resourcesAvailable={resourcesAvailable}
                                  />
                                  {node.actionIds.length ? (
                                    <div className="extension-actions" data-node={node.id}>
                                      {node.actionIds.map((actionId) => {
                                        const action = actionsById.get(actionId);
                                        return action ? renderAction(action) : null;
                                      })}
                                    </div>
                                  ) : null}
                                </li>
                              ))}
                            </ol>
                          ) : (
                            <p>No items.</p>
                          )}
                          {snapshot.collection.detail ? (
                            <div className="extension-detail">
                              <strong>
                                {snapshot.collection.detail.title}
                              </strong>
                              <pre>{snapshot.collection.detail.body}</pre>
                              <ExtensionReferences
                                references={
                                  snapshot.collection.detail.references
                                }
                                onOpenSession={onOpenSession}
                                onOpenArtifact={openExtensionArtifact}
                                onOpenResource={openExtensionResource}
                                canOpenArtifact={canOpenExtensionArtifact}
                                canOpenResource={canOpenExtensionResource}
                                resourcesAvailable={resourcesAvailable}
                              />
                            </div>
                          ) : null}
                        </div>
                      ) : null}
                      {snapshot.actions.some(
                        (action) => !referencedActions.has(action.id),
                      ) ? (
                        <div className="extension-actions" data-unscoped="true">
                          {snapshot.actions
                            .filter((action) => !referencedActions.has(action.id))
                            .map(renderAction)}
                        </div>
                      ) : null}
                    </article>
                  );
                })}
                {actionError ? <p role="alert">{actionError}</p> : null}
              </div>
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
    previous.session.extensionPresentations === next.session.extensionPresentations &&
    previous.open === next.open &&
    previous.onClose === next.onClose &&
    previous.onOpenOutput === next.onOpenOutput &&
    previous.onOpenSource === next.onOpenSource &&
    previous.onOpenSession === next.onOpenSession &&
    previous.onInvokeExtensionAction === next.onInvokeExtensionAction &&
    previous.onOpenResource === next.onOpenResource &&
    previous.modal === next.modal &&
    previous.onRestoreFocus === next.onRestoreFocus &&
    previous.resourcesAvailable === next.resourcesAvailable,
);
