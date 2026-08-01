import {
  Bot,
  Boxes,
  Braces,
  CircleDot,
  Database,
  Network,
  ShieldAlert,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import { useId, type ReactNode } from "react";
import type {
  ApprovalConsequence,
  CatalogReloadStatus,
  ChildAgentState,
  ContextCategory,
  DomainConsequence,
  FilesystemAccess,
  RuleSet,
  RuntimePolicyStatus,
  RuntimeSnapshot,
  SecretsConsequence,
  UnavailablePolicy,
} from "../runtime-status";
import "./RuntimeStatus.css";

export interface RuntimeStatusProps {
  snapshot: RuntimeSnapshot | null;
  loading?: boolean;
  error?: boolean;
}

const number = new Intl.NumberFormat();

const childStateLabel: Record<ChildAgentState, string> = {
  queued: "Queued",
  running: "Running",
  waiting: "Waiting",
  succeeded: "Succeeded",
  failed: "Failed",
  cancelled: "Cancelled",
};

const categoryLabel: Record<ContextCategory, string> = {
  system: "System",
  projectInstructions: "Project instructions",
  conversation: "Conversation",
  toolResults: "Tool results",
  attachments: "Attachments",
  documents: "Documents",
  projectFiles: "Project files",
  compactionSummaries: "Compaction summaries",
  other: "Other",
};

const accessLabel: Record<FilesystemAccess, string> = {
  none: "No filesystem access",
  trustedProjectRead: "Trusted project read",
  trustedProjectReadWrite: "Trusted project read and write",
};

const approvalLabel: Record<string, string> = {
  filesystemWrite: "Filesystem write",
  tool: "Tool invocation",
  command: "Command execution",
  remoteRead: "Remote read",
  processNetwork: "Process network",
  secretAccess: "Secret access",
};

function timestamp(value: number): { dateTime?: string; label: string } {
  if (value === 0) return { label: "Not yet observed" };
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return { label: "Unknown" };
  const dateTime = date.toISOString();
  return {
    dateTime,
    label: `${dateTime.slice(0, 10)} ${dateTime.slice(11, 19)} UTC`,
  };
}

function ObservedAt({ value }: { value: number }) {
  const observed = timestamp(value);
  return <time dateTime={observed.dateTime}>{observed.label}</time>;
}

function StatusPill({
  state,
  children,
}: {
  state: string;
  children: ReactNode;
}) {
  return (
    <span className={`runtime-status-pill is-${state}`} data-state={state}>
      {children}
    </span>
  );
}

function EmptyObservation({
  producer,
  children,
}: {
  producer: string;
  children: ReactNode;
}) {
  return (
    <div className="runtime-empty">
      <CircleDot aria-hidden="true" />
      <div>
        <strong>No {producer} observations are available</strong>
        <p>{children}</p>
      </div>
    </div>
  );
}

function RuntimeSection({
  id,
  icon,
  title,
  description,
  children,
}: {
  id: string;
  icon: ReactNode;
  title: string;
  description: string;
  children: ReactNode;
}) {
  return (
    <section className="runtime-section" aria-labelledby={id}>
      <header className="runtime-section-heading">
        <span aria-hidden="true">{icon}</span>
        <div>
          <h2 id={id}>{title}</h2>
          <p>{description}</p>
        </div>
      </header>
      {children}
    </section>
  );
}

function ChildAgents({
  snapshot,
  headingId,
}: {
  snapshot: RuntimeSnapshot;
  headingId: string;
}) {
  return (
    <RuntimeSection
      id={headingId}
      icon={<Bot />}
      title="Child agents"
      description="Host-published objectives, parentage, lifecycle, and redacted outcomes."
    >
      {snapshot.childAgents.length === 0 ? (
        <EmptyObservation producer="child-agent">
          An empty list does not prove that child agents are supported or idle.
        </EmptyObservation>
      ) : (
        <div className="runtime-card-list">
          {snapshot.childAgents.map((agent) => (
            <article key={agent.id} className="runtime-card">
              <header>
                <div>
                  <strong>{agent.objective}</strong>
                  <code>{agent.id}</code>
                </div>
                <StatusPill state={agent.state}>
                  {childStateLabel[agent.state]}
                </StatusPill>
              </header>
              <dl>
                <div>
                  <dt>Parent</dt>
                  <dd>{agent.parentId ? <code>{agent.parentId}</code> : "Root"}</dd>
                </div>
                <div>
                  <dt>Updated</dt>
                  <dd>
                    <ObservedAt value={agent.updatedAtMs} />
                  </dd>
                </div>
              </dl>
              {agent.outcome ? <p className="runtime-summary">{agent.outcome}</p> : null}
            </article>
          ))}
        </div>
      )}
    </RuntimeSection>
  );
}

function McpServers({
  snapshot,
  headingId,
}: {
  snapshot: RuntimeSnapshot;
  headingId: string;
}) {
  return (
    <RuntimeSection
      id={headingId}
      icon={<Network />}
      title="MCP servers"
      description="Trusted server lifecycle observations without host paths or configuration secrets."
    >
      {snapshot.mcpServers.length === 0 ? (
        <EmptyObservation producer="MCP server">
          No configured, starting, ready, failed, or stopped server was
          published by the host.
        </EmptyObservation>
      ) : (
        <div className="runtime-card-list">
          {snapshot.mcpServers.map((server) => (
            <article key={server.id} className="runtime-card">
              <header>
                <div>
                  <strong>{server.label}</strong>
                  <code>{server.id}</code>
                </div>
                <StatusPill state={server.state}>{server.state}</StatusPill>
              </header>
              <dl>
                <div>
                  <dt>Restarts</dt>
                  <dd>{number.format(server.restartCount)}</dd>
                </div>
                <div>
                  <dt>Updated</dt>
                  <dd>
                    <ObservedAt value={server.updatedAtMs} />
                  </dd>
                </div>
              </dl>
              {server.failure ? (
                <p className="runtime-summary is-failure">{server.failure}</p>
              ) : null}
            </article>
          ))}
        </div>
      )}
    </RuntimeSection>
  );
}

function reloadDescription(reload: CatalogReloadStatus): ReactNode {
  switch (reload.state) {
    case "idle":
      return "No catalog reload has been observed.";
    case "running":
      return (
        <>
          Reload <code>{reload.reloadId}</code> is running. Generation{" "}
          {number.format(reload.retainedGeneration)} remains active.
        </>
      );
    case "succeeded":
      return (
        <>
          Reload <code>{reload.reloadId}</code> committed generation{" "}
          {number.format(reload.generation)}.
        </>
      );
    case "failed":
      return (
        <>
          Reload <code>{reload.reloadId}</code> failed; generation{" "}
          {number.format(reload.retainedGeneration)} was retained.{" "}
          <span>{reload.failure}</span>
        </>
      );
  }
}

function Catalog({
  snapshot,
  headingId,
}: {
  snapshot: RuntimeSnapshot;
  headingId: string;
}) {
  return (
    <RuntimeSection
      id={headingId}
      icon={<Boxes />}
      title="Trusted catalog"
      description="Inert skill and extension metadata from the committed host catalog."
    >
      <div className="runtime-catalog-meta">
        <span>
          Generation <strong>{number.format(snapshot.catalog.generation)}</strong>
        </span>
        <span>
          Updated <ObservedAt value={snapshot.catalog.updatedAtMs} />
        </span>
        <StatusPill state={snapshot.catalog.reload.state}>
          Reload {snapshot.catalog.reload.state}
        </StatusPill>
      </div>
      <p className="runtime-reload">{reloadDescription(snapshot.catalog.reload)}</p>
      {snapshot.catalog.entries.length === 0 ? (
        <div className="runtime-empty is-compact">
          <CircleDot aria-hidden="true" />
          <div>
            <strong>No trusted catalog entries are published</strong>
            <p>The committed catalog currently exposes no skills or extensions.</p>
          </div>
        </div>
      ) : (
        <div className="runtime-card-list">
          {snapshot.catalog.entries.map((entry) => (
            <article key={entry.id} className="runtime-card runtime-catalog-entry">
              <header>
                <div>
                  <strong>{entry.label}</strong>
                  <span>
                    {entry.kind} · <code>{entry.id}</code>
                  </span>
                </div>
                <StatusPill state={entry.enabled ? "enabled" : "disabled"}>
                  {entry.enabled ? "Enabled" : "Disabled"}
                </StatusPill>
              </header>
              {entry.contributions.length ? (
                <ul aria-label={`${entry.label} contributions`}>
                  {entry.contributions.map((contribution) => (
                    <li key={`${contribution.kind}:${contribution.id}`}>
                      <span>{contribution.kind}</span>
                      <strong>{contribution.label}</strong>
                      <code>{contribution.id}</code>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="runtime-summary">No contributions are published.</p>
              )}
            </article>
          ))}
        </div>
      )}
    </RuntimeSection>
  );
}

function LanguageServers({
  snapshot,
  headingId,
}: {
  snapshot: RuntimeSnapshot;
  headingId: string;
}) {
  return (
    <RuntimeSection
      id={headingId}
      icon={<Braces />}
      title="Language servers"
      description="Aggregate diagnostics for trusted project and language identities."
    >
      {snapshot.lspServers.length === 0 ? (
        <EmptyObservation producer="language-server">
          No project/language producer is represented in this snapshot.
        </EmptyObservation>
      ) : (
        <div className="runtime-card-list">
          {snapshot.lspServers.map((server) => {
            const diagnostics = Object.values(server.diagnostics).reduce(
              (total, count) => total + count,
              0,
            );
            return (
              <article
                key={`${server.projectId}:${server.languageId}`}
                className="runtime-card"
              >
                <header>
                  <div>
                    <strong>{server.languageId}</strong>
                    <span>
                      Project <code>{server.projectId}</code>
                    </span>
                  </div>
                  <StatusPill state={server.state}>{server.state}</StatusPill>
                </header>
                <dl>
                  <div>
                    <dt>Diagnostics</dt>
                    <dd>{number.format(diagnostics)}</dd>
                  </div>
                  <div>
                    <dt>Revision</dt>
                    <dd>{number.format(server.diagnosticRevision)}</dd>
                  </div>
                  <div>
                    <dt>Errors</dt>
                    <dd>{number.format(server.diagnostics.errors)}</dd>
                  </div>
                  <div>
                    <dt>Warnings</dt>
                    <dd>{number.format(server.diagnostics.warnings)}</dd>
                  </div>
                </dl>
                {server.failure ? (
                  <p className="runtime-summary is-failure">{server.failure}</p>
                ) : null}
              </article>
            );
          })}
        </div>
      )}
    </RuntimeSection>
  );
}

function Context({
  snapshot,
  headingId,
}: {
  snapshot: RuntimeSnapshot;
  headingId: string;
}) {
  const { context } = snapshot;
  return (
    <RuntimeSection
      id={headingId}
      icon={<Database />}
      title="Context and compaction"
      description="Host-measured category totals that reconcile to the displayed token total."
    >
      <div className="runtime-context-total">
        <span>Current context</span>
        <strong>{number.format(context.current.totalTokens)} tokens</strong>
        <small>
          Updated <ObservedAt value={context.updatedAtMs} />
        </small>
      </div>
      {context.current.categories.length === 0 ? (
        <div className="runtime-empty is-compact">
          <CircleDot aria-hidden="true" />
          <div>
            <strong>No measured context categories</strong>
            <p>The reconciled context total is zero.</p>
          </div>
        </div>
      ) : (
        <dl className="runtime-context-categories">
          {context.current.categories.map((category) => {
            const percentage =
              context.current.totalTokens === 0
                ? 0
                : (category.tokens / context.current.totalTokens) * 100;
            return (
              <div key={category.category}>
                <dt>{categoryLabel[category.category]}</dt>
                <dd>
                  <span>{number.format(category.tokens)}</span>
                  <span
                    className="runtime-context-bar"
                    aria-hidden="true"
                  >
                    <span style={{ width: `${percentage}%` }} />
                  </span>
                </dd>
              </div>
            );
          })}
        </dl>
      )}
      <div className="runtime-compactions">
        {context.activeCompaction ? (
          <article className="runtime-compaction is-active">
            <Sparkles aria-hidden="true" />
            <div>
              <strong>Compaction in progress</strong>
              <p>
                <code>{context.activeCompaction.id}</code> started for a{" "}
                {context.activeCompaction.reason} trigger with{" "}
                {number.format(context.activeCompaction.before.totalTokens)} tokens.
              </p>
            </div>
          </article>
        ) : (
          <article className="runtime-compaction">
            <CircleDot aria-hidden="true" />
            <div>
              <strong>No active compaction</strong>
              <p>The host is not reporting a compaction in progress.</p>
            </div>
          </article>
        )}
        {context.lastCompaction ? (
          <article className="runtime-compaction">
            <Database aria-hidden="true" />
            <div>
              <strong>
                {context.lastCompaction.succeeded
                  ? "Last completed compaction"
                  : "Last compaction failed"}
              </strong>
              <p>
                {context.lastCompaction.succeeded ? (
                  <>
                    Reclaimed{" "}
                    {number.format(context.lastCompaction.reclaimedTokens)} tokens ({
                      number.format(context.lastCompaction.before.totalTokens)
                    }{" "}
                    to {number.format(context.lastCompaction.after.totalTokens)}).
                  </>
                ) : (
                  <>
                    The {context.lastCompaction.reason} attempt retained all{" "}
                    {number.format(context.lastCompaction.before.totalTokens)} tokens.
                  </>
                )}
              </p>
            </div>
          </article>
        ) : (
          <article className="runtime-compaction">
            <CircleDot aria-hidden="true" />
            <div>
              <strong>No completed compaction observed</strong>
              <p>No durable completed-compaction record is available.</p>
            </div>
          </article>
        )}
      </div>
    </RuntimeSection>
  );
}

function unavailableCopy(policy: UnavailablePolicy): ReactNode {
  return (
    <>
      <strong>Enforcement unavailable</strong>
      <span>{policy.reason}</span>
      <em>
        {policy.consequence === "featureBlocked"
          ? "Feature is blocked while enforcement is unavailable."
          : "Host behavior is unknown; ygg cannot attest to enforcement."}
      </em>
    </>
  );
}

function Rules({ rules }: { rules: RuleSet<string> }) {
  return (
    <>
      <span>
        Default: <strong>{rules.default}</strong>
      </span>
      <span>
        {number.format(rules.allow.length)} allowed ·{" "}
        {number.format(rules.deny.length)} denied
      </span>
    </>
  );
}

function DomainPolicyCopy({ consequence }: { consequence: DomainConsequence }) {
  return consequence.mode === "blocked" ? (
    <span>All access is blocked.</span>
  ) : (
    <Rules rules={consequence.domains} />
  );
}

function ApprovalPolicyCopy({
  consequence,
}: {
  consequence: ApprovalConsequence;
}) {
  return consequence.mode === "never" ? (
    <span>No listed operation requires host approval.</span>
  ) : (
    <span>
      Approval required for{" "}
      {consequence.operations
        .map((operation) => approvalLabel[operation] ?? operation)
        .join(", ")}
      .
    </span>
  );
}

function SecretsPolicyCopy({
  consequence,
}: {
  consequence: SecretsConsequence;
}) {
  return consequence.mode === "blocked" ? (
    <span>All secret access is blocked.</span>
  ) : (
    <span>
      {number.format(consequence.grants.length)} opaque named{" "}
      {consequence.grants.length === 1 ? "grant" : "grants"} permitted. Secret
      values are never displayed.
    </span>
  );
}

function PolicyCard({
  label,
  policy,
  children,
}: {
  label: string;
  policy: { status: "enforced" } | UnavailablePolicy;
  children?: ReactNode;
}) {
  const unavailable = policy.status === "unavailable";
  return (
    <article className={`runtime-policy-card ${unavailable ? "is-unavailable" : ""}`}>
      <header>
        {unavailable ? (
          <ShieldAlert aria-hidden="true" />
        ) : (
          <ShieldCheck aria-hidden="true" />
        )}
        <strong>{label}</strong>
        <StatusPill state={policy.status}>{policy.status}</StatusPill>
      </header>
      <div className="runtime-policy-copy">
        {unavailable ? unavailableCopy(policy) : children}
      </div>
    </article>
  );
}

function Policy({
  policy,
  headingId,
}: {
  policy: RuntimePolicyStatus | undefined;
  headingId: string;
}) {
  return (
    <RuntimeSection
      id={headingId}
      icon={<ShieldCheck />}
      title="Runtime policy"
      description="Observed enforcement consequences. This view does not grant or change authority."
    >
      {!policy ? (
        <div className="runtime-policy-unknown" role="status">
          <ShieldAlert aria-hidden="true" />
          <div>
            <strong>No authoritative policy observation is available</strong>
            <p>
              Filesystem, tool, command, network, approval, and secret behavior
              is unknown.
            </p>
          </div>
        </div>
      ) : (
        <>
          <div className="runtime-policy-meta">
            <span>
              Revision <strong>{number.format(policy.revision)}</strong>
            </span>
            <span>
              Observed <ObservedAt value={policy.observedAtMs} />
            </span>
          </div>
          <div className="runtime-policy-grid">
            <PolicyCard label="Filesystem" policy={policy.filesystem}>
              {policy.filesystem.status === "enforced" ? (
                <span>{accessLabel[policy.filesystem.access]}</span>
              ) : null}
            </PolicyCard>
            <PolicyCard label="Tools" policy={policy.tools}>
              {policy.tools.status === "enforced" ? (
                <Rules rules={policy.tools.rules} />
              ) : null}
            </PolicyCard>
            <PolicyCard label="Commands" policy={policy.commands}>
              {policy.commands.status === "enforced" ? (
                <Rules rules={policy.commands.rules} />
              ) : null}
            </PolicyCard>
            <PolicyCard label="Remote read" policy={policy.remoteRead}>
              {policy.remoteRead.status === "enforced" ? (
                <DomainPolicyCopy consequence={policy.remoteRead.consequence} />
              ) : null}
            </PolicyCard>
            <PolicyCard label="Process network" policy={policy.processNetwork}>
              {policy.processNetwork.status === "enforced" ? (
                <DomainPolicyCopy
                  consequence={policy.processNetwork.consequence}
                />
              ) : null}
            </PolicyCard>
            <PolicyCard label="Approvals" policy={policy.approvals}>
              {policy.approvals.status === "enforced" ? (
                <ApprovalPolicyCopy consequence={policy.approvals.consequence} />
              ) : null}
            </PolicyCard>
            <PolicyCard label="Secrets" policy={policy.secrets}>
              {policy.secrets.status === "enforced" ? (
                <SecretsPolicyCopy consequence={policy.secrets.consequence} />
              ) : null}
            </PolicyCard>
          </div>
        </>
      )}
    </RuntimeSection>
  );
}

export function RuntimeStatus({
  snapshot,
  loading = false,
  error = false,
}: RuntimeStatusProps) {
  const id = useId();
  const sectionId = (name: string) => `${id}-${name}`;

  if (loading && !snapshot) {
    return (
      <main className="runtime-view" aria-labelledby={`${id}-title`}>
        <header className="runtime-header">
          <span>Host observations</span>
          <h1 id={`${id}-title`}>Runtime status</h1>
        </header>
        <div className="runtime-page-state" role="status">
          <CircleDot aria-hidden="true" />
          Loading runtime observations…
        </div>
      </main>
    );
  }

  if (error && !snapshot) {
    return (
      <main className="runtime-view" aria-labelledby={`${id}-title`}>
        <header className="runtime-header">
          <span>Host observations</span>
          <h1 id={`${id}-title`}>Runtime status</h1>
        </header>
        <div className="runtime-page-state is-error" role="alert">
          <ShieldAlert aria-hidden="true" />
          Runtime observations are temporarily unavailable.
        </div>
      </main>
    );
  }

  if (!snapshot) {
    return (
      <main className="runtime-view" aria-labelledby={`${id}-title`}>
        <header className="runtime-header">
          <span>Host observations</span>
          <h1 id={`${id}-title`}>Runtime status</h1>
        </header>
        <div className="runtime-page-state" role="status">
          <CircleDot aria-hidden="true" />
          No runtime snapshot has been observed.
        </div>
      </main>
    );
  }

  return (
    <main className="runtime-view" aria-labelledby={`${id}-title`}>
      <header className="runtime-header">
        <span>Host observations</span>
        <h1 id={`${id}-title`}>Runtime status</h1>
        <p>
          Path-free, bounded runtime facts from the connected ygg host. Status
          does not imply a producer or control that the host did not publish.
        </p>
        {error ? (
          <p className="runtime-stale" role="status">
            Showing the last available snapshot; refresh is currently
            unavailable.
          </p>
        ) : null}
      </header>

      <div className="runtime-overview" aria-label="Runtime observation totals">
        <div>
          <Bot aria-hidden="true" />
          <span>Child agents</span>
          <strong>{number.format(snapshot.childAgents.length)}</strong>
        </div>
        <div>
          <Network aria-hidden="true" />
          <span>MCP servers</span>
          <strong>{number.format(snapshot.mcpServers.length)}</strong>
        </div>
        <div>
          <Braces aria-hidden="true" />
          <span>Language servers</span>
          <strong>{number.format(snapshot.lspServers.length)}</strong>
        </div>
        <div>
          <Database aria-hidden="true" />
          <span>Context tokens</span>
          <strong>{number.format(snapshot.context.current.totalTokens)}</strong>
        </div>
      </div>

      <ChildAgents snapshot={snapshot} headingId={sectionId("agents")} />
      <McpServers snapshot={snapshot} headingId={sectionId("mcp")} />
      <Catalog snapshot={snapshot} headingId={sectionId("catalog")} />
      <LanguageServers snapshot={snapshot} headingId={sectionId("lsp")} />
      <Context snapshot={snapshot} headingId={sectionId("context")} />
      <Policy policy={snapshot.policy} headingId={sectionId("policy")} />
    </main>
  );
}
