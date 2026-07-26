import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BrainCircuit,
  Check,
  ChevronDown,
  CircleStop,
  Code2,
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
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import yggIconUrl from "../../../../docs/assets/ygg-braille.svg";
import type {
  ActionItem,
  AttachmentRef,
  AuthorityProfile,
  HostBootstrap,
  ReasoningEffort,
  SessionSnapshot,
  TranscriptItem,
} from "../protocol";

interface ConversationProps {
  session: SessionSnapshot;
  bootstrap: HostBootstrap;
  onSubmit: (prompt: string, attachments: AttachmentRef[]) => Promise<void>;
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
}: {
  item: ActionItem;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
}) {
  const isStreaming = item.state === "streaming";
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
        {item.sourceIds?.length ? (
          <div className="action-links">
            {item.sourceIds.map((sourceId) => (
              <button key={sourceId} onClick={() => onOpenSource(sourceId)}>
                <File aria-hidden="true" />
                Open source
              </button>
            ))}
          </div>
        ) : null}
        {item.outputIds?.length ? (
          <div className="action-links">
            {item.outputIds.map((outputId) => (
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
}: {
  item: TranscriptItem;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
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
                aria-label="Ygg is responding"
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
                  className="secondary-button"
                  onClick={() =>
                    onResolveApproval(item.requestId, "allowed_session")
                  }
                >
                  Allow for session
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

function EmptySession() {
  return (
    <div className="empty-session">
      <div className="empty-session-mark" aria-hidden="true">
        <img src={yggIconUrl} alt="" />
      </div>
      <h1>What should we work on?</h1>
      <p>
        Ask a question, attach a file, or describe the result you want. Ygg
        keeps this session local and inspectable.
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
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const isWorking = session.status === "working";

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "0";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }, [prompt]);

  const submit = async () => {
    if (!prompt.trim() || isWorking) return;
    const value = prompt;
    const submittedAttachments = attachments;
    setPrompt("");
    setAttachments([]);
    await onSubmit(value, submittedAttachments);
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
      <div className={`composer ${isWorking ? "is-working" : ""}`}>
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
              ? "Add a follow-up when Ygg is ready…"
              : "Message ygg"
          }
          rows={1}
          aria-label="Message ygg"
        />
        <div className="composer-toolbar">
          <div className="composer-leading">
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
            <label className="composer-select model-select">
              <span className="sr-only">Model</span>
              <select
                value={session.modelId}
                onChange={(event) =>
                  void onConfigure({ modelId: event.target.value })
                }
              >
                {bootstrap.models.map((model) => (
                  <option key={model.id} value={model.id}>
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
          </div>
          {isWorking ? (
            <button
              className="submit-button stop-button"
              onClick={() => void onInterrupt()}
              aria-label="Stop Ygg"
            >
              <CircleStop aria-hidden="true" />
            </button>
          ) : (
            <button
              className="submit-button"
              onClick={() => void submit()}
              disabled={!prompt.trim()}
              aria-label="Send message"
            >
              <ArrowUp aria-hidden="true" />
            </button>
          )}
        </div>
      </div>
      <div className="composer-note">
        <span>
          <Code2 aria-hidden="true" />
          {session.authority === "fullAccess"
            ? "Broad Ygg authority"
            : session.authority === "workspace"
              ? "Mutations stay in this workspace"
              : "Read only — no mutations"}
        </span>
        <span>{session.contextPercent}% context</span>
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
            <EmptySession />
          ) : (
            session.items.map((item) => (
              <TranscriptItemView
                key={item.id}
                item={item}
                onResolveApproval={onResolveApproval}
                onOpenOutput={onOpenOutput}
                onOpenSource={onOpenSource}
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
