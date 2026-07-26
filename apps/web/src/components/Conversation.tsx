import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BrainCircuit,
  Check,
  ChevronDown,
  CircleStop,
  Copy,
  Download,
  ExternalLink,
  File,
  FileDiff,
  FileText,
  Globe2,
  LoaderCircle,
  Maximize2,
  Minus,
  Paperclip,
  Plus,
  RefreshCw,
  Search,
  ScanSearch,
  ShieldAlert,
  ShieldCheck,
  TerminalSquare,
  X,
} from "lucide-react";
import {
  Suspense,
  type CSSProperties,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  lazy,
  useCallback,
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
  ModelSummary,
  ReasoningEffort,
  SessionSnapshot,
  TranscriptItem,
} from "../protocol";
import { YggGlyph } from "./YggGlyph";

const MarkdownMessage = lazy(() => import("./MarkdownMessage"));

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
  onResolveUserInput: (
    requestId: string,
    answer:
      | { type: "text"; text: string }
      | { type: "choice"; choice: string },
  ) => Promise<void>;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  onIngestAttachment?: (file: File) => Promise<AttachmentRef>;
  attachmentContentUrl?: (handle: string) => string;
}

type DraftAttachment = {
  localId: string;
  file: File;
  previewUrl?: string;
  status: "uploading" | "uploaded" | "failed";
  reference?: AttachmentRef;
  error?: string;
};

function extensionLabel(name: string): string {
  const extension = name.split(".").at(-1);
  return extension && extension !== name
    ? extension.slice(0, 4).toUpperCase()
    : "FILE";
}

function AttachmentPreviewDialog({
  source,
  name,
  onClose,
}: {
  source: string | null;
  name: string;
  onClose: () => void;
}) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const imageRef = useRef<HTMLImageElement>(null);
  const dragRef = useRef<{
    pointerId: number;
    clientX: number;
    clientY: number;
    originX: number;
    originY: number;
  } | null>(null);
  const movedRef = useRef(false);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const [dragging, setDragging] = useState(false);

  const clampedPan = useCallback(
    (x: number, y: number, nextZoom: number) => {
      const image = imageRef.current;
      if (!image || nextZoom <= 1) return { x: 0, y: 0 };
      const maxX = Math.max(0, (image.offsetWidth * (nextZoom - 1)) / 2);
      const maxY = Math.max(0, (image.offsetHeight * (nextZoom - 1)) / 2);
      return {
        x: Math.max(-maxX, Math.min(maxX, x)),
        y: Math.max(-maxY, Math.min(maxY, y)),
      };
    },
    [],
  );

  const changeZoom = useCallback(
    (nextZoom: number) => {
      const value = Math.max(1, Math.min(4, nextZoom));
      setZoom(value);
      setPan((current) => clampedPan(current.x, current.y, value));
    },
    [clampedPan],
  );

  const fitImage = useCallback(() => {
    setZoom(1);
    setPan({ x: 0, y: 0 });
  }, []);

  useEffect(() => {
    if (!source) return;
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      } else if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        changeZoom(zoom + 0.5);
      } else if (event.key === "-") {
        event.preventDefault();
        changeZoom(zoom - 0.5);
      } else if (event.key === "0") {
        event.preventDefault();
        fitImage();
      }
      if (event.key === "Tab") {
        const controls = Array.from(
          dialogRef.current?.querySelectorAll<HTMLElement>(
            'button:not([disabled]), a[href]',
          ) ?? [],
        );
        if (!controls.length) return;
        const first = controls[0]!;
        const last = controls.at(-1)!;
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [changeZoom, fitImage, onClose, source, zoom]);
  if (!source) return null;
  return (
    <div
      ref={dialogRef}
      className="attachment-preview-backdrop"
      role="dialog"
      aria-modal="true"
      aria-label={`Preview ${name}`}
      onMouseDown={(event) => {
        if (event.currentTarget === event.target) onClose();
      }}
    >
      <div
        className={`attachment-preview-stage ${dragging ? "is-dragging" : ""}`}
        data-zoomed={zoom > 1 || undefined}
        onWheel={(event) => {
          event.preventDefault();
          changeZoom(zoom + (event.deltaY < 0 ? 0.25 : -0.25));
        }}
        onPointerDown={(event) => {
          if (zoom <= 1 || event.button !== 0) return;
          event.currentTarget.setPointerCapture(event.pointerId);
          dragRef.current = {
            pointerId: event.pointerId,
            clientX: event.clientX,
            clientY: event.clientY,
            originX: pan.x,
            originY: pan.y,
          };
          movedRef.current = false;
          setDragging(true);
        }}
        onPointerMove={(event) => {
          const drag = dragRef.current;
          if (!drag || drag.pointerId !== event.pointerId) return;
          const deltaX = event.clientX - drag.clientX;
          const deltaY = event.clientY - drag.clientY;
          if (Math.abs(deltaX) + Math.abs(deltaY) > 3) {
            movedRef.current = true;
          }
          setPan(
            clampedPan(drag.originX + deltaX, drag.originY + deltaY, zoom),
          );
        }}
        onPointerUp={(event) => {
          if (dragRef.current?.pointerId !== event.pointerId) return;
          dragRef.current = null;
          setDragging(false);
          event.currentTarget.releasePointerCapture(event.pointerId);
        }}
        onPointerCancel={() => {
          dragRef.current = null;
          setDragging(false);
        }}
        onClick={(event) => {
          if (event.target !== imageRef.current) return;
          if (movedRef.current) {
            movedRef.current = false;
            return;
          }
          changeZoom(zoom > 1 ? 1 : 2);
        }}
      >
        <img
          ref={imageRef}
          src={source}
          alt={name}
          draggable={false}
          style={{
            transform: `translate3d(${pan.x}px, ${pan.y}px, 0) scale(${zoom})`,
          }}
        />
      </div>
      <div className="attachment-preview-toolbar" aria-label="Image controls">
        <button
          type="button"
          onClick={() => changeZoom(zoom - 0.5)}
          disabled={zoom <= 1}
          aria-label="Zoom out"
          title="Zoom out (−)"
        >
          <Minus aria-hidden="true" />
        </button>
        <output aria-live="polite">{Math.round(zoom * 100)}%</output>
        <button
          type="button"
          onClick={() => changeZoom(zoom + 0.5)}
          disabled={zoom >= 4}
          aria-label="Zoom in"
          title="Zoom in (+)"
        >
          <Plus aria-hidden="true" />
        </button>
        <button
          type="button"
          onClick={fitImage}
          disabled={zoom === 1 && pan.x === 0 && pan.y === 0}
          aria-label="Fit image"
          title="Fit image (0)"
        >
          <Maximize2 aria-hidden="true" />
        </button>
        <a
          href={source}
          download={name}
          aria-label={`Download ${name}`}
          title="Download image"
          onClick={(event) => event.stopPropagation()}
        >
          <Download aria-hidden="true" />
        </a>
        <button
          ref={closeRef}
          type="button"
          onClick={onClose}
          aria-label="Close image preview"
          title="Close (Esc)"
          autoFocus
        >
          <X aria-hidden="true" />
        </button>
      </div>
    </div>
  );
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
  animate,
  onOpenOutput,
  onOpenSource,
  availableOutputIds,
  availableSourceIds,
}: {
  item: ActionItem;
  animate: boolean;
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
    <details
      className={`action-cell ${animate ? "is-entering" : ""}`}
      open={isStreaming}
    >
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

function UserInputCard({
  item,
  onResolve,
}: {
  item: Extract<TranscriptItem, { kind: "user_input_request" }>;
  onResolve: ConversationProps["onResolveUserInput"];
}) {
  const [answer, setAnswer] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const resolve = async (
    value:
      | { type: "text"; text: string }
      | { type: "choice"; choice: string },
  ) => {
    if (item.resolved || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      await onResolve(item.requestId, value);
      setAnswer("");
    } catch (reason) {
      setError(
        reason instanceof Error
          ? reason.message
          : "ygg could not send this answer.",
      );
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <section className="user-input-card" aria-label="Input requested">
      <span className="user-input-icon">
        {item.resolved ? (
          <ShieldCheck aria-hidden="true" />
        ) : (
          <ShieldAlert aria-hidden="true" />
        )}
      </span>
      <div>
        <span className="user-input-eyebrow">
          {item.resolved ? "Answer sent" : "ygg needs private input"}
        </span>
        <h3>{item.prompt}</h3>
        {item.resolved ? (
          <p className="user-input-resolved">
            The answer was delivered directly to the running tool and was not
            added to the transcript.
          </p>
        ) : (
          <>
            {item.choices.length ? (
              <div className="user-input-choices">
                {item.choices.map((choice) => (
                  <button
                    key={choice}
                    type="button"
                    disabled={submitting}
                    onClick={() => void resolve({ type: "choice", choice })}
                  >
                    {choice}
                  </button>
                ))}
              </div>
            ) : (
              <form
                className="user-input-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  if (answer) void resolve({ type: "text", text: answer });
                }}
              >
                <label>
                  <span className="sr-only">Private answer</span>
                  <input
                    type="password"
                    value={answer}
                    onChange={(event) => setAnswer(event.target.value)}
                    placeholder="Enter private answer"
                    autoComplete="off"
                    disabled={submitting}
                  />
                </label>
                <button
                  className="primary-button"
                  type="submit"
                  disabled={!answer || submitting}
                >
                  Send securely
                </button>
              </form>
            )}
            <p className="user-input-privacy">
              This value goes only to the waiting tool. It is not stored in
              conversation history.
            </p>
          </>
        )}
        {error ? <p role="alert">{error}</p> : null}
      </div>
    </section>
  );
}

function AssistantMessage({
  item,
  animate,
}: {
  item: Extract<TranscriptItem, { kind: "assistant_message" }>;
  animate: boolean;
}) {
  const [copyState, setCopyState] = useState<
    "idle" | "copied" | "failed"
  >("idle");
  const resetTimerRef = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (resetTimerRef.current !== null) {
        window.clearTimeout(resetTimerRef.current);
      }
    },
    [],
  );

  const copyResponse = async () => {
    if (resetTimerRef.current !== null) {
      window.clearTimeout(resetTimerRef.current);
    }
    try {
      await navigator.clipboard.writeText(item.content);
      setCopyState("copied");
    } catch {
      setCopyState("failed");
    }
    resetTimerRef.current = window.setTimeout(() => {
      setCopyState("idle");
      resetTimerRef.current = null;
    }, 1_500);
  };

  return (
    <article
      className={`assistant-message ${item.state === "streaming" ? "is-streaming" : ""} ${animate ? "is-entering" : ""}`}
      aria-live={item.state === "streaming" ? "polite" : undefined}
    >
      {item.content ? (
        <Suspense fallback={<div className="message-copy">{item.content}</div>}>
          <MarkdownMessage content={item.content} />
        </Suspense>
      ) : (
        <div className="message-copy">
          <LoaderCircle
            className="spin assistant-waiting"
            aria-label="ygg is responding"
          />
        </div>
      )}
      {item.state !== "streaming" && item.content ? (
        <div className="message-actions">
          <button
            type="button"
            onClick={() => void copyResponse()}
            aria-label={
              copyState === "copied"
                ? "Response copied"
                : copyState === "failed"
                  ? "Copy failed"
                  : "Copy response"
            }
            title={copyState === "failed" ? "Copy failed" : "Copy response"}
          >
            {copyState === "copied" ? (
              <Check aria-hidden="true" />
            ) : (
              <Copy aria-hidden="true" />
            )}
          </button>
          <span className="sr-only" role="status" aria-live="polite">
            {copyState === "copied"
              ? "Response copied"
              : copyState === "failed"
                ? "Could not copy response"
                : ""}
          </span>
        </div>
      ) : null}
    </article>
  );
}

function TranscriptItemView({
  item,
  animate,
  onResolveApproval,
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  availableOutputIds,
  availableSourceIds,
  attachmentContentUrl,
  onPreviewAttachment,
}: {
  item: TranscriptItem;
  animate: boolean;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onResolveUserInput: ConversationProps["onResolveUserInput"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  availableOutputIds: ReadonlySet<string>;
  availableSourceIds: ReadonlySet<string>;
  attachmentContentUrl?: (handle: string) => string;
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
}) {
  switch (item.kind) {
    case "user_message": {
      return (
        <article className={`user-message ${animate ? "is-entering" : ""}`}>
          <div className="message-copy">{item.content}</div>
          {item.attachments?.length ? (
            <div
              className="message-attachments"
              role="list"
              aria-label={
                item.attachments.some((attachment) =>
                  attachment.mediaType.startsWith("image/"),
                )
                  ? "Attached images"
                  : "Attached files"
              }
            >
              {item.attachments.map((attachment, attachmentIndex) => {
                const numberedImage = attachment.mediaType.startsWith("image/")
                  ? item.attachments!
                      .slice(0, attachmentIndex + 1)
                      .filter((candidate) =>
                        candidate.mediaType.startsWith("image/"),
                      ).length
                  : 0;
                return (
                  <span role="listitem" key={attachment.id}>
                    {attachment.mediaType.startsWith("image/") &&
                    attachment.handle &&
                    attachmentContentUrl?.(attachment.handle) ? (
                      <button
                        className="message-image-attachment"
                        onClick={(event) =>
                          onPreviewAttachment(
                            attachmentContentUrl(attachment.handle!),
                            attachment.name,
                            event.currentTarget,
                          )
                        }
                        aria-label={`View attached image ${numberedImage}`}
                      >
                        <img
                          src={attachmentContentUrl(attachment.handle)}
                          alt={attachment.name}
                        />
                      </button>
                    ) : (
                      <span className="message-file-attachment">
                        <b>{extensionLabel(attachment.name)}</b>
                        <em>{attachment.name}</em>
                      </span>
                    )}
                  </span>
                );
              })}
            </div>
          ) : null}
        </article>
      );
    }

    case "assistant_message":
      return <AssistantMessage item={item} animate={animate} />;

    case "reasoning":
      return (
        <details
          className={`reasoning-block ${item.state === "streaming" ? "is-live" : ""} ${animate ? "is-entering" : ""}`}
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
          animate={animate}
          onOpenOutput={onOpenOutput}
          onOpenSource={onOpenSource}
          availableOutputIds={availableOutputIds}
          availableSourceIds={availableSourceIds}
        />
      );

    case "approval":
      return (
        <section
          className={`approval-card ${animate ? "is-entering" : ""}`}
          aria-label="Approval needed"
        >
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

    case "user_input_request":
      return (
        <UserInputCard
          item={item}
          onResolve={onResolveUserInput}
        />
      );

    case "run_outcome":
      return (
        <div
          className={`run-outcome is-${item.outcome} ${animate ? "is-entering" : ""}`}
        >
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
          {item.durationMs > 0 ? (
            <em>Worked for {formatDuration(item.durationMs)}</em>
          ) : null}
        </div>
      );
  }
}

type WorkItem = Extract<
  TranscriptItem,
  { kind: "action" | "reasoning" }
>;

type TranscriptRow =
  | { kind: "item"; item: TranscriptItem }
  | {
      kind: "work";
      id: string;
      items: WorkItem[];
      outcome?: Extract<TranscriptItem, { kind: "run_outcome" }>;
    };

function transcriptRows(items: TranscriptItem[]): TranscriptRow[] {
  const lastWorkIndexByTurn = new Map<string, number>();
  const outcomeByTurn = new Map<
    string,
    Extract<TranscriptItem, { kind: "run_outcome" }>
  >();
  items.forEach((item, index) => {
    if (item.kind === "action" || item.kind === "reasoning") {
      lastWorkIndexByTurn.set(item.turnId, index);
    } else if (item.kind === "run_outcome") {
      outcomeByTurn.set(item.turnId, item);
    }
  });

  const rows: TranscriptRow[] = [];
  let index = 0;
  while (index < items.length) {
    const item = items[index]!;
    if (item.kind === "action" || item.kind === "reasoning") {
      const workItems: WorkItem[] = [];
      const turnId = item.turnId;
      let lastIndex = index;
      while (lastIndex < items.length) {
        const candidate = items[lastIndex]!;
        if (
          candidate.turnId !== turnId ||
          (candidate.kind !== "action" && candidate.kind !== "reasoning")
        ) {
          break;
        }
        workItems.push(candidate);
        lastIndex += 1;
      }
      const ownsOutcome =
        lastWorkIndexByTurn.get(turnId) === lastIndex - 1;
      rows.push({
        kind: "work",
        id: `work-${workItems[0]!.id}`,
        items: workItems,
        outcome: ownsOutcome ? outcomeByTurn.get(turnId) : undefined,
      });
      index = lastIndex;
      continue;
    }
    if (
      item.kind === "run_outcome" &&
      lastWorkIndexByTurn.has(item.turnId)
    ) {
      index += 1;
      continue;
    }
    rows.push({ kind: "item", item });
    index += 1;
  }
  return rows;
}

interface WorkGroupProps {
  row: Extract<TranscriptRow, { kind: "work" }>;
  initialItemIds: ReadonlySet<string>;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onResolveUserInput: ConversationProps["onResolveUserInput"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  availableOutputIds: ReadonlySet<string>;
  availableSourceIds: ReadonlySet<string>;
  attachmentContentUrl?: (handle: string) => string;
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
}

function WorkGroup({
  row,
  initialItemIds,
  onResolveApproval,
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  availableOutputIds,
  availableSourceIds,
  attachmentContentUrl,
  onPreviewAttachment,
}: WorkGroupProps) {
  const live = row.items.some((item) => item.state === "streaming");
  const [userOpen, setUserOpen] = useState(false);
  const itemDuration = row.items.reduce(
    (total, item) =>
      total + (item.kind === "action" ? item.durationMs ?? 0 : 0),
    0,
  );
  const duration = row.outcome?.durationMs || itemDuration;
  const label = live
    ? "Working…"
    : duration > 0
      ? `Worked for ${formatDuration(duration)}`
      : "Work details";
  const open = live || userOpen;

  return (
    <section
      className={`work-group ${live ? "is-live" : "is-complete"} ${open ? "is-open" : "is-collapsed"}`}
      aria-label={label}
    >
      <button
        type="button"
        className="work-group-summary"
        aria-expanded={open}
        aria-disabled={live}
        onClick={() => {
          if (!live) setUserOpen((current) => !current);
        }}
      >
        <span className="work-group-glyph">
          <span className="work-group-status is-working">
            <LoaderCircle className="spin" aria-hidden="true" />
          </span>
          <span className="work-group-status is-finished">
            {row.outcome?.outcome === "failed" ? (
              <AlertTriangle aria-hidden="true" />
            ) : (
              <Check aria-hidden="true" />
            )}
          </span>
        </span>
        <span>{label}</span>
        <ChevronDown aria-hidden="true" />
      </button>
      <div className="work-group-content-clip" aria-hidden={!open}>
        <div className="work-group-content">
          {row.items.map((item) => (
            <TranscriptItemView
              key={item.id}
              item={item}
              animate={!initialItemIds.has(item.id)}
              onResolveApproval={onResolveApproval}
              onResolveUserInput={onResolveUserInput}
              onOpenOutput={onOpenOutput}
              onOpenSource={onOpenSource}
              availableOutputIds={availableOutputIds}
              availableSourceIds={availableSourceIds}
              attachmentContentUrl={attachmentContentUrl}
              onPreviewAttachment={onPreviewAttachment}
            />
          ))}
        </div>
      </div>
    </section>
  );
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
          ? "Describe a task, from a quick fix to a multi-step job. You can attach images."
          : "Describe a task, from a quick fix to a multi-step job."}
      </p>
    </div>
  );
}

function ReasoningPowerSlider({
  options,
  value,
  disabled,
  onChange,
}: {
  options: ReasoningEffort[];
  value: ReasoningEffort;
  disabled: boolean;
  onChange: (value: ReasoningEffort) => void;
}) {
  const selectedIndex = Math.max(0, options.indexOf(value));
  const lastIndex = Math.max(0, options.length - 1);
  const position = lastIndex ? (selectedIndex / lastIndex) * 100 : 0;
  const [burst, setBurst] = useState(0);
  const wheelDeltaRef = useRef(0);
  const isMax = selectedIndex === lastIndex && lastIndex > 0;
  const isFast =
    !isMax &&
    /fast|high|xhigh/i.test(options[selectedIndex] ?? "") &&
    selectedIndex > 0;

  const selectIndex = (index: number) => {
    const nextIndex = Math.max(0, Math.min(lastIndex, index));
    const next = options[nextIndex];
    if (!next || next === value || disabled) return;
    if (nextIndex === lastIndex) setBurst((current) => current + 1);
    onChange(next);
  };

  return (
    <div className="reasoning-power-control">
      <span className="reasoning-power-label" aria-hidden="true">
        {value}
      </span>
      <div className="power-slider-container">
        <div
          className="power-slider-root"
          data-max={isMax}
          data-fast={isFast}
          data-disabled={disabled || undefined}
          style={{ "--power-position": `${position}%` } as CSSProperties}
        >
          <div className="power-slider-track" aria-hidden="true">
            <span className="power-slider-range" />
            {isMax ? <span className="power-slider-max-fill" /> : null}
            {isFast ? (
              <span className="power-slider-fast-particles">
                {Array.from({ length: 14 }, (_, index) => (
                  <i key={index} />
                ))}
              </span>
            ) : null}
            <span className="power-slider-ticks">
              {options.map((option, index) => (
                <i
                  key={option}
                  data-selected={index <= selectedIndex}
                  style={
                    {
                      "--tick-position": `${lastIndex ? (index / lastIndex) * 100 : 50}%`,
                    } as CSSProperties
                  }
                />
              ))}
            </span>
          </div>
          <span className="power-slider-thumb-rail" aria-hidden="true">
            <span className="power-slider-thumb" />
            {burst ? (
              <span className="power-slider-burst" key={burst}>
                {Array.from({ length: 16 }, (_, index) => (
                  <i key={index} />
                ))}
              </span>
            ) : null}
          </span>
          <input
            className="power-slider-input"
            type="range"
            min={0}
            max={lastIndex}
            step={1}
            value={selectedIndex}
            disabled={disabled || lastIndex === 0}
            aria-label="Reasoning effort"
            aria-valuetext={value}
            onChange={(event) => selectIndex(Number(event.target.value))}
            onWheel={(event) => {
              if (disabled) return;
              wheelDeltaRef.current += event.deltaY;
              if (Math.abs(wheelDeltaRef.current) < 30) return;
              event.preventDefault();
              selectIndex(
                selectedIndex + (wheelDeltaRef.current > 0 ? -1 : 1),
              );
              wheelDeltaRef.current = 0;
            }}
          />
        </div>
      </div>
    </div>
  );
}

function providerLabel(provider: string): string {
  const leaf = provider.split("/").filter(Boolean).at(-1) ?? provider;
  return leaf
    .replaceAll("-", " ")
    .replace(/\b\w/g, (character) => character.toUpperCase());
}

function ModelPicker({
  models,
  value,
  disabled,
  hasStagedImages,
  onChange,
}: {
  models: ModelSummary[];
  value: string;
  disabled: boolean;
  hasStagedImages: boolean;
  onChange: (modelId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const triggerRef = useRef<HTMLButtonElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const activeModel =
    models.find((model) => model.id === value) ?? models.at(0);
  const normalizedQuery = query.trim().toLowerCase();
  const eligibleModels = models.filter(
    (model) =>
      model.available &&
      (!hasStagedImages || model.inputModalities.includes("image")),
  );
  const filteredModels = eligibleModels.filter((model) => {
    if (!normalizedQuery) return true;
    return [model.name, model.id, model.provider].some((candidate) =>
      candidate.toLowerCase().includes(normalizedQuery),
    );
  });
  const localModels = filteredModels.filter((model) => model.local);
  const remoteModels = filteredModels.filter((model) => !model.local);

  const close = (restoreFocus = true) => {
    setOpen(false);
    setQuery("");
    if (restoreFocus) {
      triggerRef.current?.focus();
    }
  };

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (
        !popoverRef.current?.contains(target) &&
        !triggerRef.current?.contains(target)
      ) {
        setOpen(false);
        setQuery("");
      }
    };
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setOpen(false);
      setQuery("");
      window.requestAnimationFrame(() => triggerRef.current?.focus());
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    window.requestAnimationFrame(() => searchRef.current?.focus());
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const focusOption = (from: HTMLElement, direction: 1 | -1) => {
    const options = Array.from(
      popoverRef.current?.querySelectorAll<HTMLButtonElement>(
        'button[role="option"]',
      ) ?? [],
    );
    if (!options.length) return;
    const currentIndex = options.indexOf(from as HTMLButtonElement);
    const nextIndex =
      currentIndex < 0
        ? direction > 0
          ? 0
          : options.length - 1
        : (currentIndex + direction + options.length) % options.length;
    options[nextIndex]?.focus();
  };

  const choose = (modelId: string) => {
    if (modelId !== value) onChange(modelId);
    close();
  };

  const renderGroup = (label: string, groupModels: ModelSummary[]) => {
    if (!groupModels.length) return null;
    return (
      <div className="model-picker-group" role="group" aria-label={label}>
        <span className="model-picker-group-label">{label}</span>
        {groupModels.map((model) => (
          <button
            key={model.id}
            type="button"
            role="option"
            aria-selected={model.id === value}
            className={model.id === value ? "is-selected" : undefined}
            onClick={() => choose(model.id)}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") {
                event.preventDefault();
                focusOption(event.currentTarget, 1);
              } else if (event.key === "ArrowUp") {
                event.preventDefault();
                focusOption(event.currentTarget, -1);
              } else if (event.key === "Home" || event.key === "End") {
                event.preventDefault();
                const options = Array.from(
                  popoverRef.current?.querySelectorAll<HTMLButtonElement>(
                    'button[role="option"]',
                  ) ?? [],
                );
                options[event.key === "Home" ? 0 : options.length - 1]?.focus();
              }
            }}
          >
            <span className="model-picker-option-mark" aria-hidden="true">
              {model.local ? <span className="local-model-dot" /> : null}
            </span>
            <span className="model-picker-option-copy">
              <strong>{model.name}</strong>
              <small>
                {providerLabel(model.provider)}
                {model.inputModalities.includes("image") ? " · Images" : ""}
              </small>
            </span>
            {model.id === value ? <Check aria-hidden="true" /> : null}
          </button>
        ))}
      </div>
    );
  };

  return (
    <div className="model-picker">
      <button
        ref={triggerRef}
        type="button"
        className="model-picker-trigger"
        aria-label="Model"
        aria-haspopup="listbox"
        aria-expanded={open}
        data-value={value}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        {activeModel?.local ? (
          <span className="local-model-dot" aria-hidden="true" />
        ) : null}
        <span>{activeModel?.name ?? value}</span>
        <ChevronDown aria-hidden="true" />
      </button>
      {open ? (
        <div
          ref={popoverRef}
          className="model-picker-popover"
          role="dialog"
          aria-label="Choose a model"
        >
          <div className="model-picker-heading">
            <strong>Choose a model</strong>
            <small>{eligibleModels.length} available</small>
          </div>
          <label className="model-picker-search">
            <Search aria-hidden="true" />
            <span className="sr-only">Search models</span>
            <input
              ref={searchRef}
              value={query}
              placeholder="Search models"
              aria-label="Search models"
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "ArrowDown") return;
                event.preventDefault();
                const firstOption =
                  popoverRef.current?.querySelector<HTMLButtonElement>(
                    'button[role="option"]',
                  );
                firstOption?.focus();
              }}
            />
          </label>
          <div
            className="model-picker-list"
            role="listbox"
            aria-label="Available models"
          >
            {renderGroup("On this device", localModels)}
            {renderGroup("Connected providers", remoteModels)}
            {!filteredModels.length ? (
              <p className="model-picker-empty">No matching models</p>
            ) : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function ComposerMenu<T extends string>({
  label,
  value,
  options,
  disabled,
  icon,
  className,
  onChange,
}: {
  label: string;
  value: T;
  options: Array<{ value: T; label: string; description?: string }>;
  disabled?: boolean;
  icon: ReactNode;
  className?: string;
  onChange: (value: T) => void;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const activeOption =
    options.find((option) => option.value === value) ?? options[0];

  const close = (restoreFocus: boolean) => {
    setOpen(false);
    if (restoreFocus) {
      window.requestAnimationFrame(() => triggerRef.current?.focus());
    }
  };

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (
        !popoverRef.current?.contains(target) &&
        !triggerRef.current?.contains(target)
      ) {
        setOpen(false);
      }
    };
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      close(true);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    window.requestAnimationFrame(() => {
      popoverRef.current
        ?.querySelector<HTMLButtonElement>('[aria-checked="true"]')
        ?.focus();
    });
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  useEffect(() => {
    if (!disabled || !open) return;
    const frame = window.requestAnimationFrame(() => setOpen(false));
    return () => window.cancelAnimationFrame(frame);
  }, [disabled, open]);

  const focusMenuItem = (
    current: HTMLButtonElement,
    direction: 1 | -1,
  ) => {
    const items = Array.from(
      popoverRef.current?.querySelectorAll<HTMLButtonElement>(
        '[role="menuitemradio"]',
      ) ?? [],
    );
    if (!items.length) return;
    const index = items.indexOf(current);
    items[(index + direction + items.length) % items.length]?.focus();
  };

  if (!activeOption) return null;

  return (
    <div className={`composer-menu ${className ?? ""}`}>
      <button
        ref={triggerRef}
        type="button"
        className="composer-menu-trigger"
        aria-label={`${label}: ${activeOption.label}`}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
        onKeyDown={(event) => {
          if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
          event.preventDefault();
          setOpen(true);
        }}
      >
        <span className="composer-menu-trigger-icon" aria-hidden="true">
          {icon}
        </span>
        <span className="composer-menu-trigger-label">
          {activeOption.label}
        </span>
        <ChevronDown aria-hidden="true" />
      </button>
      {open ? (
        <div
          ref={popoverRef}
          className="composer-menu-popover"
          role="menu"
          aria-label={label}
        >
          <span className="composer-menu-heading">{label}</span>
          {options.map((option) => (
            <button
              key={option.value}
              type="button"
              role="menuitemradio"
              aria-checked={option.value === value}
              onClick={() => {
                onChange(option.value);
                close(true);
              }}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                  event.preventDefault();
                  focusMenuItem(
                    event.currentTarget,
                    event.key === "ArrowDown" ? 1 : -1,
                  );
                } else if (event.key === "Home" || event.key === "End") {
                  event.preventDefault();
                  const items = Array.from(
                    popoverRef.current?.querySelectorAll<HTMLButtonElement>(
                      '[role="menuitemradio"]',
                    ) ?? [],
                  );
                  items[event.key === "Home" ? 0 : items.length - 1]?.focus();
                }
              }}
            >
              <span>
                <strong>{option.label}</strong>
                {option.description ? (
                  <small>{option.description}</small>
                ) : null}
              </span>
              {option.value === value ? <Check aria-hidden="true" /> : null}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function Composer({
  session,
  bootstrap,
  onSubmit,
  onInterrupt,
  onConfigure,
  onIngestAttachment,
  onPreviewAttachment,
}: Pick<
  ConversationProps,
  | "session"
  | "bootstrap"
  | "onSubmit"
  | "onInterrupt"
  | "onConfigure"
  | "onIngestAttachment"
> & {
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
}) {
  const [prompt, setPrompt] = useState("");
  const [attachments, setAttachments] = useState<DraftAttachment[]>([]);
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  const [draggingFiles, setDraggingFiles] = useState(false);
  const [activeDelivery, setActiveDelivery] = useState<
    "steer" | "followUp"
  >(bootstrap.capabilities.followUp ? "followUp" : "steer");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragDepthRef = useRef(0);
  const attachmentsRef = useRef<DraftAttachment[]>([]);
  const mountedRef = useRef(true);
  const isWorking =
    Boolean(session.activeRunId) ||
    session.status === "working" ||
    session.status === "needs_attention";
  const activeModel = bootstrap.models.find(
    (model) => model.id === session.modelId,
  );
  const providerKey =
    `${activeModel?.provider ?? ""} ${activeModel?.id ?? ""}`.toLowerCase();
  const modelAccent =
    providerAccents.find(([provider]) => providerKey.includes(provider))?.[1] ??
    "var(--theme-pigment)";
  const intensity = thinkingIntensity[session.reasoning] ?? 0.5;
  const reasoningOptions =
    bootstrap.models.find((model) => model.id === session.modelId)?.reasoning ??
    [session.reasoning];
  const composerStyle = {
    "--model-accent-light": balancedAccent(modelAccent, 0.11),
    "--model-accent-dark": balancedAccent(modelAccent, 0.27),
    "--thinking-intensity": intensity,
    "--thinking-opacity": 0.4 + intensity * 0.55,
  } as CSSProperties;
  const attachmentPolicy = bootstrap.capabilities.attachmentPolicy;
  const modelAcceptsAttachments =
    activeModel?.inputModalities.includes("image") ?? false;
  const attachmentsAvailable = Boolean(
    bootstrap.capabilities.attachments &&
      bootstrap.capabilities.attachmentIngest &&
      attachmentPolicy &&
      onIngestAttachment &&
      modelAcceptsAttachments,
  );
  const hasStagedImages = attachments.some((attachment) =>
    (attachment.file.type || "application/octet-stream").startsWith("image/"),
  );
  const attachmentsPending = attachments.some(
    (attachment) => attachment.status === "uploading",
  );
  const attachmentsFailed = attachments.some(
    (attachment) => attachment.status === "failed",
  );
  const uploadedAttachments = attachments.flatMap((attachment) =>
    attachment.reference ? [attachment.reference] : [],
  );
  const canSubmit =
    (Boolean(prompt.trim()) || uploadedAttachments.length > 0) &&
    !attachmentsPending &&
    !attachmentsFailed;

  useEffect(() => {
    if (!isWorking) {
      setActiveDelivery(
        bootstrap.capabilities.followUp ? "followUp" : "steer",
      );
    } else if (
      activeDelivery === "followUp" &&
      !bootstrap.capabilities.followUp &&
      bootstrap.capabilities.steer
    ) {
      setActiveDelivery("steer");
    } else if (
      activeDelivery === "steer" &&
      !bootstrap.capabilities.steer &&
      bootstrap.capabilities.followUp
    ) {
      setActiveDelivery("followUp");
    }
  }, [
    activeDelivery,
    bootstrap.capabilities.followUp,
    bootstrap.capabilities.steer,
    isWorking,
  ]);

  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      for (const attachment of attachmentsRef.current) {
        if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
      }
    };
  }, []);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "0";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }, [prompt]);

  const submit = async () => {
    if (!canSubmit || submitting) return;
    if (
      isWorking &&
      ((activeDelivery === "steer" && !bootstrap.capabilities.steer) ||
        (activeDelivery === "followUp" && !bootstrap.capabilities.followUp))
    ) {
      return;
    }
    const value = prompt;
    const submittedAttachments = attachments;
    const submittedReferences = uploadedAttachments;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await onSubmit(
        value,
        submittedReferences,
        isWorking ? activeDelivery : undefined,
      );
      setPrompt((current) => (current === value ? "" : current));
      const submittedIds = new Set(
        submittedAttachments.map((attachment) => attachment.localId),
      );
      for (const attachment of submittedAttachments) {
        if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
      }
      setAttachments((current) =>
        current.filter((attachment) => !submittedIds.has(attachment.localId)),
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

  const uploadAttachment = async (attachment: DraftAttachment) => {
    if (!onIngestAttachment) return;
    try {
      const reference = await onIngestAttachment(attachment.file);
      if (!mountedRef.current) return;
      setAttachments((current) =>
        current.map((candidate) =>
          candidate.localId === attachment.localId
            ? { ...candidate, status: "uploaded", reference, error: undefined }
            : candidate,
        ),
      );
    } catch (error) {
      if (!mountedRef.current) return;
      setAttachments((current) =>
        current.map((candidate) =>
          candidate.localId === attachment.localId
            ? {
                ...candidate,
                status: "failed",
                error:
                  error instanceof Error
                    ? error.message
                    : "Upload failed. Try again.",
              }
            : candidate,
        ),
      );
    }
  };

  const stageFiles = (files: File[]) => {
    if (!attachmentsAvailable || !attachmentPolicy || !files.length) return;
    setAttachmentError(null);
    const accepted: DraftAttachment[] = [];
    let totalBytes = attachments.reduce(
      (total, attachment) => total + attachment.file.size,
      0,
    );
    for (const file of files) {
      if (attachments.length + accepted.length >= attachmentPolicy.maxCount) {
        setAttachmentError(
          `You can attach up to ${attachmentPolicy.maxCount} files.`,
        );
        break;
      }
      if (file.size > attachmentPolicy.maxFileBytes) {
        setAttachmentError(`${file.name} is too large to attach.`);
        continue;
      }
      if (totalBytes + file.size > attachmentPolicy.maxTotalBytes) {
        setAttachmentError("These files exceed the total attachment limit.");
        break;
      }
      const mediaType = file.type || "application/octet-stream";
      const acceptedType =
        attachmentPolicy.acceptedMediaTypes.length === 0 ||
        attachmentPolicy.acceptedMediaTypes.some((pattern) =>
          pattern.endsWith("/*")
            ? mediaType.startsWith(pattern.slice(0, -1))
            : mediaType === pattern,
        );
      if (!acceptedType) {
        setAttachmentError(`${file.name} is not a supported file type.`);
        continue;
      }
      totalBytes += file.size;
      accepted.push({
        localId: crypto.randomUUID(),
        file,
        previewUrl:
          mediaType.startsWith("image/") && URL.createObjectURL
            ? URL.createObjectURL(file)
            : undefined,
        status: "uploading",
      });
    }
    if (!accepted.length) return;
    setAttachments((current) => [...current, ...accepted]);
    for (const attachment of accepted) void uploadAttachment(attachment);
  };

  const addAttachments = (event: ChangeEvent<HTMLInputElement>) => {
    stageFiles(Array.from(event.target.files ?? []));
    event.target.value = "";
  };

  const removeAttachment = (localId: string) => {
    setAttachments((current) => {
      const removed = current.find(
        (attachment) => attachment.localId === localId,
      );
      if (removed?.previewUrl) URL.revokeObjectURL(removed.previewUrl);
      return current.filter((attachment) => attachment.localId !== localId);
    });
  };

  return (
    <div
      className={`composer-wrap ${draggingFiles ? "is-dragging-files" : ""}`}
      onDragEnter={(event) => {
        if (
          !attachmentsAvailable ||
          !Array.from(event.dataTransfer.types).includes("Files")
        ) {
          return;
        }
        event.preventDefault();
        dragDepthRef.current += 1;
        setDraggingFiles(true);
      }}
      onDragOver={(event) => {
        if (!attachmentsAvailable) {
          event.dataTransfer.dropEffect = "none";
          return;
        }
        event.preventDefault();
        event.dataTransfer.dropEffect = "copy";
      }}
      onDragLeave={(event) => {
        if (!attachmentsAvailable) return;
        event.preventDefault();
        dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
        if (dragDepthRef.current === 0) setDraggingFiles(false);
      }}
      onDrop={(event) => {
        if (!attachmentsAvailable) return;
        event.preventDefault();
        dragDepthRef.current = 0;
        setDraggingFiles(false);
        stageFiles(Array.from(event.dataTransfer.files));
      }}
    >
      <div
        className={`composer ${isWorking ? "is-working" : ""}`}
        style={composerStyle}
      >
        {draggingFiles ? (
          <div className="attachment-drop-overlay" aria-hidden="true">
            <Paperclip />
            <strong>Drop files to attach</strong>
          </div>
        ) : null}
        {attachments.length ? (
          <div className="composer-attachments" aria-label="Attached files">
            {attachments.map((attachment) => (
              <div
                className={`composer-attachment is-${attachment.status}`}
                key={attachment.localId}
              >
                {attachment.previewUrl ? (
                  <button
                    className="composer-attachment-preview"
                    onClick={(event) =>
                      onPreviewAttachment(
                        attachment.previewUrl!,
                        attachment.file.name,
                        event.currentTarget,
                      )
                    }
                    aria-label={`Click to preview ${attachment.file.name}`}
                  >
                    <img src={attachment.previewUrl} alt="" />
                  </button>
                ) : (
                  <span className="attachment-extension" aria-hidden="true">
                    {extensionLabel(attachment.file.name)}
                  </span>
                )}
                <span className="composer-attachment-copy">
                  <strong>{attachment.file.name}</strong>
                  <small aria-live="polite" aria-atomic="true">
                    {attachment.status === "uploading"
                      ? "Uploading…"
                      : attachment.status === "failed"
                        ? attachment.error ?? "Upload failed"
                        : "Ready"}
                  </small>
                </span>
                {attachment.status === "failed" ? (
                  <button
                    className="attachment-retry"
                    onClick={() => {
                      setAttachments((current) =>
                        current.map((candidate) =>
                          candidate.localId === attachment.localId
                            ? {
                                ...candidate,
                                status: "uploading",
                                error: undefined,
                              }
                            : candidate,
                        ),
                      );
                      void uploadAttachment(attachment);
                    }}
                    aria-label={`Retry ${attachment.file.name}`}
                  >
                    <RefreshCw aria-hidden="true" />
                  </button>
                ) : null}
                <button
                  className="attachment-remove"
                  onClick={() => removeAttachment(attachment.localId)}
                  aria-label={`Remove ${attachment.file.name}`}
                >
                  <X aria-hidden="true" />
                </button>
              </div>
            ))}
          </div>
        ) : null}
        <textarea
          ref={textareaRef}
          value={prompt}
          onChange={(event) => setPrompt(event.target.value)}
          onKeyDown={onKeyDown}
          onPaste={(event) => {
            if (!attachmentsAvailable) return;
            const files = Array.from(event.clipboardData.files);
            if (!files.length) return;
            event.preventDefault();
            stageFiles(files);
          }}
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
            {attachmentsAvailable ? (
              <>
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  hidden
                  accept={attachmentPolicy?.acceptedMediaTypes.join(",")}
                  onChange={addAttachments}
                />
                <button
                  className="composer-icon-button"
                  onClick={() => fileInputRef.current?.click()}
                  aria-label="Add files or photos"
                  title="Add files or photos"
                >
                  <Paperclip aria-hidden="true" />
                </button>
              </>
            ) : null}
            <ModelPicker
              models={bootstrap.models}
              value={session.modelId}
              disabled={isWorking}
              hasStagedImages={hasStagedImages}
              onChange={(modelId) => void onConfigure({ modelId })}
            />
            <ReasoningPowerSlider
              options={reasoningOptions}
              value={session.reasoning}
              disabled={isWorking}
              onChange={(reasoning) => void onConfigure({ reasoning })}
            />
            <ComposerMenu
              label="Authority"
              className="authority-menu"
              value={session.authority}
              disabled={isWorking}
              icon={<ShieldCheck />}
              options={bootstrap.authorityProfiles.map((authority) => ({
                value: authority,
                label: authorityLabels[authority],
                description:
                  authority === "readOnly"
                    ? "Inspect without changing files"
                    : authority === "workspace"
                      ? "Work inside the selected project"
                      : "Use tools across this device",
              }))}
              onChange={(authority) => void onConfigure({ authority })}
            />
            {isWorking ? (
              <ComposerMenu
                label="While ygg is working"
                className="delivery-menu"
                value={activeDelivery}
                icon={
                  activeDelivery === "steer" ? (
                    <BrainCircuit />
                  ) : (
                    <ArrowDown />
                  )
                }
                options={[
                  ...(bootstrap.capabilities.followUp
                    ? [
                        {
                          value: "followUp" as const,
                          label: "Follow up",
                          description: "Queue after the current response",
                        },
                      ]
                    : []),
                  ...(bootstrap.capabilities.steer
                    ? [
                        {
                          value: "steer" as const,
                          label: "Steer now",
                          description: "Guide the response in progress",
                        },
                      ]
                    : []),
                ]}
                onChange={setActiveDelivery}
              />
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
              disabled={submitting || !canSubmit}
              aria-disabled={submitting || !canSubmit}
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
        {attachmentError ? (
          <p className="composer-error" role="alert">
            {attachmentError}
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
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  onIngestAttachment,
  attachmentContentUrl,
}: ConversationProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [showJump, setShowJump] = useState(false);
  const [attachmentPreview, setAttachmentPreview] = useState<{
    source: string;
    name: string;
    trigger: HTMLElement;
  } | null>(null);
  const [initialItemIds] = useState(
    () => new Set(session.items.map((item) => item.id)),
  );
  const shouldStickRef = useRef(true);
  const resourcesAvailable = bootstrap.capabilities.resources;
  const availableOutputIds = new Set(
    resourcesAvailable ? session.outputs.map((output) => output.id) : [],
  );
  const availableSourceIds = new Set(
    resourcesAvailable ? session.sources.map((source) => source.id) : [],
  );
  const rows = transcriptRows(session.items);

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

  const closeAttachmentPreview = () => {
    const trigger = attachmentPreview?.trigger;
    setAttachmentPreview(null);
    window.requestAnimationFrame(() => {
      if (trigger?.isConnected) trigger.focus();
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
                bootstrap.capabilities.attachmentIngest &&
                Boolean(bootstrap.capabilities.attachmentPolicy) &&
                Boolean(
                  bootstrap.models
                    .find((model) => model.id === session.modelId)
                    ?.inputModalities.includes("image"),
                )
              }
            />
          ) : (
            rows.map((row) =>
              row.kind === "work" ? (
                <WorkGroup
                  key={row.id}
                  row={row}
                  initialItemIds={initialItemIds}
                  onResolveApproval={onResolveApproval}
                  onResolveUserInput={onResolveUserInput}
                  onOpenOutput={onOpenOutput}
                  onOpenSource={onOpenSource}
                  availableOutputIds={availableOutputIds}
                  availableSourceIds={availableSourceIds}
                  attachmentContentUrl={attachmentContentUrl}
                  onPreviewAttachment={(source, name, trigger) =>
                    setAttachmentPreview({ source, name, trigger })
                  }
                />
              ) : (
                <TranscriptItemView
                  key={row.item.id}
                  item={row.item}
                  animate={!initialItemIds.has(row.item.id)}
                  onResolveApproval={onResolveApproval}
                  onResolveUserInput={onResolveUserInput}
                  onOpenOutput={onOpenOutput}
                  onOpenSource={onOpenSource}
                  availableOutputIds={availableOutputIds}
                  availableSourceIds={availableSourceIds}
                  attachmentContentUrl={attachmentContentUrl}
                  onPreviewAttachment={(source, name, trigger) =>
                    setAttachmentPreview({ source, name, trigger })
                  }
                />
              ),
            )
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
        key={session.sessionId}
        session={session}
        bootstrap={bootstrap}
        onSubmit={onSubmit}
        onInterrupt={onInterrupt}
        onConfigure={onConfigure}
        onIngestAttachment={onIngestAttachment}
        onPreviewAttachment={(source, name, trigger) =>
          setAttachmentPreview({ source, name, trigger })
        }
      />
      {attachmentPreview ? (
        <AttachmentPreviewDialog
          key={attachmentPreview.source}
          source={attachmentPreview.source}
          name={attachmentPreview.name}
          onClose={closeAttachmentPreview}
        />
      ) : null}
    </section>
  );
}
