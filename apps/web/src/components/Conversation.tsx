import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BrainCircuit,
  Check,
  ChevronDown,
  CircleStop,
  ExternalLink,
  File,
  FileCode2,
  FileDiff,
  FileText,
  Globe2,
  LoaderCircle,
  Paperclip,
  Plus,
  Search,
  ScanSearch,
  ShieldAlert,
  ShieldCheck,
  TerminalSquare,
  X,
} from "lucide-react";
import {
  type CSSProperties,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type {
  ActionItem,
  AttachmentRef,
  AuthorityProfile,
  HostBootstrap,
  ReasoningEffort,
  SessionSnapshot,
  TranscriptItem,
} from "../protocol";
import { YggGlyph } from "./YggGlyph";

interface ConversationProps {
  session: SessionSnapshot;
  bootstrap: HostBootstrap;
  onSubmit: (
    prompt: string,
    attachments: AttachmentRef[],
    activeDelivery?: "steer" | "followUp",
  ) => Promise<void>;
  onInterrupt: () => Promise<void>;
  onConfigure: (patch: {
    modelId?: string;
    reasoning?: ReasoningEffort;
    authority?: AuthorityProfile;
  }) => Promise<void>;
  onResolveApproval: (
    requestId: string,
    decision: "allowed_once" | "allowed_session" | "denied",
  ) => Promise<void>;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
}

const actionIcons: Record<ActionItem["actionKind"], ReactNode> = {
  command: <TerminalSquare aria-hidden="true" />,
  file_read: <FileText aria-hidden="true" />,
  file_write: <FileDiff aria-hidden="true" />,
  web_search: <Search aria-hidden="true" />,
  preview: <Globe2 aria-hidden="true" />,
  analysis: <ScanSearch aria-hidden="true" />,
};

const authorityLabels: Record<AuthorityProfile, string> = {
  readOnly: "Read only",
  workspace: "Workspace",
  fullAccess: "Full access",
};

const providerAccents: Array<[string, string]> = [
  ["openai", "#1f1f1f"],
  ["anthropic", "#cc785c"],
  ["google", "#34a853"],
  ["xai", "#736cd3"],
  ["meta", "#0089f4"],
  ["mistral", "#fd6f00"],
  ["deepseek", "#2243e6"],
  ["alibaba", "#ff7018"],
  ["minimax", "#eb3568"],
  ["kimi", "#047afe"],
  ["z.ai", "#1c7ff8"],
  ["nvidia", "#86b737"],
  ["xiaomi", "#ff6900"],
  ["cohere", "#d18ee2"],
  ["amazon", "#ff9900"],
  ["microsoft", "#0078d5"],
  ["ai21", "#d63864"],
  ["bytedance", "#3c8bff"],
  ["perplexity", "#1b818e"],
  ["ibm", "#0f62fe"],
  ["baidu", "#2436d8"],
  ["tencent", "#5cb9ff"],
  ["allenai", "#f0529c"],
];

const thinkingIntensity: Record<string, number> = {
  off: 0,
  minimal: 0.2,
  low: 0.35,
  medium: 0.5,
  high: 0.7,
  xhigh: 0.85,
  max: 1,
};

function balancedAccent(source: string, targetLuminance: number): string {
  const match = /^#([\da-f]{2})([\da-f]{2})([\da-f]{2})$/i.exec(source);
  if (!match) return source;
  const rgb = match.slice(1).map((channel) => Number.parseInt(channel, 16));
  const luminance = (channels: number[]) =>
    channels
      .map((channel) => channel / 255)
      .map((channel) =>
        channel <= 0.04045
          ? channel / 12.92
          : ((channel + 0.055) / 1.055) ** 2.4,
      )
      .reduce(
        (sum, channel, index) =>
          sum + channel * ([0.2126, 0.7152, 0.0722][index] ?? 0),
        0,
      );
  const sourceLuminance = luminance(rgb);
  if (Math.abs(sourceLuminance - targetLuminance) < 0.01) return source;
  const destination = sourceLuminance < targetLuminance ? 255 : 0;
  let low = 0;
  let high = 1;
  let result = rgb;
  for (let iteration = 0; iteration < 18; iteration += 1) {
    const amount = (low + high) / 2;
    const candidate = rgb.map((channel) =>
      Math.round(channel + (destination - channel) * amount),
    );
    const candidateLuminance = luminance(candidate);
    result = candidate;
    if (
      (destination === 255 && candidateLuminance < targetLuminance) ||
      (destination === 0 && candidateLuminance > targetLuminance)
    ) {
      low = amount;
    } else {
      high = amount;
    }
  }
  return `rgb(${result.join(" ")})`;
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1_000) return `${durationMs}ms`;
  const seconds = Math.round(durationMs / 1_000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}

function ActionCell({
  item,
  onOpenOutput,
  onOpenSource,
  availableOutputIds,
  availableSourceIds,
}: {
  item: ActionItem;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  availableOutputIds: ReadonlySet<string>;
  availableSourceIds: ReadonlySet<string>;
}) {
  const isStreaming = item.state === "streaming";
  const sourceIds = item.sourceIds?.filter((id) =>
    availableSourceIds.has(id),
  );
  const outputIds = item.outputIds?.filter((id) =>
    availableOutputIds.has(id),
  );
  return (
    <details className="action-cell" open={isStreaming}>
      <summary>
        <span className={`action-glyph ${isStreaming ? "is-live" : ""}`}>
          {isStreaming ? (
            <LoaderCircle className="spin" aria-hidden="true" />
          ) : (
            actionIcons[item.actionKind]
          )}
        </span>
        <span className="action-title">
          <strong>{item.label}</strong>
          {item.target ? <code>{item.target}</code> : null}
        </span>
        {typeof item.additions === "number" ? (
          <span className="diff-count">
            <em>+{item.additions}</em>
            <b>−{item.deletions ?? 0}</b>
          </span>
        ) : null}
        {item.durationMs ? (
          <span className="action-duration">
            {formatDuration(item.durationMs)}
          </span>
        ) : null}
        <ChevronDown className="disclosure-chevron" aria-hidden="true" />
      </summary>
      <div className="action-detail">
        {item.detail ? <p>{item.detail}</p> : null}
        {sourceIds?.length ? (
          <div className="action-links">
            {sourceIds.map((sourceId) => (
              <button key={sourceId} onClick={() => onOpenSource(sourceId)}>
                <File aria-hidden="true" />
                Open source
              </button>
            ))}
          </div>
        ) : null}
        {outputIds?.length ? (
          <div className="action-links">
            {outputIds.map((outputId) => (
              <button key={outputId} onClick={() => onOpenOutput(outputId)}>
                <ExternalLink aria-hidden="true" />
                Open output
              </button>
            ))}
          </div>
        ) : null}
      </div>
    </details>
  );
}

function TranscriptItemView({
  item,
  onResolveApproval,
  onOpenOutput,
  onOpenSource,
  availableOutputIds,
  availableSourceIds,
}: {
  item: TranscriptItem;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  availableOutputIds: ReadonlySet<string>;
  availableSourceIds: ReadonlySet<string>;
}) {
  switch (item.kind) {
    case "user_message":
      return (
        <article className="user-message">
          <div className="message-copy">{item.content}</div>
          {item.attachments?.length ? (
            <div className="message-attachments">
              {item.attachments.map((attachment) => (
                <span key={attachment.id}>
                  <Paperclip aria-hidden="true" />
                  {attachment.name}
                </span>
              ))}
            </div>
          ) : null}
        </article>
      );

    case "assistant_message":
      return (
        <article
          className={`assistant-message ${item.state === "streaming" ? "is-streaming" : ""}`}
          aria-live={item.state === "streaming" ? "polite" : undefined}
        >
          <div className="message-copy">
            {item.content || (
              <LoaderCircle
                className="spin assistant-waiting"
                aria-label="ygg is responding"
              />
            )}
          </div>
        </article>
      );

    case "reasoning":
      return (
        <details
          className={`reasoning-block ${item.state === "streaming" ? "is-live" : ""}`}
          open={item.state === "streaming"}
        >
          <summary>
            {item.state === "streaming" ? (
              <LoaderCircle className="spin" aria-hidden="true" />
            ) : (
              <BrainCircuit aria-hidden="true" />
            )}
            <span>{item.summary}</span>
            <ChevronDown aria-hidden="true" />
          </summary>
          <p>{item.content}</p>
        </details>
      );

    case "action":
      return (
        <ActionCell
          item={item}
          onOpenOutput={onOpenOutput}
          onOpenSource={onOpenSource}
          availableOutputIds={availableOutputIds}
          availableSourceIds={availableSourceIds}
        />
      );

    case "approval":
      return (
        <section className="approval-card" aria-label="Approval needed">
          <div className="approval-icon">
            {item.resolved ? (
              <ShieldCheck aria-hidden="true" />
            ) : (
              <ShieldAlert aria-hidden="true" />
            )}
          </div>
          <div className="approval-copy">
            <span className="approval-eyebrow">
              {item.resolved ? "Resolved" : "Your approval is needed"}
            </span>
            <h3>{item.title}</h3>
            <p>{item.description}</p>
            <span className="approval-scope">{item.scopeLabel}</span>
            {item.resolved ? (
              <div className="approval-resolved">
                <Check aria-hidden="true" />
                {item.resolved === "denied"
                  ? "Denied"
                  : item.resolved === "allowed_session"
                    ? "Allowed for this session"
                    : "Allowed once"}
              </div>
            ) : (
              <div className="approval-actions">
                <button
                  className="secondary-button"
                  onClick={() => onResolveApproval(item.requestId, "denied")}
                >
                  Deny
                </button>
                <button
                  className="primary-button"
                  onClick={() =>
                    onResolveApproval(item.requestId, "allowed_once")
                  }
                >
                  Allow once
                </button>
              </div>
            )}
          </div>
        </section>
      );

    case "run_outcome":
      return (
        <div className={`run-outcome is-${item.outcome}`}>
          <span>
            {item.outcome === "done" ? (
              <Check aria-hidden="true" />
            ) : item.outcome === "failed" ? (
              <AlertTriangle aria-hidden="true" />
            ) : (
              <CircleStop aria-hidden="true" />
            )}
          </span>
          <strong>{item.summary}</strong>
          <em>Worked for {formatDuration(item.durationMs)}</em>
        </div>
      );
  }
}

function EmptySession({ attachments }: { attachments: boolean }) {
  return (
    <div className="empty-session">
      <div className="empty-session-mark" aria-hidden="true">
        <YggGlyph />
      </div>
      <h1>What can I take off your plate?</h1>
      <p>
        {attachments
          ? "Describe a task, from a quick fix to a multi-step job. You can attach files and folders."
          : "Describe a task, from a quick fix to a multi-step job."}
      </p>
    </div>
  );
}

function Composer({
  session,
  bootstrap,
  onSubmit,
  onInterrupt,
  onConfigure,
}: Pick<
  ConversationProps,
  "session" | "bootstrap" | "onSubmit" | "onInterrupt" | "onConfigure"
>) {
  const [prompt, setPrompt] = useState("");
  const [attachments, setAttachments] = useState<AttachmentRef[]>([]);
  const [activeDelivery, setActiveDelivery] = useState<
    "steer" | "followUp"
  >("followUp");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const isWorking = session.status === "working";
  const activeModel = bootstrap.models.find(
    (model) => model.id === session.modelId,
  );
  const providerKey =
    `${activeModel?.provider ?? ""} ${activeModel?.id ?? ""}`.toLowerCase();
  const modelAccent =
    providerAccents.find(([provider]) => providerKey.includes(provider))?.[1] ??
    "var(--theme-pigment)";
  const intensity = thinkingIntensity[session.reasoning] ?? 0.5;
  const composerStyle = {
    "--model-accent-light": balancedAccent(modelAccent, 0.11),
    "--model-accent-dark": balancedAccent(modelAccent, 0.27),
    "--thinking-intensity": intensity,
    "--thinking-opacity": 0.4 + intensity * 0.55,
  } as CSSProperties;

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "0";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }, [prompt]);

  const submit = async () => {
    if (!prompt.trim() || submitting) return;
    if (
      isWorking &&
      ((activeDelivery === "steer" && !bootstrap.capabilities.steer) ||
        (activeDelivery === "followUp" && !bootstrap.capabilities.followUp))
    ) {
      return;
    }
    const value = prompt;
    const submittedAttachments = attachments;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await onSubmit(
        value,
        submittedAttachments,
        isWorking ? activeDelivery : undefined,
      );
      setPrompt((current) => (current === value ? "" : current));
      setAttachments((current) =>
        current === submittedAttachments ? [] : current,
      );
    } catch (error) {
      setSubmitError(
        error instanceof Error
          ? error.message
          : "ygg could not send this message.",
      );
    } finally {
      setSubmitting(false);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  };

  const addAttachments = (event: ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(event.target.files ?? []);
    setAttachments((current) => [
      ...current,
      ...files.map((file) => ({
        id: crypto.randomUUID(),
        name: file.name,
        mediaType: file.type || "application/octet-stream",
        size: file.size,
      })),
    ]);
    event.target.value = "";
  };

  return (
    <div className="composer-wrap">
      <div
        className={`composer ${isWorking ? "is-working" : ""}`}
        style={composerStyle}
      >
        {attachments.length ? (
          <div className="composer-attachments">
            {attachments.map((attachment) => (
              <span key={attachment.id}>
                <FileCode2 aria-hidden="true" />
                {attachment.name}
                <button
                  onClick={() =>
                    setAttachments((current) =>
                      current.filter((item) => item.id !== attachment.id),
                    )
                  }
                  aria-label={`Remove ${attachment.name}`}
                >
                  <X aria-hidden="true" />
                </button>
              </span>
            ))}
          </div>
        ) : null}
        <textarea
          ref={textareaRef}
          value={prompt}
          onChange={(event) => setPrompt(event.target.value)}
          onKeyDown={onKeyDown}
          placeholder={
            isWorking
              ? activeDelivery === "steer"
                ? "Steer the active run…"
                : "Queue a follow-up…"
              : session.items.length
                ? "Reply…"
                : "Describe a task…"
          }
          rows={1}
          aria-label="Message ygg"
          aria-describedby={submitError ? "composer-send-error" : undefined}
        />
        <div className="composer-toolbar">
          <div className="composer-leading">
            {bootstrap.capabilities.attachments &&
            bootstrap.capabilities.attachmentIngest ? (
              <>
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  hidden
                  onChange={addAttachments}
                />
                <button
                  className="composer-icon-button"
                  onClick={() => fileInputRef.current?.click()}
                  aria-label="Attach files"
                  title="Attach files"
                >
                  <Plus aria-hidden="true" />
                </button>
              </>
            ) : null}
            <label className="composer-select model-select">
              <span className="sr-only">Model</span>
              <select
                value={session.modelId}
                disabled={isWorking}
                onChange={(event) =>
                  void onConfigure({ modelId: event.target.value })
                }
              >
                {bootstrap.models.map((model) => (
                  <option
                    key={model.id}
                    value={model.id}
                    disabled={!model.available}
                  >
                    {model.local ? "● " : ""}
                    {model.name}
                  </option>
                ))}
              </select>
              <ChevronDown aria-hidden="true" />
            </label>
            <label className="composer-select">
              <span className="sr-only">Reasoning effort</span>
              <select
                value={session.reasoning}
                disabled={isWorking}
                onChange={(event) =>
                  void onConfigure({
                    reasoning: event.target.value as ReasoningEffort,
                  })
                }
              >
                {(
                  bootstrap.models.find(
                    (model) => model.id === session.modelId,
                  )?.reasoning ?? [session.reasoning]
                ).map((reasoning) => (
                  <option key={reasoning} value={reasoning}>
                    {reasoning}
                  </option>
                ))}
              </select>
              <ChevronDown aria-hidden="true" />
            </label>
            <label className="composer-select authority-select">
              <ShieldCheck aria-hidden="true" />
              <span className="sr-only">Authority</span>
              <select
                value={session.authority}
                disabled={isWorking}
                onChange={(event) =>
                  void onConfigure({
                    authority: event.target.value as AuthorityProfile,
                  })
                }
              >
                {bootstrap.authorityProfiles.map((authority) => (
                  <option key={authority} value={authority}>
                    {authorityLabels[authority]}
                  </option>
                ))}
              </select>
              <ChevronDown aria-hidden="true" />
            </label>
            {isWorking ? (
              <label className="composer-select delivery-select">
                <span className="sr-only">Active run delivery</span>
                <select
                  value={activeDelivery}
                  onChange={(event) =>
                    setActiveDelivery(
                      event.target.value as "steer" | "followUp",
                    )
                  }
                >
                  {bootstrap.capabilities.steer ? (
                    <option value="steer">Steer now</option>
                  ) : null}
                  {bootstrap.capabilities.followUp ? (
                    <option value="followUp">Follow up</option>
                  ) : null}
                </select>
                <ChevronDown aria-hidden="true" />
              </label>
            ) : null}
          </div>
          <div className="composer-actions">
            {isWorking ? (
              <button
                className="submit-button stop-button"
                onClick={() => void onInterrupt()}
                aria-label="Stop ygg"
              >
                <CircleStop aria-hidden="true" />
              </button>
            ) : null}
            <button
              className="submit-button"
              onClick={() => void submit()}
              disabled={submitting || !prompt.trim()}
              aria-disabled={submitting || !prompt.trim()}
              aria-label={
                isWorking
                  ? activeDelivery === "steer"
                    ? "Steer active run"
                    : "Queue follow-up"
                  : "Send message"
              }
            >
              <ArrowUp aria-hidden="true" />
            </button>
          </div>
        </div>
        {submitError ? (
          <p id="composer-send-error" className="composer-error" role="alert">
            {submitError}
          </p>
        ) : null}
      </div>
    </div>
  );
}

export function Conversation({
  session,
  bootstrap,
  onSubmit,
  onInterrupt,
  onConfigure,
  onResolveApproval,
  onOpenOutput,
  onOpenSource,
}: ConversationProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [showJump, setShowJump] = useState(false);
  const shouldStickRef = useRef(true);
  const resourcesAvailable = bootstrap.capabilities.resources;
  const availableOutputIds = new Set(
    resourcesAvailable ? session.outputs.map((output) => output.id) : [],
  );
  const availableSourceIds = new Set(
    resourcesAvailable ? session.sources.map((source) => source.id) : [],
  );

  useEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    const onScroll = () => {
      const distance =
        element.scrollHeight - element.scrollTop - element.clientHeight;
      shouldStickRef.current = distance < 120;
      setShowJump(distance >= 180);
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    return () => element.removeEventListener("scroll", onScroll);
  }, [session.sessionId]);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element || !shouldStickRef.current) return;
    element.scrollTop = element.scrollHeight;
  }, [session.items, session.sessionId]);

  const jumpToLive = () => {
    const element = scrollRef.current;
    if (!element) return;
    const reduceMotion =
      window.matchMedia("(prefers-reduced-motion: reduce)").matches ||
      document.documentElement.dataset.motion === "reduced" ||
      document.documentElement.dataset.motion === "none";
    element.scrollTo({
      top: element.scrollHeight,
      behavior: reduceMotion ? "auto" : "smooth",
    });
  };

  return (
    <section className="conversation" aria-label="Conversation">
      <div className="transcript-scroll" ref={scrollRef}>
        <div
          className={`transcript ${session.items.length === 0 ? "is-empty" : ""}`}
        >
          {session.items.length === 0 ? (
            <EmptySession
              attachments={
                bootstrap.capabilities.attachments &&
                bootstrap.capabilities.attachmentIngest
              }
            />
          ) : (
            session.items.map((item) => (
              <TranscriptItemView
                key={item.id}
                item={item}
                onResolveApproval={onResolveApproval}
                onOpenOutput={onOpenOutput}
                onOpenSource={onOpenSource}
                availableOutputIds={availableOutputIds}
                availableSourceIds={availableSourceIds}
              />
            ))
          )}
        </div>
      </div>
      {showJump ? (
        <button className="jump-to-live" onClick={jumpToLive}>
          <ArrowDown aria-hidden="true" />
          Jump to latest
        </button>
      ) : null}
      <Composer
        session={session}
        bootstrap={bootstrap}
        onSubmit={onSubmit}
        onInterrupt={onInterrupt}
        onConfigure={onConfigure}
      />
    </section>
  );
}
