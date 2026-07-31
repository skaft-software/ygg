import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BrainCircuit,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Copy,
  Download,
  ExternalLink,
  File,
  FileArchive,
  FileCode2,
  FileDiff,
  FileText,
  GitFork,
  Globe2,
  LoaderCircle,
  Maximize2,
  Minus,
  Paperclip,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  ScanSearch,
  ShieldAlert,
  ShieldCheck,
  TerminalSquare,
  X,
  Zap,
} from "lucide-react";
import {
  Suspense,
  type ComponentProps,
  type CSSProperties,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  lazy,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { CommandRejectedError } from "../command-error";
import { SessionDraftStore } from "../drafts";
import type {
  ActionItem,
  AttachmentRef,
  AuthorityProfile,
  DocumentReference,
  CommandDiscovery,
  CommandSuggestion,
  HostBootstrap,
  GoalState,
  ModelSummary,
  OutputRef,
  ReasoningEffort,
  SessionSnapshot,
  SourceRef,
  TranscriptItem,
  TrustedFileCatalog,
  TrustedFileEntry,
  TrustedFileRead,
  TrustedFileSearchResult,
} from "../protocol";
import { GoalBadge } from "./GoalBadge";
import {
  ConversationBranchDialog,
  type ConversationBranchAction,
} from "./ConversationBranchDialog";
import { PromptContextPicker } from "./PromptContextPicker";
import {
  ComposerCompletion,
  type ComposerCompletionOption,
} from "./ComposerCompletion";
import {
  BashLogo,
  InlineDiffPreview,
  InlineFilePreview,
  ShellCommand,
  type ResourceContentUrl,
} from "./ActivityPreview";
import {
  parseGoalCommand,
  type GoalCommand,
} from "./ComposerCommands/goal";

const MarkdownMessage = lazy(() => import("./MarkdownMessage"));

interface ConversationProps {
  session: SessionSnapshot;
  bootstrap: HostBootstrap;
  goal?: GoalState | null;
  onGoalCommand?: (command: GoalCommand) => Promise<string>;
  onSubmit: (
    prompt: string,
    attachments: AttachmentRef[],
    activeDelivery?: "steer" | "followUp",
    idempotencyKey?: string,
    documents?: DocumentReference[],
    projectFiles?: TrustedFileEntry[],
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
  onOpenResource?: (
    handle: string,
    title: string,
    presentation: "text" | "diff" | "image",
  ) => void;
  resourceContentUrl?: ResourceContentUrl;
  onIngestAttachment?: (file: File) => Promise<AttachmentRef>;
  onIngestDocument?: (file: File) => Promise<DocumentReference>;
  onListProjectFiles?: () => Promise<TrustedFileCatalog>;
  onSearchProjectFiles?: (
    query: string,
  ) => Promise<TrustedFileSearchResult>;
  onReadProjectFile?: (entryId: string) => Promise<TrustedFileRead>;
  onGetCommandDiscovery?: () => Promise<CommandDiscovery>;
  onInvokeSlashCommand?: (
    invocation: string,
    idempotencyKey: string,
  ) => Promise<void>;
  onExportSession?: () => Promise<void> | void;
  onForkSession?: () => Promise<void>;
  onOpenRuntimeStatus?: () => void;
  onEditUserTurn?: (entryId: string, text: string) => Promise<void>;
  onRetryResponse?: (
    entryId: string,
    model?: { id: string; reasoning: ReasoningEffort },
  ) => Promise<void>;
  onForkConversation?: (entryId: string) => Promise<void>;
  attachmentContentUrl?: (handle: string) => string;
}

type DraftAttachment = {
  localId: string;
  file?: File;
  previewUrl?: string;
  status: "uploading" | "uploaded" | "failed";
  reference?: AttachmentRef;
  error?: string;
};

type SubmissionFailure = {
  kind: "connection" | "provider" | "session" | "rejected";
  title: string;
  message: string;
  retryable: boolean;
};

function classifySubmissionFailure(error: unknown): SubmissionFailure {
  const message =
    error instanceof Error
      ? error.message
      : "ygg could not send this message.";
  if (error instanceof CommandRejectedError) {
    if (
      error.code === "staleGeneration" ||
      error.code === "replayGap" ||
      error.code === "locked" ||
      error.code === "invalidBoundary" ||
      error.code === "alreadyResolved"
    ) {
      return {
        kind: "session",
        title: "Session state changed",
        message,
        retryable: error.retryable,
      };
    }
    if (error.code === "unavailable" || error.code === "internal") {
      return {
        kind: "provider",
        title: "Provider or host unavailable",
        message,
        retryable: error.retryable,
      };
    }
    if (error.code === "incompatibleProtocol") {
      return {
        kind: "connection",
        title: "Host protocol changed",
        message,
        retryable: error.retryable,
      };
    }
    return {
      kind: "rejected",
      title: "Message was not accepted",
      message,
      retryable: error.retryable,
    };
  }
  const normalized = message.toLocaleLowerCase();
  if (
    normalized.includes("fetch") ||
    normalized.includes("network") ||
    normalized.includes("connect") ||
    normalized.includes("unavailable")
  ) {
    return {
      kind: "connection",
      title: "Connection interrupted",
      message,
      retryable: true,
    };
  }
  if (
    normalized.includes("provider") ||
    normalized.includes("model") ||
    normalized.includes("rate limit")
  ) {
    return {
      kind: "provider",
      title: "Provider did not accept the request",
      message,
      retryable: true,
    };
  }
  if (
    normalized.includes("session") ||
    normalized.includes("generation") ||
    normalized.includes("working") ||
    normalized.includes("stale")
  ) {
    return {
      kind: "session",
      title: "Session state changed",
      message,
      retryable: true,
    };
  }
  return {
    kind: "rejected",
    title: "Message was not accepted",
    message,
    retryable: false,
  };
}

function draftAttachmentName(attachment: DraftAttachment): string {
  return attachment.file?.name ?? attachment.reference?.name ?? "Attachment";
}

function draftAttachmentMediaType(attachment: DraftAttachment): string {
  return (
    attachment.file?.type ??
    attachment.reference?.mediaType ??
    "application/octet-stream"
  );
}

function draftAttachmentSize(attachment: DraftAttachment): number {
  return attachment.file?.size ?? attachment.reference?.size ?? 0;
}

function browserDraftStore(): SessionDraftStore {
  try {
    return new SessionDraftStore(
      typeof window === "undefined" ? undefined : window.localStorage,
    );
  } catch {
    return new SessionDraftStore();
  }
}

function extensionLabel(name: string): string {
  const extension = name.split(".").at(-1);
  return extension && extension !== name
    ? extension.slice(0, 4).toUpperCase()
    : "FILE";
}

const ARCHIVE_FILE_EXTENSIONS = new Set([
  "7z",
  "bz2",
  "gz",
  "rar",
  "tar",
  "tgz",
  "xz",
  "zip",
]);
const CODE_FILE_EXTENSIONS = new Set([
  "c",
  "cc",
  "cpp",
  "cs",
  "css",
  "go",
  "h",
  "html",
  "java",
  "js",
  "json",
  "jsx",
  "kt",
  "kts",
  "php",
  "py",
  "rb",
  "rs",
  "scss",
  "sh",
  "sql",
  "swift",
  "toml",
  "ts",
  "tsx",
  "xml",
  "yaml",
  "yml",
  "zsh",
]);
const CODE_MEDIA_TYPES = new Set([
  "application/javascript",
  "application/json",
  "application/sql",
  "application/typescript",
  "application/xml",
  "application/x-sh",
  "text/css",
  "text/html",
  "text/javascript",
  "text/typescript",
]);

type AttachmentFileKind = "archive" | "code" | "pdf" | "generic";

function attachmentExtension(name: string): string {
  const extension = name.split(".").at(-1);
  return extension && extension !== name ? extension.toLowerCase() : "";
}

function attachmentFileKind(attachment: AttachmentRef): AttachmentFileKind {
  const mediaType = attachment.mediaType
    .split(";", 1)[0]
    ?.trim()
    .toLowerCase();
  const extension = attachmentExtension(attachment.name);
  if (mediaType === "application/pdf" || extension === "pdf") return "pdf";
  if (
    ARCHIVE_FILE_EXTENSIONS.has(extension) ||
    mediaType?.includes("zip") ||
    mediaType === "application/gzip" ||
    mediaType === "application/x-tar"
  ) {
    return "archive";
  }
  if (
    CODE_FILE_EXTENSIONS.has(extension) ||
    CODE_MEDIA_TYPES.has(mediaType ?? "")
  ) {
    return "code";
  }
  return "generic";
}

function TranscriptFileIcon({ attachment }: { attachment: AttachmentRef }) {
  const kind = attachmentFileKind(attachment);
  const className = `message-file-attachment-icon is-${kind}`;
  switch (kind) {
    case "pdf":
      return <FileText aria-hidden="true" className={className} />;
    case "archive":
      return <FileArchive aria-hidden="true" className={className} />;
    case "code":
      return <FileCode2 aria-hidden="true" className={className} />;
    case "generic":
      return <File aria-hidden="true" className={className} />;
  }
}

function attachmentSizeLabel(size: number): string {
  if (size < 1024) return `${size} B`;
  const units = ["B", "KB", "MB", "GB"];
  const unit = Math.min(
    Math.floor(Math.log(size) / Math.log(1024)),
    units.length - 1,
  );
  const value = Math.round((size / 1024 ** unit) * 10) / 10;
  return `${value} ${units[unit]}`;
}

function attachmentMetadataLabel(attachment: AttachmentRef): string {
  return `Media type: ${attachment.mediaType}\nSource: ${
    attachment.handle ? "uploaded attachment" : "transcript record"
  }`;
}

function documentTypeLabel(document: DocumentReference): string {
  if (document.mediaType === "application/pdf") {
    if (document.pageCount === undefined) return "PDF";
    return `PDF · ${document.pageCount} ${
      document.pageCount === 1 ? "page" : "pages"
    }`;
  }
  if (document.mediaType === "text/markdown") return "Markdown";
  return "Text";
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
  file_search: <Search aria-hidden="true" />,
  file_write: <FileDiff aria-hidden="true" />,
  web_search: <Search aria-hidden="true" />,
  skill: <BrainCircuit aria-hidden="true" />,
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

function cleanReasoningText(value: string): string {
  return value
    .replace(/\*\*([^*]+)\*\*/g, "$1. ")
    .replace(/__([^_]+)__/g, "$1. ")
    .replace(/[`*_#>~]/g, "")
    .replace(/([.!?])(?=[A-Z])/g, "$1 ")
    .replace(/\s+/g, " ")
    .replace(/\s+([,.;:!?])/g, "$1")
    .replace(/([.!?])(?:\s*[.!?])+/g, "$1")
    .trim();
}

function reasoningDisplayLabel(
  item: Extract<TranscriptItem, { kind: "reasoning" }>,
): string {
  const summary = cleanReasoningText(item.summary);
  if (
    summary &&
    !/^(?:thinking|reasoning|working|executing|analy[sz](?:e|ing)?|processing)[.!…]*$/i.test(
      summary,
    )
  ) {
    return summary;
  }
  const emphasized = Array.from(
    item.content.matchAll(/\*\*([^*]+)\*\*/g),
    (match) => cleanReasoningText(match[1] ?? ""),
  ).filter(Boolean);
  const fallback = emphasized.at(-1) ?? cleanReasoningText(item.content);
  if (!fallback) return summary || "Working";
  return fallback.length > 92
    ? `${fallback.slice(0, 89).trimEnd()}…`
    : fallback;
}

function LiveDots({ animated = true }: { animated?: boolean }) {
  return (
    <span className={`live-dots ${animated ? "" : "is-static"}`} aria-hidden="true">
      <i />
    </span>
  );
}

function useExitPresence(open: boolean, durationMs = 170) {
  const [retain, setRetain] = useState(open);

  useEffect(() => {
    if (open) {
      if (retain) return;
      const frame = window.requestAnimationFrame(() => setRetain(true));
      return () => window.cancelAnimationFrame(frame);
    }
    if (!retain) return;
    const timer = window.setTimeout(() => setRetain(false), durationMs);
    return () => window.clearTimeout(timer);
  }, [durationMs, open, retain]);

  return { present: open || retain, closing: !open && retain };
}

function actionFileTitle(
  item: ActionItem,
  sources: readonly SourceRef[],
): string {
  if (item.actionKind === "file_read" && sources.length === 1) {
    return sources[0]!.title;
  }
  if (item.actionKind === "file_write" && item.changedPaths.length === 1) {
    return item.changedPaths[0]!;
  }
  return item.target ?? item.label;
}

function ActionCell({
  item,
  animate,
  durationOverrideMs,
  groupExpanded = false,
  sessionId,
  resourceContentUrl,
  onOpenOutput,
  onOpenSource,
  onOpenResource,
  availableOutputs,
  availableSources,
  outputsByOrigin,
  sourcesByOrigin,
}: {
  item: ActionItem;
  animate: boolean;
  durationOverrideMs?: number;
  groupExpanded?: boolean;
  sessionId: string;
  resourceContentUrl?: ResourceContentUrl;
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  onOpenResource: ConversationProps["onOpenResource"];
  availableOutputs: ReadonlyMap<string, OutputRef>;
  availableSources: ReadonlyMap<string, SourceRef>;
  outputsByOrigin: ReadonlyMap<string, readonly OutputRef[]>;
  sourcesByOrigin: ReadonlyMap<string, readonly SourceRef[]>;
}) {
  const isStreaming = item.state === "streaming";
  const [expanded, setExpanded] = useState(false);
  const sourceIds = new Set([
    ...(item.sourceIds ?? []),
    ...(sourcesByOrigin.get(item.id) ?? []).map((source) => source.id),
  ]);
  const outputIds = new Set([
    ...(item.outputIds ?? []),
    ...(outputsByOrigin.get(item.id) ?? []).map((output) => output.id),
  ]);
  const sources = Array.from(sourceIds)
    .map((id) => availableSources.get(id))
    .filter((source): source is SourceRef => Boolean(source));
  const outputs = Array.from(outputIds)
    .map((id) => availableOutputs.get(id))
    .filter((output): output is OutputRef => Boolean(output));
  const durationMs = item.durationMs ?? durationOverrideMs;
  const isCommand = item.actionKind === "command";
  const fileTitle = actionFileTitle(item, sources);
  const command = item.commandPreview ?? item.target ?? item.label;
  const inlinePreview =
    item.actionKind === "file_write" && item.diffHandle ? (
      <InlineDiffPreview
        title={fileTitle}
        url={
          groupExpanded || expanded
            ? resourceContentUrl
              ? resourceContentUrl(sessionId, item.diffHandle)
              : undefined
            : undefined
        }
        enabled={groupExpanded || expanded}
        sessionId={sessionId}
        handle={item.diffHandle}
      />
    ) : item.actionKind === "file_read" && sources.length ? (
      <div className="activity-file-previews">
        {sources.map((source) => (
          <InlineFilePreview
            key={source.id}
            source={source}
            url={
              groupExpanded || expanded
                ? resourceContentUrl && source.handle
                  ? resourceContentUrl(sessionId, source.handle)
                  : undefined
                : undefined
            }
            enabled={groupExpanded || expanded}
            sessionId={sessionId}
          />
        ))}
      </div>
    ) : null;
  const secondaryLabel =
    !isCommand && fileTitle !== item.label ? item.label : undefined;

  return (
    <>
      <details
        className={`action-cell ${animate ? "is-entering" : ""}`}
        data-status={item.status}
        onToggle={(event) => setExpanded(event.currentTarget.open)}
      >
        <summary>
          <span
            className={`action-glyph ${isStreaming ? "is-live" : ""} ${isCommand ? "is-bash" : ""}`}
          >
            {isStreaming ? <LiveDots animated={false} /> : isCommand ? <BashLogo /> : actionIcons[item.actionKind]}
          </span>
          <span className={`action-title ${isCommand ? "is-command" : ""}`}>
            {isCommand ? (
              <ShellCommand command={command} />
            ) : (
              <>
                <strong>{fileTitle}</strong>
                {item.target && item.target !== fileTitle ? <code>{item.target}</code> : null}
                {secondaryLabel ? <small>{secondaryLabel}</small> : null}
              </>
            )}
          </span>
          {typeof item.additions === "number" ? (
            <span className="diff-count">
              <em>+{item.additions}</em>
              <b>−{item.deletions ?? 0}</b>
            </span>
          ) : null}
          {durationMs ? <span className="action-duration">{formatDuration(durationMs)}</span> : null}
          {item.status === "failed" ? (
            <AlertTriangle className="action-status-icon" aria-label="Action failed" />
          ) : null}
          <ChevronDown className="disclosure-chevron" aria-hidden="true" />
        </summary>
        <div className="action-detail">
          {item.outputSummary ?? item.summary ?? item.detail ? (
            <p>{item.outputSummary ?? item.summary ?? item.detail}</p>
          ) : null}
          {item.commandPreview ? (
            <pre className="action-command">
              <ShellCommand command={item.commandPreview} />
            </pre>
          ) : null}
          {!groupExpanded && expanded ? inlinePreview : null}
          <dl className="action-metadata">
            <div>
              <dt>Status</dt>
              <dd>{item.status}</dd>
            </div>
            {item.cwd ? (
              <div>
                <dt>Working directory</dt>
                <dd>
                  <code>{item.cwd}</code>
                </dd>
              </div>
            ) : null}
            {typeof item.exitCode === "number" ? (
              <div>
                <dt>Exit</dt>
                <dd>{item.exitCode}</dd>
              </div>
            ) : null}
            {typeof item.signal === "number" ? (
              <div>
                <dt>Signal</dt>
                <dd>{item.signal}</dd>
              </div>
            ) : null}
            {item.observedOutputBytes > 0 ? (
              <div>
                <dt>Observed output</dt>
                <dd>{item.observedOutputBytes.toLocaleString()} bytes</dd>
              </div>
            ) : null}
            {item.droppedOutputBytes > 0 ? (
              <div data-warning="true">
                <dt>Truncated</dt>
                <dd>{item.droppedOutputBytes.toLocaleString()} bytes omitted</dd>
              </div>
            ) : null}
          </dl>
          {item.outputHandle && onOpenResource ? (
            <div className="action-links">
              <button
                onClick={() =>
                  onOpenResource(item.outputHandle!, `${item.label} output`, "text")
                }
                aria-label={`Open full output for ${item.label}`}
              >
                <TerminalSquare aria-hidden="true" />
                Open full output
              </button>
            </div>
          ) : null}
          {(item.diffHandle || item.resultHandle) && onOpenResource ? (
            <div className="action-links">
              {item.diffHandle ? (
                <button
                  onClick={() =>
                    onOpenResource(item.diffHandle!, `${fileTitle} changes`, "diff")
                  }
                  aria-label={`View changes to ${fileTitle}`}
                >
                  <FileDiff aria-hidden="true" />
                  View full diff
                </button>
              ) : null}
              {item.resultHandle ? (
                <button
                  onClick={() =>
                    onOpenResource(item.resultHandle!, fileTitle, "text")
                  }
                  aria-label={`View resulting ${fileTitle}`}
                >
                  <FileText aria-hidden="true" />
                  View file
                </button>
              ) : null}
            </div>
          ) : null}
          {sources.length ? (
            <div className="action-links">
              {sources.map((source) => (
                <button
                  key={source.id}
                  onClick={() => onOpenSource(source.id)}
                  aria-label={`Open source ${source.title}`}
                >
                  <File aria-hidden="true" />
                  {source.title}
                </button>
              ))}
            </div>
          ) : null}
          {outputs.length ? (
            <div className="action-links">
              {outputs.map((output) => (
                <button
                  key={output.id}
                  onClick={() => onOpenOutput(output.id)}
                  aria-label={`Open output ${output.title}`}
                >
                  <ExternalLink aria-hidden="true" />
                  {output.title}
                </button>
              ))}
            </div>
          ) : null}
        </div>
      </details>
      {groupExpanded ? inlinePreview : null}
    </>
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
  branchingAvailable,
  onRetry,
  onRetryWithModel,
  onFork,
}: {
  item: Extract<TranscriptItem, { kind: "assistant_message" }>;
  animate: boolean;
  branchingAvailable: boolean;
  onRetry: () => void;
  onRetryWithModel: () => void;
  onFork: () => void;
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
      {item.content && item.state === "streaming" ? (
        <div className="message-copy">{item.content}</div>
      ) : item.content ? (
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
          {branchingAvailable ? (
            <>
              <button
                type="button"
                onClick={onRetry}
                aria-label="Retry response"
                title="Retry response"
              >
                <RefreshCw aria-hidden="true" />
              </button>
              <button
                type="button"
                onClick={onRetryWithModel}
                aria-label="Retry response with another model"
                title="Retry with another model"
              >
                <Zap aria-hidden="true" />
              </button>
              <button
                type="button"
                onClick={onFork}
                aria-label="Fork conversation here"
                title="Fork into a new session"
              >
                <GitFork aria-hidden="true" />
              </button>
            </>
          ) : null}
        </div>
      ) : null}
    </article>
  );
}

const TranscriptItemView = memo(function TranscriptItemView({
  item,
  animate,
  actionDurationOverrideMs,
  onResolveApproval,
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  onOpenResource,
  availableOutputs,
  availableSources,
  outputsByOrigin,
  sourcesByOrigin,
  sessionId = "",
  resourceContentUrl,
  groupExpanded = false,
  attachmentContentUrl,
  onPreviewAttachment,
  conversationBranching = false,
  onEditUserTurn = () => {},
  onRetryResponse = () => {},
  onRetryResponseWithModel = () => {},
  onForkConversation = () => {},
}: {
  item: TranscriptItem;
  animate: boolean;
  actionDurationOverrideMs?: number;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onResolveUserInput: ConversationProps["onResolveUserInput"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  onOpenResource: ConversationProps["onOpenResource"];
  availableOutputs: ReadonlyMap<string, OutputRef>;
  availableSources: ReadonlyMap<string, SourceRef>;
  outputsByOrigin: ReadonlyMap<string, readonly OutputRef[]>;
  sourcesByOrigin: ReadonlyMap<string, readonly SourceRef[]>;
  sessionId?: string;
  resourceContentUrl?: ResourceContentUrl;
  groupExpanded?: boolean;
  attachmentContentUrl?: (handle: string) => string;
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
  conversationBranching?: boolean;
  onEditUserTurn?: (
    item: Extract<TranscriptItem, { kind: "user_message" }>,
  ) => void;
  onRetryResponse?: (
    item: Extract<TranscriptItem, { kind: "assistant_message" }>,
  ) => void;
  onRetryResponseWithModel?: (
    item: Extract<TranscriptItem, { kind: "assistant_message" }>,
  ) => void;
  onForkConversation?: (item: TranscriptItem) => void;
}) {
  switch (item.kind) {
    case "user_message": {
      return (
        <article className={`user-message ${animate ? "is-entering" : ""}`}>
          <div className="message-copy">{item.content}</div>
          {item.branchProvenance ? (
            <div className="conversation-branch-provenance">
              <GitFork aria-hidden="true" />
              <span>
                <strong>
                  {item.branchProvenance.operation === "editUserTurn"
                    ? "Edited-turn branch"
                    : item.branchProvenance.operation === "retryResponse"
                      ? "Retried-response branch"
                      : "Forked conversation"}
                </strong>
                <small>{item.branchProvenance.warning}</small>
              </span>
            </div>
          ) : null}
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
                const isImage = attachment.mediaType.startsWith("image/");
                const numberedImage = isImage
                  ? item.attachments!
                      .slice(0, attachmentIndex + 1)
                      .filter((candidate) =>
                        candidate.mediaType.startsWith("image/"),
                      ).length
                  : 0;
                const imageUrl =
                  isImage && attachment.handle
                    ? attachmentContentUrl?.(attachment.handle)
                    : undefined;
                const fileAttachment = (
                  <>
                    <TranscriptFileIcon attachment={attachment} />
                    <span className="message-file-attachment-copy">
                      <em>{attachment.name}</em>
                      <small>{attachmentSizeLabel(attachment.size)}</small>
                    </span>
                  </>
                );
                return (
                  <span role="listitem" key={attachment.id}>
                    {imageUrl ? (
                      <button
                        className="message-image-attachment"
                        onClick={(event) =>
                          onPreviewAttachment(
                            imageUrl,
                            attachment.name,
                            event.currentTarget,
                          )
                        }
                        aria-label={`View attached image ${numberedImage}`}
                      >
                        <img src={imageUrl} alt={attachment.name} />
                      </button>
                    ) : (
                      <span
                        className="message-file-attachment"
                        title={attachmentMetadataLabel(attachment)}
                      >
                        {fileAttachment}
                      </span>
                    )}
                  </span>
                );
              })}
            </div>
          ) : null}
          {item.documents?.length || item.projectFiles?.length ? (
            <div className="message-context" aria-label="Referenced context">
              {item.documents?.map((document) => (
                <span key={document.id}>
                  <FileText aria-hidden="true" />
                  <span>
                    <strong>{document.displayName}</strong>
                    <small className="document-reference-badges">
                      <span className="document-reference-badge">
                        {documentTypeLabel(document)}
                      </span>
                      <span className="document-reference-badge is-fidelity">
                        {document.fidelity}
                      </span>
                    </small>
                  </span>
                </span>
              ))}
              {item.projectFiles?.map((file) => (
                <span key={file.id}>
                  <File aria-hidden="true" />
                  <span>
                    <strong>{file.relativePath}</strong>
                    <small>Trusted project snapshot</small>
                  </span>
                </span>
              ))}
            </div>
          ) : null}
          {item.state === "streaming" ? (
            <span
              className="user-message-state"
              role="status"
              data-delivery={item.delivery}
            >
              {item.delivery === "followUp"
                ? "Queued follow-up"
                : item.delivery === "steer"
                  ? "Steering active run"
                  : item.delivery === "submit"
                    ? "Sending"
                    : "Pending delivery"}
            </span>
          ) : null}
          {conversationBranching &&
          item.state !== "streaming" &&
          item.durableEntryId ? (
            <div className="message-actions branch-message-actions">
              <button
                type="button"
                onClick={() => onEditUserTurn(item)}
                aria-label="Edit this turn"
                title="Edit this turn"
              >
                <Pencil aria-hidden="true" />
              </button>
              <button
                type="button"
                onClick={() => onForkConversation(item)}
                aria-label="Fork conversation here"
                title="Fork into a new session"
              >
                <GitFork aria-hidden="true" />
              </button>
            </div>
          ) : null}
        </article>
      );
    }

    case "assistant_message":
      return (
        <AssistantMessage
          item={item}
          animate={animate}
          branchingAvailable={
            conversationBranching &&
            item.state !== "streaming" &&
            Boolean(item.durableEntryId)
          }
          onRetry={() => onRetryResponse(item)}
          onRetryWithModel={() => onRetryResponseWithModel(item)}
          onFork={() => onForkConversation(item)}
        />
      );

    case "reasoning":
      return (
        <details
          className={`reasoning-block ${item.state === "streaming" ? "is-live" : ""} ${animate ? "is-entering" : ""}`}
        >
          <summary aria-live={item.state === "streaming" ? "polite" : undefined}>
            {item.state === "streaming" ? (
              <LiveDots animated={false} />
            ) : (
              <BrainCircuit aria-hidden="true" />
            )}
            <span>{reasoningDisplayLabel(item)}</span>
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
          durationOverrideMs={actionDurationOverrideMs}
          groupExpanded={groupExpanded}
          sessionId={sessionId}
          resourceContentUrl={resourceContentUrl}
          onOpenOutput={onOpenOutput}
          onOpenSource={onOpenSource}
          onOpenResource={onOpenResource}
          availableOutputs={availableOutputs}
          availableSources={availableSources}
          outputsByOrigin={outputsByOrigin}
          sourcesByOrigin={sourcesByOrigin}
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
      return null;

  }
});

function AnchoredTranscriptItem(
  props: ComponentProps<typeof TranscriptItemView>,
) {
  return (
    <div
      id={`transcript-item-${props.item.id}`}
      className="transcript-item-anchor"
      data-item-id={props.item.id}
      tabIndex={-1}
    >
      <TranscriptItemView {...props} />
    </div>
  );
}

type WorkItem = Extract<
  TranscriptItem,
  { kind: "action" | "reasoning" }
>;

const actionSummaryLabels: Record<
  ActionItem["actionKind"],
  (count: number) => string
> = {
  command: (count) => `Ran ${count === 1 ? "command" : "commands"}`,
  file_read: () => "Read files",
  file_search: () => "Searched files",
  file_write: (count) => `Edited ${count === 1 ? "file" : "files"}`,
  web_search: () => "Searched the web",
  skill: (count) => `Used ${count === 1 ? "skill" : "skills"}`,
  preview: (count) => `Viewed ${count === 1 ? "preview" : "previews"}`,
  analysis: () => "Inspected results",
};

function describeWork(items: readonly WorkItem[]): string {
  if (items.length === 1) {
    const item = items[0]!;
    return item.kind === "action"
      ? item.label
      : reasoningDisplayLabel(item);
  }
  const actionCounts = new Map<ActionItem["actionKind"], number>();
  for (const item of items) {
    if (item.kind !== "action") continue;
    actionCounts.set(
      item.actionKind,
      (actionCounts.get(item.actionKind) ?? 0) + 1,
    );
  }
  const labels = Array.from(actionCounts, ([kind, count]) =>
    actionSummaryLabels[kind](count),
  );
  if (labels.length) {
    return labels
      .map((label, index) =>
        index === 0 ? label : `${label[0]!.toLowerCase()}${label.slice(1)}`,
      )
      .join(", ");
  }
  const reasoning = [...items]
    .reverse()
    .find(
      (item): item is Extract<WorkItem, { kind: "reasoning" }> =>
        item.kind === "reasoning",
    );
  return reasoning ? reasoningDisplayLabel(reasoning) : "Work details";
}

type WorkEntry =
  | { kind: "item"; item: WorkItem }
  | { kind: "command_batch"; id: string; items: ActionItem[] };

function aggregateWorkEntries(items: readonly WorkItem[]): WorkEntry[] {
  const entries: WorkEntry[] = [];
  let index = 0;
  while (index < items.length) {
    const item = items[index]!;
    if (
      item.kind === "action" &&
      item.actionKind === "command" &&
      item.status !== "running"
    ) {
      const commands: ActionItem[] = [item];
      let nextIndex = index + 1;
      while (nextIndex < items.length) {
        const candidate = items[nextIndex]!;
        if (
          candidate.kind !== "action" ||
          candidate.actionKind !== "command" ||
          candidate.status === "running" ||
          candidate.rawToolName !== item.rawToolName
        ) {
          break;
        }
        commands.push(candidate);
        nextIndex += 1;
      }
      if (commands.length > 1) {
        entries.push({
          kind: "command_batch",
          id: `commands-${commands[0]!.id}`,
          items: commands,
        });
      } else {
        entries.push({ kind: "item", item });
      }
      index = nextIndex;
      continue;
    }
    entries.push({ kind: "item", item });
    index += 1;
  }
  return entries;
}

type TranscriptRow =
  | { kind: "item"; item: TranscriptItem }
  | {
      kind: "work";
      id: string;
      items: WorkItem[];
      outcome?: Extract<TranscriptItem, { kind: "run_outcome" }>;
    };

function workIdentity(item: TranscriptItem): string {
  return item.runId ? `run:${item.runId}` : `turn:${item.turnId}`;
}

function transcriptRows(items: TranscriptItem[]): TranscriptRow[] {
  const lastWorkIndexByRun = new Map<string, number>();
  const outcomeByRun = new Map<
    string,
    Extract<TranscriptItem, { kind: "run_outcome" }>
  >();
  items.forEach((item, index) => {
    if (item.kind === "action" || item.kind === "reasoning") {
      lastWorkIndexByRun.set(workIdentity(item), index);
    } else if (item.kind === "run_outcome") {
      outcomeByRun.set(workIdentity(item), item);
    }
  });

  const rows: TranscriptRow[] = [];
  let index = 0;
  while (index < items.length) {
    const item = items[index]!;
    if (item.kind === "action" || item.kind === "reasoning") {
      const workItems: WorkItem[] = [];
      const identity = workIdentity(item);
      let lastIndex = index;
      while (lastIndex < items.length) {
        const candidate = items[lastIndex]!;
        if (
          workIdentity(candidate) !== identity ||
          (candidate.kind !== "action" && candidate.kind !== "reasoning")
        ) {
          break;
        }
        workItems.push(candidate);
        lastIndex += 1;
      }
      const ownsOutcome =
        lastWorkIndexByRun.get(identity) === lastIndex - 1;
      rows.push({
        kind: "work",
        id: `work-${workItems[0]!.id}`,
        items: workItems,
        outcome: ownsOutcome ? outcomeByRun.get(identity) : undefined,
      });
      index = lastIndex;
      continue;
    }
    if (
      item.kind === "run_outcome" &&
      lastWorkIndexByRun.has(workIdentity(item))
    ) {
      index += 1;
      continue;
    }
    rows.push({ kind: "item", item });
    index += 1;
  }
  return rows;
}

function latestActiveWorkRowId(
  rows: readonly TranscriptRow[],
  status: SessionSnapshot["status"],
  activeRunId: SessionSnapshot["activeRunId"],
): string | undefined {
  if (status !== "working" || !activeRunId) return undefined;

  for (let index = rows.length - 1; index >= 0; index -= 1) {
    const row = rows[index]!;
    if (
      row.kind === "work" &&
      row.items.some((item) => item.runId === activeRunId)
    ) {
      return row.id;
    }
  }
  return undefined;
}

interface WorkGroupProps {
  row: Extract<TranscriptRow, { kind: "work" }>;
  active: boolean;
  initialItemIds: ReadonlySet<string>;
  onResolveApproval: ConversationProps["onResolveApproval"];
  onResolveUserInput: ConversationProps["onResolveUserInput"];
  onOpenOutput: (outputId: string) => void;
  onOpenSource: (sourceId: string) => void;
  onOpenResource: ConversationProps["onOpenResource"];
  sessionId: string;
  resourceContentUrl?: ResourceContentUrl;
  availableOutputs: ReadonlyMap<string, OutputRef>;
  availableSources: ReadonlyMap<string, SourceRef>;
  outputsByOrigin: ReadonlyMap<string, readonly OutputRef[]>;
  sourcesByOrigin: ReadonlyMap<string, readonly SourceRef[]>;
  attachmentContentUrl?: (handle: string) => string;
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
}

const WorkGroup = memo(function WorkGroup({
  row,
  active,
  initialItemIds,
  onResolveApproval,
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  onOpenResource,
  sessionId,
  resourceContentUrl,
  availableOutputs,
  availableSources,
  outputsByOrigin,
  sourcesByOrigin,
  attachmentContentUrl,
  onPreviewAttachment,
}: WorkGroupProps) {
  const live = active;
  const [userOpen, setUserOpen] = useState(false);
  const itemDuration = row.items.reduce(
    (total, item) =>
      total + (item.kind === "action" ? item.durationMs ?? 0 : 0),
    0,
  );
  const duration = row.outcome?.durationMs || itemDuration;
  const failedActionCount = row.items.filter(
    (item) => item.kind === "action" && item.status === "failed",
  ).length;
  const historyLabel = [
    describeWork(row.items),
    duration > 0 ? formatDuration(duration) : null,
    failedActionCount
      ? `${failedActionCount} ${failedActionCount === 1 ? "failure" : "failures"}`
      : null,
  ]
    .filter((part): part is string => Boolean(part))
    .join(" · ");
  const label = live ? "Working" : historyLabel;
  const open = row.items.length > 0 && userOpen;
  const directAction =
    !live && row.items.length === 1 && row.items[0]?.kind === "action"
      ? row.items[0]
      : undefined;

  if (directAction) {
    return (
      <section
        className="work-group is-complete is-direct"
        aria-label={historyLabel}
      >
        <AnchoredTranscriptItem
          item={directAction}
          sessionId={sessionId}
          resourceContentUrl={resourceContentUrl}
          groupExpanded={false}
          animate={!initialItemIds.has(directAction.id)}
          actionDurationOverrideMs={duration}
          onResolveApproval={onResolveApproval}
          onResolveUserInput={onResolveUserInput}
          onOpenOutput={onOpenOutput}
          onOpenSource={onOpenSource}
          onOpenResource={onOpenResource}
          availableOutputs={availableOutputs}
          availableSources={availableSources}
          outputsByOrigin={outputsByOrigin}
          sourcesByOrigin={sourcesByOrigin}
          attachmentContentUrl={attachmentContentUrl}
          onPreviewAttachment={onPreviewAttachment}
        />
      </section>
    );
  }

  return (
    <section
      className={`work-group ${live ? "is-live" : "is-complete"} ${open ? "is-open" : "is-collapsed"}`}
      aria-label={label}
    >
      {row.items.length ? (
        <>
          <button
            type="button"
            className="work-group-summary"
            aria-expanded={open}
            onClick={() => setUserOpen((current) => !current)}
          >
            <span
              className={`work-group-glyph ${live ? "is-live" : "is-finished"}`}
            >
              {live ? (
                <LiveDots />
              ) : row.outcome?.outcome === "failed" ? (
                <AlertTriangle aria-hidden="true" />
              ) : (
                <Check aria-hidden="true" />
              )}
            </span>
            <span>{label}</span>
            <ChevronDown aria-hidden="true" />
          </button>
          <div
            className="work-group-content-clip"
            aria-hidden={!open}
            inert={!open}
          >
            <div className="work-group-content">
              {aggregateWorkEntries(row.items).map((entry) =>
                entry.kind === "command_batch" ? (
                  <div
                    className="command-batch"
                    key={entry.id}
                    role="group"
                    aria-label="Bash commands"
                  >
                    {entry.items.map((item) => (
                      <AnchoredTranscriptItem
                        key={item.id}
                        item={item}
                        sessionId={sessionId}
                        resourceContentUrl={resourceContentUrl}
                        groupExpanded={open}
                        animate={false}
                        onResolveApproval={onResolveApproval}
                        onResolveUserInput={onResolveUserInput}
                        onOpenOutput={onOpenOutput}
                        onOpenSource={onOpenSource}
                        onOpenResource={onOpenResource}
                        availableOutputs={availableOutputs}
                        availableSources={availableSources}
                        outputsByOrigin={outputsByOrigin}
                        sourcesByOrigin={sourcesByOrigin}
                        attachmentContentUrl={attachmentContentUrl}
                        onPreviewAttachment={onPreviewAttachment}
                      />
                    ))}
                  </div>
                ) : (
                  <AnchoredTranscriptItem
                    key={entry.item.id}
                    item={entry.item}
                    sessionId={sessionId}
                    resourceContentUrl={resourceContentUrl}
                    groupExpanded={open}
                    animate={!initialItemIds.has(entry.item.id)}
                    onResolveApproval={onResolveApproval}
                    onResolveUserInput={onResolveUserInput}
                    onOpenOutput={onOpenOutput}
                    onOpenSource={onOpenSource}
                    onOpenResource={onOpenResource}
                    availableOutputs={availableOutputs}
                    availableSources={availableSources}
                    outputsByOrigin={outputsByOrigin}
                    sourcesByOrigin={sourcesByOrigin}
                    attachmentContentUrl={attachmentContentUrl}
                    onPreviewAttachment={onPreviewAttachment}
                  />
                ),
              )}
            </div>
          </div>
        </>
      ) : null}
    </section>
  );
}, (previous, next) => {
  if (
    previous.row.id !== next.row.id ||
    previous.row.outcome !== next.row.outcome ||
    previous.row.items.length !== next.row.items.length
  ) {
    return false;
  }
  for (let index = 0; index < previous.row.items.length; index += 1) {
    if (previous.row.items[index] !== next.row.items[index]) return false;
  }
  return (
    previous.active === next.active &&
    previous.initialItemIds === next.initialItemIds &&
    previous.onResolveApproval === next.onResolveApproval &&
    previous.onResolveUserInput === next.onResolveUserInput &&
    previous.onOpenOutput === next.onOpenOutput &&
    previous.onOpenSource === next.onOpenSource &&
    previous.onOpenResource === next.onOpenResource &&
    previous.sessionId === next.sessionId &&
    previous.resourceContentUrl === next.resourceContentUrl &&
    previous.availableOutputs === next.availableOutputs &&
    previous.availableSources === next.availableSources &&
    previous.outputsByOrigin === next.outputsByOrigin &&
    previous.sourcesByOrigin === next.sourcesByOrigin &&
    previous.attachmentContentUrl === next.attachmentContentUrl &&
    previous.onPreviewAttachment === next.onPreviewAttachment
  );
});

function EmptySession({
  attachments,
}: {
  attachments: boolean;
}) {
  return (
    <div className="empty-session">
      <span className="empty-session-eyebrow">New workspace task</span>
      <h1>What should we work on?</h1>
      <p>
        {attachments
          ? "Describe the change you want to make. Add files or images when they help."
          : "Describe the change you want to make in this workspace."}
      </p>
    </div>
  );
}

function ReasoningPowerSlider({
  options,
  value,
  formatValue,
  showLabel = true,
  disabled,
  onChange,
}: {
  options: ReasoningEffort[];
  value: ReasoningEffort;
  formatValue?: (value: ReasoningEffort) => string;
  showLabel?: boolean;
  disabled: boolean;
  onChange: (value: ReasoningEffort) => void;
}) {
  const [draftValue, setDraftValue] = useState(value);
  const [dragging, setDragging] = useState(false);
  const pendingValueRef = useRef<ReasoningEffort | null>(null);
  const pendingTimerRef = useRef<number | null>(null);
  const latestValueRef = useRef(value);
  const selectedValue = options.includes(draftValue) ? draftValue : value;
  const selectedIndex = Math.max(0, options.indexOf(selectedValue));
  const lastIndex = Math.max(0, options.length - 1);
  const position = lastIndex ? (selectedIndex / lastIndex) * 100 : 0;
  const wheelDeltaRef = useRef(0);
  const selectedEffort = (options[selectedIndex] ?? "").toLowerCase();
  const isMax = selectedEffort === "max";
  const hasFloatingParticles = selectedEffort === "xhigh";
  const thumbOffset = Number((14 - position * 0.28).toFixed(4));
  const visibleValue = formatValue?.(selectedValue) ?? selectedValue;

  useEffect(() => {
    latestValueRef.current = value;
  }, [value]);

  useEffect(() => {
    if (
      pendingValueRef.current &&
      value !== pendingValueRef.current &&
      options.includes(draftValue)
    ) {
      return;
    }
    pendingValueRef.current = null;
    if (pendingTimerRef.current !== null) {
      window.clearTimeout(pendingTimerRef.current);
      pendingTimerRef.current = null;
    }
    setDraftValue(value);
  }, [draftValue, options, value]);

  useEffect(
    () => () => {
      if (pendingTimerRef.current !== null) {
        window.clearTimeout(pendingTimerRef.current);
      }
    },
    [],
  );

  const selectIndex = (index: number) => {
    const nextIndex = Math.max(0, Math.min(lastIndex, index));
    const next = options[nextIndex];
    if (!next || next === selectedValue || disabled) return;
    setDraftValue(next);
    pendingValueRef.current = next;
    if (pendingTimerRef.current !== null) {
      window.clearTimeout(pendingTimerRef.current);
    }
    pendingTimerRef.current = window.setTimeout(() => {
      pendingValueRef.current = null;
      pendingTimerRef.current = null;
      setDraftValue(latestValueRef.current);
    }, 1_000);
    onChange(next);
  };

  return (
    <div className="reasoning-power-control">
      {showLabel ? (
        <span className="reasoning-power-label" aria-hidden="true">
          {visibleValue}
        </span>
      ) : null}
      <div className="power-slider-container">
        <div
          className="power-slider-root"
          data-max={isMax}
          data-overdrive={hasFloatingParticles}
          data-dragging={dragging || undefined}
          data-disabled={disabled || undefined}
          style={
            {
              "--power-position": `${position}%`,
              "--power-thumb-position": `calc(${position}% + ${thumbOffset}px)`,
            } as CSSProperties
          }
        >
          <div className="power-slider-track" aria-hidden="true">
            <span className="power-slider-range" />
            <span className="power-slider-max-fill" />
            <span className="power-slider-fast-particles">
              {Array.from({ length: 12 }, (_, index) => (
                <span key={index} />
              ))}
            </span>
          </div>
          <span className="power-slider-thumb-rail" aria-hidden="true">
            <span className="power-slider-thumb" />
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
            aria-valuetext={visibleValue}
            onPointerDown={() => setDragging(true)}
            onPointerUp={() => setDragging(false)}
            onPointerCancel={() => setDragging(false)}
            onBlur={() => setDragging(false)}
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

function preciseEffortLabel(value: ReasoningEffort): string {
  if (value.toLowerCase() === "xhigh") return "Extra high";
  return value
    .replaceAll(/[-_]/g, " ")
    .replace(/\b\w/g, (character) => character.toUpperCase());
}

function abstractEffortLabel(
  options: ReasoningEffort[],
  value: ReasoningEffort,
): string {
  const selectedIndex = Math.max(0, options.indexOf(value));
  if (options.length > 1 && selectedIndex === options.length - 1) {
    return "Max";
  }
  return preciseEffortLabel(value);
}

function compactModelLabel(name: string): string {
  return name.replace(/^(?:Claude|OpenAI)\s+/i, "");
}

function ModelPicker({
  models,
  value,
  reasoningOptions,
  reasoning,
  disabled,
  hasStagedImages,
  onChange,
  onReasoningChange,
}: {
  models: ModelSummary[];
  value: string;
  reasoningOptions: ReasoningEffort[];
  reasoning: ReasoningEffort;
  disabled: boolean;
  hasStagedImages: boolean;
  onChange: (modelId: string) => void;
  onReasoningChange: (reasoning: ReasoningEffort) => void;
}) {
  const [open, setOpen] = useState(false);
  const [view, setView] = useState<
    "simple" | "advanced" | "models" | "effort"
  >("simple");
  const [query, setQuery] = useState("");
  const popoverPresence = useExitPresence(open);
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
  const simpleEffort = abstractEffortLabel(reasoningOptions, reasoning);
  const exactEffort = preciseEffortLabel(reasoning);
  const isMax = simpleEffort === "Max";

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
      setOpen(false);
      window.requestAnimationFrame(() => triggerRef.current?.focus());
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  useEffect(() => {
    if (!open) return;
    window.requestAnimationFrame(() => {
      if (view === "models") {
        searchRef.current?.focus();
        return;
      }
      popoverRef.current
        ?.querySelector<HTMLElement>("[data-autofocus]")
        ?.focus();
    });
  }, [open, view]);

  useEffect(() => {
    if (popoverPresence.present) return;
    const frame = window.requestAnimationFrame(() => {
      setQuery("");
      setView("simple");
    });
    return () => window.cancelAnimationFrame(frame);
  }, [popoverPresence.present]);

  useEffect(() => {
    if (!disabled || !open) return;
    const frame = window.requestAnimationFrame(() => setOpen(false));
    return () => window.cancelAnimationFrame(frame);
  }, [disabled, open]);

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
    setView("advanced");
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

  const renderSimpleView = () => (
    <div className="model-picker-overview">
      <button
        type="button"
        className="model-picker-advanced-toggle"
        data-autofocus
        onClick={() => setView("advanced")}
      >
        <span>Advanced</span>
        <ChevronRight aria-hidden="true" />
        <Zap aria-hidden="true" />
      </button>
      <ReasoningPowerSlider
        options={reasoningOptions}
        value={reasoning}
        formatValue={(effort) =>
          abstractEffortLabel(reasoningOptions, effort)
        }
        showLabel={false}
        disabled={disabled}
        onChange={onReasoningChange}
      />
    </div>
  );

  const renderAdvancedView = () => (
    <div className="model-picker-advanced-panel">
      <button
        type="button"
        className="model-picker-setting-row"
        data-autofocus
        onClick={() => setView("models")}
      >
        <span>Model</span>
        <strong>
          {activeModel?.name ?? value}
          <ChevronRight aria-hidden="true" />
        </strong>
      </button>
      <button
        type="button"
        className="model-picker-setting-row"
        onClick={() => setView("effort")}
      >
        <span>Effort</span>
        <strong>
          {exactEffort}
          <ChevronRight aria-hidden="true" />
        </strong>
      </button>
      <button
        type="button"
        className="model-picker-advanced-toggle is-expanded"
        onClick={() => setView("simple")}
      >
        <span>Advanced</span>
        <ChevronUp aria-hidden="true" />
      </button>
    </div>
  );

  const renderModelView = () => (
    <>
      <button
        type="button"
        className="model-picker-back"
        onClick={() => setView("advanced")}
      >
        <ChevronLeft aria-hidden="true" />
        <strong>Model</strong>
      </button>
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
    </>
  );

  const renderEffortView = () => (
    <>
      <button
        type="button"
        className="model-picker-back"
        data-autofocus
        onClick={() => setView("advanced")}
      >
        <ChevronLeft aria-hidden="true" />
        <strong>Effort</strong>
      </button>
      <div
        className="model-picker-effort-list"
        role="listbox"
        aria-label="Exact effort levels"
      >
        {reasoningOptions.map((option) => (
          <button
            key={option}
            type="button"
            role="option"
            aria-selected={option === reasoning}
            className={option === reasoning ? "is-selected" : undefined}
            onClick={() => {
              if (option !== reasoning) onReasoningChange(option);
              setView("advanced");
            }}
          >
            <span>{preciseEffortLabel(option)}</span>
            {option === reasoning ? <Check aria-hidden="true" /> : null}
          </button>
        ))}
      </div>
    </>
  );

  return (
    <div className="model-picker">
      <button
        ref={triggerRef}
        type="button"
        className="model-picker-trigger"
        aria-label={`Model and effort: ${activeModel?.name ?? value}, ${simpleEffort}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        data-value={value}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        {activeModel?.local ? (
          <span className="local-model-dot" aria-hidden="true" />
        ) : null}
        <span>{compactModelLabel(activeModel?.name ?? value)}</span>
        <span
          className={`model-picker-trigger-effort ${isMax ? "is-max" : ""}`}
        >
          {simpleEffort}
        </span>
        <ChevronDown aria-hidden="true" />
      </button>
      {popoverPresence.present ? (
        <div
          ref={popoverRef}
          className={`model-picker-popover ${popoverPresence.closing ? "is-closing" : ""}`}
          role="dialog"
          aria-label="Model and effort"
          aria-hidden={popoverPresence.closing || undefined}
          inert={popoverPresence.closing}
          data-view={view}
        >
          {view === "simple" ? renderSimpleView() : null}
          {view === "advanced" ? renderAdvancedView() : null}
          {view === "models" ? renderModelView() : null}
          {view === "effort" ? renderEffortView() : null}
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
  const popoverPresence = useExitPresence(open);
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
      {popoverPresence.present ? (
        <div
          ref={popoverRef}
          className={`composer-menu-popover ${popoverPresence.closing ? "is-closing" : ""}`}
          role="menu"
          aria-label={label}
          aria-hidden={popoverPresence.closing || undefined}
          inert={popoverPresence.closing}
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

type ComposerCompletionTrigger =
  | {
      kind: "files";
      key: string;
      query: string;
      start: number;
      end: number;
    }
  | {
      kind: "commands" | "skillSubcommands" | "skills";
      key: string;
      query: string;
      start: number;
      end: number;
    };

type ComposerCompletionAction =
  | { type: "file"; file: TrustedFileEntry; start: number; end: number }
  | { type: "command"; command: CommandSuggestion }
  | { type: "skillSubcommand"; name: string; acceptsArgument: boolean }
  | { type: "skill"; name: string; start: number; end: number }
  | { type: "direct"; name: "export" | "fork" | "status" };

type ComposerOption = ComposerCompletionOption & {
  action: ComposerCompletionAction;
};

const skillSubcommands = [
  {
    name: "list",
    description: "list discovered skills",
    acceptsArgument: false,
  },
  {
    name: "active",
    description: "list active skills",
    acceptsArgument: false,
  },
  {
    name: "show",
    description: "show a skill's details",
    acceptsArgument: true,
  },
  {
    name: "search",
    description: "search discovered skills",
    acceptsArgument: true,
  },
  {
    name: "load",
    description: "activate a skill",
    acceptsArgument: true,
  },
  {
    name: "reload",
    description: "rescan configured skill roots",
    acceptsArgument: false,
  },
  {
    name: "off",
    description: "deactivate a skill",
    acceptsArgument: true,
  },
] as const;

function tokenTail(value: string, caret: number, pattern = "[^\\s]"): string {
  return value.slice(caret).match(new RegExp(`^${pattern}*`, "u"))?.[0] ?? "";
}

function looksLikeAbsolutePath(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed.startsWith("/") || trimmed.includes("\n")) return false;
  const firstToken = trimmed.match(/^(?:\\\s|[^\s])+/u)?.[0] ?? "";
  return firstToken.slice(1).replaceAll("\\ ", " ").includes("/");
}

function isSlashCommandInput(value: string): boolean {
  return value.startsWith("/") && !looksLikeAbsolutePath(value);
}

function slashCommandName(value: string): string {
  return value.slice(1).trimStart().split(/\s/u, 1)[0] ?? "";
}

function completionTrigger(
  value: string,
  requestedCaret: number,
): ComposerCompletionTrigger | null {
  const caret = Math.max(0, Math.min(requestedCaret, value.length));
  const before = value.slice(0, caret);
  const fileMatch = !isSlashCommandInput(value)
    ? /(^|\s)@([^\s@]*)$/u.exec(before)
    : null;
  if (fileMatch) {
    const start = before.length - fileMatch[0].length + fileMatch[1].length;
    const tail = tokenTail(value, caret, "[^\\s@]");
    const query = `${fileMatch[2]}${tail}`;
    const end = caret + tail.length;
    return {
      kind: "files",
      key: `files:${start}:${end}:${query}`,
      query,
      start,
      end,
    };
  }

  const skillNameMatch = /^\/skills\s+(load|off|show)\s+([^\s]*)$/u.exec(
    before,
  );
  if (skillNameMatch) {
    const start = before.length - skillNameMatch[2].length;
    const tail = tokenTail(value, caret);
    const query = `${skillNameMatch[2]}${tail}`;
    const end = caret + tail.length;
    return {
      kind: "skills",
      key: `skills:${start}:${end}:${query}`,
      query,
      start,
      end,
    };
  }

  const skillSubcommandMatch = /^\/skills\s+([^\s]*)$/u.exec(before);
  if (skillSubcommandMatch) {
    const start = before.length - skillSubcommandMatch[1].length;
    const tail = tokenTail(value, caret);
    const query = `${skillSubcommandMatch[1]}${tail}`;
    const end = caret + tail.length;
    return {
      kind: "skillSubcommands",
      key: `skillSubcommands:${start}:${end}:${query}`,
      query,
      start,
      end,
    };
  }

  const commandMatch = /^\/([^\s]*)$/u.exec(before);
  if (!commandMatch || /\s/u.test(value) || looksLikeAbsolutePath(value)) {
    return null;
  }
  const tail = tokenTail(value, caret);
  const query = `${commandMatch[1]}${tail}`;
  const end = caret + tail.length;
  return {
    kind: "commands",
    key: `commands:0:${end}:${query}`,
    query,
    start: 0,
    end,
  };
}

function fuzzyScore(value: string, query: string): number | null {
  const needle = query.toLocaleLowerCase();
  if (!needle) return 0;
  const haystack = value.toLocaleLowerCase();
  let cursor = 0;
  let previous = -1;
  let score = 0;
  for (const character of needle) {
    const index = haystack.indexOf(character, cursor);
    if (index < 0) return null;
    score += index - previous - 1;
    previous = index;
    cursor = index + 1;
  }
  return score + (haystack.startsWith(needle) ? -100 : 0);
}

function fuzzyTrustedFiles(
  files: TrustedFileEntry[],
  query: string,
): TrustedFileEntry[] {
  return files
    .map((file, index) => {
      const displayScore = fuzzyScore(file.displayName, query);
      const pathScore = fuzzyScore(file.relativePath, query);
      const score =
        displayScore === null
          ? pathScore
          : pathScore === null
            ? displayScore
            : Math.min(displayScore, pathScore);
      return { file, index, score };
    })
    .filter(
      (candidate): candidate is { file: TrustedFileEntry; index: number; score: number } =>
        candidate.score !== null,
    )
    .sort((left, right) => left.score - right.score || left.index - right.index)
    .map((candidate) => candidate.file);
}

function trustedFileIcon(kind: TrustedFileEntry["kind"]): ReactNode {
  if (kind === "source") return <FileDiff />;
  if (kind === "configuration") return <File />;
  return <FileText />;
}

function byteLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function estimatedContextInputCost(
  contextTokens: number,
  model: ModelSummary | undefined,
): string | undefined {
  const pricing = model?.inputPricing;
  if (!pricing) return undefined;

  let rate = pricing.baseMicrodollarsPerMillionTokens;
  for (const tier of pricing.tiers) {
    if (contextTokens < tier.minInputTokens) break;
    rate = tier.microdollarsPerMillionTokens;
  }
  const dollars =
    (contextTokens / 1_000_000) * (rate / 1_000_000);
  return dollars.toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: dollars < 1 ? 4 : 2,
  });
}

function Composer({
  session,
  bootstrap,
  goal = null,
  onGoalCommand,
  onSubmit,
  onInterrupt,
  onConfigure,
  onIngestAttachment,
  onIngestDocument,
  onListProjectFiles,
  onSearchProjectFiles,
  onReadProjectFile,
  onGetCommandDiscovery,
  onInvokeSlashCommand,
  onExportSession,
  onForkSession,
  onOpenRuntimeStatus,
  onPreviewAttachment,
  attachmentContentUrl,
}: Pick<
  ConversationProps,
  | "session"
  | "bootstrap"
  | "goal"
  | "onGoalCommand"
  | "onSubmit"
  | "onInterrupt"
  | "onConfigure"
  | "onIngestAttachment"
  | "onIngestDocument"
  | "onListProjectFiles"
  | "onSearchProjectFiles"
  | "onReadProjectFile"
  | "onGetCommandDiscovery"
  | "onInvokeSlashCommand"
  | "onExportSession"
  | "onForkSession"
  | "onOpenRuntimeStatus"
  | "attachmentContentUrl"
> & {
  onPreviewAttachment: (
    source: string,
    name: string,
    trigger: HTMLElement,
  ) => void;
}) {
  const [draftStore] = useState(browserDraftStore);
  const [restoredDraft] = useState(() =>
    draftStore.load(bootstrap.host.id, session.sessionId),
  );
  const [prompt, setPrompt] = useState(restoredDraft?.text ?? "");
  const [attachments, setAttachments] = useState<DraftAttachment[]>(() =>
    (restoredDraft?.attachments ?? []).map((reference) => ({
      localId: `restored-${reference.id}`,
      status: "uploaded",
      reference,
    })),
  );
  const [documents, setDocuments] = useState<DocumentReference[]>([]);
  const [projectFiles, setProjectFiles] = useState<TrustedFileEntry[]>([]);
  const [composerCaret, setComposerCaret] = useState(prompt.length);
  const [fileCatalog, setFileCatalog] = useState<TrustedFileCatalog | null>(
    null,
  );
  const [fileCatalogLoading, setFileCatalogLoading] = useState(false);
  const [fileCatalogError, setFileCatalogError] = useState<string | null>(null);
  const [commandDiscovery, setCommandDiscovery] =
    useState<CommandDiscovery | null>(null);
  const [commandDiscoveryLoading, setCommandDiscoveryLoading] = useState(false);
  const [commandDiscoveryError, setCommandDiscoveryError] = useState<
    string | null
  >(null);
  const [dismissedCompletionKey, setDismissedCompletionKey] = useState<
    string | null
  >(null);
  const [activeCompletionIndex, setActiveCompletionIndex] = useState(0);
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  const [draggingFiles, setDraggingFiles] = useState(false);
  const [activeDelivery, setActiveDelivery] = useState<
    "steer" | "followUp"
  >(() => {
    if (
      restoredDraft?.delivery === "followUp" &&
      bootstrap.capabilities.followUp
    ) {
      return "followUp";
    }
    if (restoredDraft?.delivery === "steer" && bootstrap.capabilities.steer) {
      return "steer";
    }
    return bootstrap.capabilities.followUp ? "followUp" : "steer";
  });
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<SubmissionFailure | null>(
    null,
  );
  const [commandFeedback, setCommandFeedback] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const pendingComposerCaretRef = useRef<number | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragDepthRef = useRef(0);
  const attachmentsRef = useRef<DraftAttachment[]>([]);
  const promptRef = useRef(prompt);
  const fileCatalogRequestRef = useRef(0);
  const commandDiscoveryRequestRef = useRef(0);
  const mountedRef = useRef(true);
  const pendingCommandRef = useRef<{
    id: string;
    signature: string;
  } | null>(null);
  const pendingSlashCommandRef = useRef<{
    id: string;
    invocation: string;
  } | null>(null);
  const isWorking =
    Boolean(session.activeRunId) ||
    session.status === "working" ||
    session.status === "needs_attention";
  // Approval/input pauses retain an active run but do not animate the model
  // shimmer. The TUI only advances its perimeter while the model is running.
  const isModelWorking = session.status === "working";
  const activeModel = bootstrap.models.find(
    (model) => model.id === session.modelId,
  );
  const contextCost = estimatedContextInputCost(
    session.contextTokens,
    activeModel,
  );
  const contextDescription = `${session.contextPercent}% of context used${
    contextCost ? `; estimated next-turn input cost ~${contextCost}` : ""
  }`;
  const providerKey =
    `${activeModel?.provider ?? ""} ${activeModel?.id ?? ""}`.toLowerCase();
  const modelAccent =
    providerAccents.find(([provider]) => providerKey.includes(provider))?.[1] ??
    "#16876d";
  const reasoningOptions =
    bootstrap.models.find((model) => model.id === session.modelId)?.reasoning ??
    [session.reasoning];
  const composerStyle = {
    "--model-accent-light": balancedAccent(modelAccent, 0.11),
    "--model-accent-dark": balancedAccent(modelAccent, 0.27),
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
  const documentsAvailable = Boolean(
    bootstrap.capabilities.documents && onIngestDocument,
  );
  const projectFilesAvailable = Boolean(
    bootstrap.capabilities.trustedProjectFiles &&
      onListProjectFiles &&
      onSearchProjectFiles &&
      onReadProjectFile,
  );
  const triggeredCompletion = useMemo(
    () => completionTrigger(prompt, composerCaret),
    [composerCaret, prompt],
  );
  const activeCompletion =
    triggeredCompletion &&
    triggeredCompletion.key !== dismissedCompletionKey &&
    (triggeredCompletion.kind !== "files" || projectFilesAvailable)
      ? triggeredCompletion
      : null;

  useEffect(() => {
    if (
      activeCompletion?.kind !== "files" ||
      !onListProjectFiles ||
      fileCatalog ||
      fileCatalogLoading ||
      fileCatalogError
    ) {
      return;
    }
    const request = ++fileCatalogRequestRef.current;
    setFileCatalogLoading(true);
    void onListProjectFiles()
      .then((catalog) => {
        if (request === fileCatalogRequestRef.current) setFileCatalog(catalog);
      })
      .catch((error: unknown) => {
        if (request === fileCatalogRequestRef.current) {
          setFileCatalogError(
            error instanceof Error
              ? error.message
              : "Trusted project files could not be loaded.",
          );
        }
      })
      .finally(() => {
        if (request === fileCatalogRequestRef.current) {
          setFileCatalogLoading(false);
        }
      });
  }, [
    activeCompletion?.kind,
    fileCatalog,
    fileCatalogError,
    fileCatalogLoading,
    onListProjectFiles,
  ]);

  useEffect(() => {
    if (
      (activeCompletion?.kind !== "commands" &&
        activeCompletion?.kind !== "skills") ||
      !onGetCommandDiscovery ||
      commandDiscovery ||
      commandDiscoveryLoading ||
      commandDiscoveryError
    ) {
      return;
    }
    const request = ++commandDiscoveryRequestRef.current;
    setCommandDiscoveryLoading(true);
    void onGetCommandDiscovery()
      .then((discovery) => {
        if (request === commandDiscoveryRequestRef.current) {
          setCommandDiscovery(discovery);
        }
      })
      .catch((error: unknown) => {
        if (request === commandDiscoveryRequestRef.current) {
          setCommandDiscoveryError(
            error instanceof Error
              ? error.message
              : "Slash commands could not be loaded.",
          );
        }
      })
      .finally(() => {
        if (request === commandDiscoveryRequestRef.current) {
          setCommandDiscoveryLoading(false);
        }
      });
  }, [
    activeCompletion?.kind,
    commandDiscovery,
    commandDiscoveryError,
    commandDiscoveryLoading,
    onGetCommandDiscovery,
  ]);

  const completionOptions = useMemo<ComposerOption[]>(() => {
    if (!activeCompletion) return [];
    if (activeCompletion.kind === "files") {
      const selected = new Set(projectFiles.map((file) => file.id));
      return fuzzyTrustedFiles(fileCatalog?.files ?? [], activeCompletion.query)
        .filter((file) => !selected.has(file.id))
        .slice(0, 100)
        .map((file) => ({
          id: `file-${file.id}`,
          title: file.displayName,
          description: file.relativePath,
          meta: byteLabel(file.byteLen),
          icon: trustedFileIcon(file.kind),
          disabled: projectFiles.length >= 20,
          action: {
            type: "file",
            file,
            start: activeCompletion.start,
            end: activeCompletion.end,
          },
        }));
    }

    if (activeCompletion.kind === "skillSubcommands") {
      const query = activeCompletion.query.toLocaleLowerCase();
      return skillSubcommands
        .filter((subcommand) => subcommand.name.startsWith(query))
        .map((subcommand) => ({
          id: `skill-subcommand-${subcommand.name}`,
          title: `/skills ${subcommand.name}`,
          description: subcommand.description,
          disabled: isWorking || !onInvokeSlashCommand,
          action: {
            type: "skillSubcommand",
            name: subcommand.name,
            acceptsArgument: subcommand.acceptsArgument,
          },
        }));
    }

    if (activeCompletion.kind === "skills") {
      return (commandDiscovery?.skills ?? [])
        .filter((skill) =>
          fuzzyScore(
            `${skill.id} ${skill.name} ${skill.description}`,
            activeCompletion.query,
          ) !== null,
        )
        .map((skill) => ({
          id: `skill-${skill.id}`,
          title: skill.name,
          description: skill.description,
          meta: skill.active ? "Active" : undefined,
          icon: <BrainCircuit />,
          disabled: isWorking || !onInvokeSlashCommand,
          action: {
            type: "skill",
            name: skill.id,
            start: activeCompletion.start,
            end: activeCompletion.end,
          },
        }));
    }

    const direct = new Map<
      string,
      Omit<ComposerOption, "id"> & {
        action: Extract<ComposerCompletionAction, { type: "direct" }>;
      }
    >();
    if (onExportSession && bootstrap.capabilities.sessionExport) {
      direct.set("export", {
        title: "/export",
        description: "download this session with secret redaction",
        icon: <Download />,
        disabled: isWorking,
        action: { type: "direct", name: "export" },
      });
    }
    if (onOpenRuntimeStatus) {
      direct.set("status", {
        title: "/status",
        description: "open runtime status",
        icon: <ScanSearch />,
        disabled: false,
        action: { type: "direct", name: "status" },
      });
    }
    if (onForkSession && bootstrap.capabilities.conversationBranching) {
      direct.set("fork", {
        title: "/fork",
        description: "fork this conversation at its current checkpoint",
        icon: <GitFork />,
        disabled: isWorking || !session.branches.head,
        action: { type: "direct", name: "fork" },
      });
    }

    const ordered = [...(commandDiscovery?.commands ?? [])].filter(
      (command) =>
        !["export", "fork", "status"].includes(command.name) ||
        direct.has(command.name),
    );
    const insertDirect = (
      name: "export" | "fork" | "status",
      after?: string,
    ) => {
      if (!direct.has(name) || ordered.some((command) => command.name === name)) {
        return;
      }
      const index = after
        ? ordered.findIndex((command) => command.name === after)
        : -1;
      ordered.splice(index < 0 ? ordered.length : index + 1, 0, {
        name,
        usage: `/${name}`,
        description: direct.get(name)!.description ?? "",
        acceptsArgument: false,
        kind: "builtIn",
      });
    };
    insertDirect("status", "logout");
    insertDirect("export", "sessions");
    insertDirect("fork", "export");
    const query = activeCompletion.query.toLocaleLowerCase();
    return ordered
      .filter((command) => command.name.toLocaleLowerCase().startsWith(query))
      .map((command) => {
        const replacement = direct.get(command.name);
        if (replacement) return { ...replacement, id: `command-${command.name}` };
        return {
          id: `command-${command.name}`,
          title: `/${command.name}`,
          description: command.description,
          meta: command.argumentHint ?? command.usage,
          icon:
            command.kind === "prompt" ? (
              <FileText />
            ) : command.kind === "extension" ? (
              <TerminalSquare />
            ) : (
              <Zap />
            ),
          disabled: isWorking || !onInvokeSlashCommand,
          action: { type: "command", command },
        };
      });
  }, [
    activeCompletion,
    bootstrap.capabilities.conversationBranching,
    bootstrap.capabilities.sessionExport,
    commandDiscovery?.commands,
    commandDiscovery?.skills,
    fileCatalog?.files,
    isWorking,
    onExportSession,
    onForkSession,
    onInvokeSlashCommand,
    onOpenRuntimeStatus,
    projectFiles,
    session.branches.head,
  ]);
  const completionLoading =
    activeCompletion?.kind === "files"
      ? fileCatalogLoading
      : activeCompletion?.kind === "commands" || activeCompletion?.kind === "skills"
        ? commandDiscoveryLoading
        : false;
  const completionError =
    activeCompletion?.kind === "files"
      ? fileCatalogError
      : activeCompletion?.kind === "commands" || activeCompletion?.kind === "skills"
        ? commandDiscoveryError
        : null;
  const completionLabel =
    activeCompletion?.kind === "files"
      ? "Trusted project files"
      : activeCompletion?.kind === "skills"
        ? "Available skills"
        : activeCompletion?.kind === "skillSubcommands"
          ? "Skill commands"
          : "Slash commands";

  useEffect(() => {
    setActiveCompletionIndex(0);
  }, [activeCompletion?.key]);

  useEffect(() => {
    setActiveCompletionIndex((index) =>
      Math.max(0, Math.min(index, completionOptions.length - 1)),
    );
  }, [completionOptions.length]);

  const hasStagedImages = attachments.some((attachment) =>
    draftAttachmentMediaType(attachment).startsWith("image/"),
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
    (Boolean(prompt.trim()) ||
      uploadedAttachments.length > 0 ||
      documents.length > 0 ||
      projectFiles.length > 0) &&
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
    promptRef.current = prompt;
  }, [prompt]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      const references = attachments.flatMap((attachment) =>
        attachment.reference ? [attachment.reference] : [],
      );
      if (!prompt && references.length === 0) {
        draftStore.clear(bootstrap.host.id, session.sessionId);
        return;
      }
      draftStore.save(bootstrap.host.id, session.sessionId, {
        text: prompt,
        delivery: isWorking ? activeDelivery : "submit",
        attachments: references,
        updatedAt: new Date().toISOString(),
      });
    }, 120);
    return () => window.clearTimeout(timer);
  }, [
    activeDelivery,
    attachments,
    bootstrap.host.id,
    draftStore,
    isWorking,
    prompt,
    session.sessionId,
  ]);

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
    const caret = pendingComposerCaretRef.current;
    if (caret !== null) {
      pendingComposerCaretRef.current = null;
      textarea.focus();
      textarea.setSelectionRange(caret, caret);
    }
  }, [prompt]);

  const submit = async () => {
    if (!canSubmit || submitting) return;
    const value = prompt;
    const goalCommand = parseGoalCommand(value);
    if (
      !goalCommand &&
      isWorking &&
      ((activeDelivery === "steer" && !bootstrap.capabilities.steer) ||
        (activeDelivery === "followUp" && !bootstrap.capabilities.followUp))
    ) {
      return;
    }
    const invocation = value.trim();
    const isSlashCommand = isSlashCommandInput(value);
    if (isSlashCommand && !goalCommand) {
      const commandName = slashCommandName(value);
      const hasPromptContext =
        uploadedAttachments.length > 0 ||
        documents.length > 0 ||
        projectFiles.length > 0;
      if (hasPromptContext) {
        setSubmitError({
          kind: "rejected",
          title: "Slash commands cannot include prompt context",
          message:
            "Remove attachments, documents, and referenced files before running a slash command.",
          retryable: false,
        });
        return;
      }
      const exactLocalInvocation = invocation === `/${commandName}`;
      const handlesStatus =
        exactLocalInvocation && commandName === "status" && onOpenRuntimeStatus;
      const handlesExport =
        exactLocalInvocation &&
        commandName === "export" &&
        onExportSession &&
        bootstrap.capabilities.sessionExport;
      const handlesFork =
        exactLocalInvocation &&
        commandName === "fork" &&
        onForkSession &&
        bootstrap.capabilities.conversationBranching &&
        Boolean(session.branches.head);
      const localCommand = ["export", "fork", "status"].includes(commandName);
      if (
        localCommand &&
        exactLocalInvocation &&
        !handlesStatus &&
        !handlesExport &&
        !handlesFork
      ) {
        setSubmitError({
          kind: "rejected",
          title: "Slash command unavailable",
          message: "This slash command is not available from the connected host.",
          retryable: false,
        });
        return;
      }
      if (isWorking && !handlesStatus) {
        setSubmitError({
          kind: "session",
          title: "Session is still working",
          message: "Slash commands are available after current work finishes.",
          retryable: true,
        });
        return;
      }
      if (
        !handlesStatus &&
        !handlesExport &&
        !handlesFork &&
        !onInvokeSlashCommand
      ) {
        setSubmitError({
          kind: "rejected",
          title: "Slash command unavailable",
          message: "This slash command is not available from the connected host.",
          retryable: false,
        });
        return;
      }
      setSubmitting(true);
      setSubmitError(null);
      try {
        if (handlesExport) {
          await onExportSession();
        } else if (handlesFork) {
          await onForkSession();
        } else if (handlesStatus) {
          onOpenRuntimeStatus();
        } else if (onInvokeSlashCommand) {
          let pendingSlashCommand = pendingSlashCommandRef.current;
          if (pendingSlashCommand?.invocation !== invocation) {
            pendingSlashCommand = {
              id: crypto.randomUUID(),
              invocation,
            };
            pendingSlashCommandRef.current = pendingSlashCommand;
          }
          await onInvokeSlashCommand(invocation, pendingSlashCommand.id);
          pendingSlashCommandRef.current = null;
        } else {
          throw new Error("This slash command is not available.");
        }
        commandDiscoveryRequestRef.current += 1;
        setCommandDiscovery(null);
        setCommandDiscoveryError(null);
        setCommandDiscoveryLoading(false);
        setPrompt((current) => (current === value ? "" : current));
        setComposerCaret(0);
        setDismissedCompletionKey(null);
        if (promptRef.current === value) {
          draftStore.clear(bootstrap.host.id, session.sessionId);
        }
      } catch (error) {
        setSubmitError(classifySubmissionFailure(error));
      } finally {
        setSubmitting(false);
      }
      return;
    }

    const submittedAttachments = attachments;
    const submittedReferences = uploadedAttachments;
    if (goalCommand) {
      if (
        submittedReferences.length > 0 ||
        documents.length > 0 ||
        projectFiles.length > 0
      ) {
        setSubmitError({
          kind: "rejected",
          title: "Goal command cannot include context",
          message: "Remove attachments and selected context before using /goal.",
          retryable: false,
        });
        return;
      }
      if (!onGoalCommand) {
        setSubmitError({
          kind: "rejected",
          title: "Goal commands are unavailable",
          message: "This session does not support the /goal extension.",
          retryable: false,
        });
        return;
      }
      setSubmitting(true);
      setSubmitError(null);
      setCommandFeedback(null);
      try {
        const feedback = await onGoalCommand(goalCommand);
        setPrompt((current) => (current === value ? "" : current));
        pendingCommandRef.current = null;
        draftStore.clear(bootstrap.host.id, session.sessionId);
        setCommandFeedback(feedback);
      } catch (error) {
        setSubmitError(classifySubmissionFailure(error));
      } finally {
        setSubmitting(false);
      }
      return;
    }
    const commandSignature = JSON.stringify({
      text: value,
      delivery: isWorking ? activeDelivery : "submit",
      attachments: submittedReferences.map((attachment) => attachment.id),
      documents: documents.map((document) => document.id),
      projectFiles: projectFiles.map((file) => file.id),
    });
    if (pendingCommandRef.current?.signature !== commandSignature) {
      pendingCommandRef.current = {
        id: crypto.randomUUID(),
        signature: commandSignature,
      };
    }
    const idempotencyKey = pendingCommandRef.current.id;
    setSubmitting(true);
    setSubmitError(null);
    setCommandFeedback(null);
    try {
      await onSubmit(
        value,
        submittedReferences,
        isWorking ? activeDelivery : undefined,
        idempotencyKey,
        documents,
        projectFiles,
      );
      pendingCommandRef.current = null;
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
      setDocuments([]);
      setProjectFiles([]);
      if (
        promptRef.current === value &&
        attachmentsRef.current.every((attachment) =>
          submittedIds.has(attachment.localId),
        )
      ) {
        draftStore.clear(bootstrap.host.id, session.sessionId);
      }
    } catch (error) {
      setSubmitError(classifySubmissionFailure(error));
    } finally {
      setSubmitting(false);
    }
  };

  const replaceComposerText = (value: string, caret: number) => {
    pendingComposerCaretRef.current = caret;
    setPrompt(value);
    setComposerCaret(caret);
    setDismissedCompletionKey(completionTrigger(value, caret)?.key ?? null);
  };

  const selectCompletion = (option: ComposerOption) => {
    if (option.disabled) return;
    const { action } = option;
    if (action.type === "file") {
      setProjectFiles((current) =>
        current.some((file) => file.id === action.file.id) || current.length >= 20
          ? current
          : [...current, action.file],
      );
      const value = `${prompt.slice(0, action.start)}${prompt.slice(action.end)}`;
      replaceComposerText(value, action.start);
      return;
    }
    if (action.type === "command") {
      const value = `/${action.command.name}${
        action.command.acceptsArgument ? " " : ""
      }`;
      replaceComposerText(value, value.length);
      return;
    }
    if (action.type === "skillSubcommand") {
      const value = `/skills ${action.name}${action.acceptsArgument ? " " : ""}`;
      replaceComposerText(value, value.length);
      return;
    }
    if (action.type === "skill") {
      const value = `${prompt.slice(0, action.start)}${action.name}${prompt.slice(
        action.end,
      )}`;
      replaceComposerText(value, action.start + action.name.length);
      return;
    }
    const value = `/${action.name}`;
    replaceComposerText(value, value.length);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.nativeEvent.isComposing) return;
    if (activeCompletion) {
      const enabledIndexes = completionOptions.flatMap((option, index) =>
        option.disabled ? [] : [index],
      );
      if (event.key === "Escape") {
        event.preventDefault();
        setDismissedCompletionKey(activeCompletion.key);
        return;
      }
      if (event.key === "Tab" && activeCompletion.kind === "commands") {
        if (enabledIndexes.length === 1) {
          event.preventDefault();
          selectCompletion(completionOptions[enabledIndexes[0]!]!);
        }
        return;
      }
      if (
        ["ArrowDown", "ArrowUp", "Home", "End", "PageDown", "PageUp"].includes(
          event.key,
        ) &&
        enabledIndexes.length
      ) {
        event.preventDefault();
        const selected = enabledIndexes.indexOf(activeCompletionIndex);
        const current =
          selected >= 0
            ? selected
            : ["ArrowUp", "PageUp", "End"].includes(event.key)
              ? enabledIndexes.length
              : -1;
        const next =
          event.key === "ArrowDown"
            ? Math.min(enabledIndexes.length - 1, current + 1)
            : event.key === "ArrowUp"
              ? Math.max(0, current - 1)
              : event.key === "Home"
                ? 0
                : event.key === "End"
                  ? enabledIndexes.length - 1
                  : event.key === "PageDown"
                    ? Math.min(enabledIndexes.length - 1, current + 5)
                    : Math.max(0, current - 5);
        setActiveCompletionIndex(enabledIndexes[next]!);
        return;
      }
      if (event.key === "Enter" && !event.shiftKey) {
        if (completionLoading) {
          event.preventDefault();
          return;
        }
        const option = completionOptions[activeCompletionIndex];
        if (option && !option.disabled) {
          const invokeExactCommand =
            activeCompletion.kind === "commands" &&
            ((option.action.type === "command" &&
              option.action.command.name === activeCompletion.query &&
              !option.action.command.acceptsArgument) ||
              (option.action.type === "direct" &&
                option.action.name === activeCompletion.query));
          event.preventDefault();
          if (invokeExactCommand) {
            setDismissedCompletionKey(activeCompletion.key);
            void submit();
          } else {
            selectCompletion(option);
          }
          return;
        }
      }
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  };

  const uploadAttachment = async (attachment: DraftAttachment) => {
    if (!onIngestAttachment || !attachment.file) return;
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
      (total, attachment) => total + draftAttachmentSize(attachment),
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

  const uploadDocument = async (file: File) => {
    if (!onIngestDocument || !documentsAvailable) {
      throw new Error("Document ingest is not available.");
    }
    if (documents.length >= 8) {
      throw new Error("You can attach up to 8 documents.");
    }
    if (file.size === 0 || file.size > 16 * 1024 * 1024) {
      throw new Error(`${file.name} exceeds the document upload limit.`);
    }
    const reference = await onIngestDocument(file);
    setDocuments((current) =>
      current.some((document) => document.id === reference.id)
        ? current
        : [...current, reference],
    );
    return reference;
  };

  const toggleProjectFile = (file: TrustedFileEntry) => {
    setProjectFiles((current) =>
      current.some((entry) => entry.id === file.id)
        ? current.filter((entry) => entry.id !== file.id)
        : current.length >= 20
          ? current
          : [...current, file],
    );
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
        {isModelWorking ? (
          <span className="composer-running-edge" aria-hidden="true">
            <svg preserveAspectRatio="none">
              <rect
                className="composer-running-edge-base"
                x="1"
                y="1"
                width="calc(100% - 2px)"
                height="calc(100% - 2px)"
                rx="10"
                pathLength="100"
              />
              <rect
                className="composer-running-edge-chase"
                x="1"
                y="1"
                width="calc(100% - 2px)"
                height="calc(100% - 2px)"
                rx="10"
                pathLength="100"
              />
            </svg>
          </span>
        ) : null}
        {draggingFiles ? (
          <div className="attachment-drop-overlay" aria-hidden="true">
            <Paperclip />
            <strong>Drop files to attach</strong>
          </div>
        ) : null}
        {goal ? (
          <div className="composer-goal-status">
            <GoalBadge goal={goal} working={isWorking} compact />
            <span>{goal.objective}</span>
          </div>
        ) : null}
        {attachments.length ? (
          <div className="composer-attachments" aria-label="Attached files">
            {attachments.map((attachment) => {
              const name = draftAttachmentName(attachment);
              const restoredPreview =
                !attachment.previewUrl &&
                attachment.reference?.handle &&
                draftAttachmentMediaType(attachment).startsWith("image/")
                  ? attachmentContentUrl?.(attachment.reference.handle)
                  : undefined;
              const previewUrl = attachment.previewUrl ?? restoredPreview;
              return (
                <div
                  className={`composer-attachment is-${attachment.status}`}
                  key={attachment.localId}
                >
                  {previewUrl ? (
                  <button
                    className="composer-attachment-preview"
                    onClick={(event) =>
                      onPreviewAttachment(
                        previewUrl,
                        name,
                        event.currentTarget,
                      )
                    }
                    aria-label={`Click to preview ${name}`}
                  >
                    <img src={previewUrl} alt="" />
                  </button>
                ) : (
                  <span className="attachment-extension" aria-hidden="true">
                    {extensionLabel(name)}
                  </span>
                )}
                <span className="composer-attachment-copy">
                  <strong>{name}</strong>
                  <small aria-live="polite" aria-atomic="true">
                    {attachment.status === "uploading"
                      ? "Uploading…"
                      : attachment.status === "failed"
                        ? attachment.error ?? "Upload failed"
                        : "Ready"}
                  </small>
                </span>
                {attachment.status === "failed" && attachment.file ? (
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
                    aria-label={`Retry ${name}`}
                  >
                    <RefreshCw aria-hidden="true" />
                  </button>
                ) : null}
                <button
                  className="attachment-remove"
                  onClick={() => removeAttachment(attachment.localId)}
                  aria-label={`Remove ${name}`}
                >
                  <X aria-hidden="true" />
                </button>
                </div>
              );
            })}
          </div>
        ) : null}
        {projectFiles.length ? (
          <div
            className="composer-project-files"
            aria-label="Referenced trusted project files"
          >
            {projectFiles.map((file) => (
              <span key={file.id}>
                {trustedFileIcon(file.kind)}
                <strong>{file.relativePath}</strong>
                <button
                  type="button"
                  aria-label={`Remove referenced ${file.relativePath}`}
                  onClick={() =>
                    setProjectFiles((current) =>
                      current.filter((candidate) => candidate.id !== file.id),
                    )
                  }
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
          onChange={(event) => {
            setPrompt(event.target.value);
            setComposerCaret(event.target.selectionStart);
            setDismissedCompletionKey(null);
          }}
          onSelect={(event) =>
            setComposerCaret(event.currentTarget.selectionStart)
          }
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
                : "Describe a change…"
          }
          rows={1}
          aria-label="Message ygg"
          aria-describedby={submitError ? "composer-send-error" : undefined}
          aria-expanded={Boolean(activeCompletion)}
          aria-haspopup={activeCompletion ? "listbox" : undefined}
          aria-controls={
            activeCompletion ? "composer-completion-list" : undefined
          }
          aria-activedescendant={
            activeCompletion && completionOptions[activeCompletionIndex]
              ? `composer-completion-${completionOptions[activeCompletionIndex]!.id}`
              : undefined
          }
        />
        {activeCompletion ? (
          <ComposerCompletion
            label={completionLabel}
            heading={completionLabel}
            options={completionOptions}
            activeIndex={activeCompletionIndex}
            loading={completionLoading}
            error={completionError}
            emptyLabel={
              activeCompletion.kind === "files"
                ? "No trusted files match this reference."
                : activeCompletion.kind === "skills"
                  ? "No skills match this name."
                  : "No matching commands."
            }
            onSelect={(option) => selectCompletion(option as ComposerOption)}
          />
        ) : null}
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
            <PromptContextPicker
              documents={documents}
              projectFiles={projectFiles}
              documentsAvailable={documentsAvailable}
              projectFilesAvailable={projectFilesAvailable}
              onUploadDocument={uploadDocument}
              onRemoveDocument={(documentId) =>
                setDocuments((current) =>
                  current.filter((document) => document.id !== documentId),
                )
              }
              onToggleProjectFile={toggleProjectFile}
              onListProjectFiles={() =>
                onListProjectFiles?.() ??
                Promise.reject(
                  new Error("Project-file browsing is not available."),
                )
              }
              onSearchProjectFiles={(query) =>
                onSearchProjectFiles?.(query) ??
                Promise.reject(
                  new Error("Project-file search is not available."),
                )
              }
              onReadProjectFile={(entryId) =>
                onReadProjectFile?.(entryId) ??
                Promise.reject(
                  new Error("Project-file preview is not available."),
                )
              }
            />
            <ModelPicker
              models={bootstrap.models}
              value={session.modelId}
              reasoningOptions={reasoningOptions}
              reasoning={session.reasoning}
              disabled={isWorking}
              hasStagedImages={hasStagedImages}
              onChange={(modelId) => void onConfigure({ modelId })}
              onReasoningChange={(reasoning) =>
                void onConfigure({ reasoning })
              }
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
            <span
              className="composer-context-cost"
              aria-label={contextDescription}
              title={contextDescription}
            >
              <span>{session.contextPercent}%</span>
              {contextCost ? (
                <>
                  <span aria-hidden="true">·</span>
                  <strong>~{contextCost}</strong>
                </>
              ) : null}
            </span>
            {isWorking ? (
              <button
                className="submit-button stop-button"
                onClick={() => void onInterrupt()}
                aria-label="Stop ygg"
              >
                <span className="stop-glyph" aria-hidden="true" />
              </button>
            ) : null}
            {!isWorking || canSubmit ? (
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
            ) : null}
          </div>
        </div>
        {commandFeedback ? (
          <p className="composer-command-feedback" role="status">
            {commandFeedback}
          </p>
        ) : null}
        {submitError ? (
          <div
            id="composer-send-error"
            className="composer-recovery"
            role="alert"
            data-kind={submitError.kind}
          >
            <span>
              <strong>{submitError.title}</strong>
              <small>{submitError.message}</small>
            </span>
            <div>
              {submitError.retryable ? (
                <button
                  type="button"
                  disabled={submitting || !canSubmit}
                  onClick={() => void submit()}
                >
                  Retry
                </button>
              ) : null}
              <button
                type="button"
                disabled={submitting}
                onClick={() => {
                  pendingCommandRef.current = null;
                  pendingSlashCommandRef.current = null;
                  setSubmitError(null);
                }}
              >
                Cancel retry
              </button>
            </div>
          </div>
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

const MemoizedComposer = memo(Composer, (previous, next) => {
  const previousSession = previous.session;
  const nextSession = next.session;
  return (
    previousSession.sessionId === nextSession.sessionId &&
    previousSession.status === nextSession.status &&
    previousSession.activeRunId === nextSession.activeRunId &&
    previousSession.modelId === nextSession.modelId &&
    previousSession.reasoning === nextSession.reasoning &&
    previousSession.authority === nextSession.authority &&
    previousSession.contextTokens === nextSession.contextTokens &&
    previousSession.contextPercent === nextSession.contextPercent &&
    previousSession.branches.head === nextSession.branches.head &&
    previousSession.items.length === nextSession.items.length &&
    previous.goal === next.goal &&
    previous.bootstrap === next.bootstrap &&
    previous.onSubmit === next.onSubmit &&
    previous.onGoalCommand === next.onGoalCommand &&
    previous.onInterrupt === next.onInterrupt &&
    previous.onConfigure === next.onConfigure &&
    previous.onIngestAttachment === next.onIngestAttachment &&
    previous.onIngestDocument === next.onIngestDocument &&
    previous.onListProjectFiles === next.onListProjectFiles &&
    previous.onSearchProjectFiles === next.onSearchProjectFiles &&
    previous.onReadProjectFile === next.onReadProjectFile &&
    previous.onGetCommandDiscovery === next.onGetCommandDiscovery &&
    previous.onInvokeSlashCommand === next.onInvokeSlashCommand &&
    previous.onExportSession === next.onExportSession &&
    previous.onForkSession === next.onForkSession &&
    previous.onOpenRuntimeStatus === next.onOpenRuntimeStatus &&
    previous.onPreviewAttachment === next.onPreviewAttachment
  );
});

export function Conversation({
  session,
  bootstrap,
  goal,
  onGoalCommand,
  onSubmit,
  onInterrupt,
  onConfigure,
  onResolveApproval,
  onResolveUserInput,
  onOpenOutput,
  onOpenSource,
  onOpenResource,
  resourceContentUrl,
  onIngestAttachment,
  onIngestDocument,
  onListProjectFiles,
  onSearchProjectFiles,
  onReadProjectFile,
  onGetCommandDiscovery,
  onInvokeSlashCommand,
  onExportSession,
  onForkSession,
  onOpenRuntimeStatus,
  onEditUserTurn,
  onRetryResponse,
  onForkConversation,
  attachmentContentUrl,
}: ConversationProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [showJump, setShowJump] = useState(false);
  const [attachmentPreview, setAttachmentPreview] = useState<{
    source: string;
    name: string;
    trigger: HTMLElement;
  } | null>(null);
  const [branchAction, setBranchAction] =
    useState<ConversationBranchAction | null>(null);
  const [initialItemIds] = useState(
    () => new Set(session.items.map((item) => item.id)),
  );
  const shouldStickRef = useRef(true);
  const scrollFrameRef = useRef<number | null>(null);
  const resourcesAvailable = bootstrap.capabilities.resources;
  const availableOutputs = useMemo(
    () =>
      new Map(
        (resourcesAvailable ? session.outputs : []).map((output) => [
          output.id,
          output,
        ]),
      ),
    [resourcesAvailable, session.outputs],
  );
  const availableSources = useMemo(
    () =>
      new Map(
        (resourcesAvailable ? session.sources : []).map((source) => [
          source.id,
          source,
        ]),
      ),
    [resourcesAvailable, session.sources],
  );
  const outputsByOrigin = useMemo(() => {
    const byOrigin = new Map<string, OutputRef[]>();
    for (const output of availableOutputs.values()) {
      if (!output.originItemId) continue;
      const current = byOrigin.get(output.originItemId) ?? [];
      current.push(output);
      byOrigin.set(output.originItemId, current);
    }
    return byOrigin;
  }, [availableOutputs]);
  const sourcesByOrigin = useMemo(() => {
    const byOrigin = new Map<string, SourceRef[]>();
    for (const source of availableSources.values()) {
      if (!source.originItemId) continue;
      const current = byOrigin.get(source.originItemId) ?? [];
      current.push(source);
      byOrigin.set(source.originItemId, current);
    }
    return byOrigin;
  }, [availableSources]);
  const rows = useMemo(() => transcriptRows(session.items), [session.items]);
  const activeWorkRowId = useMemo(
    () => latestActiveWorkRowId(rows, session.status, session.activeRunId),
    [rows, session.status, session.activeRunId],
  );
  const previewAttachment = useCallback(
    (source: string, name: string, trigger: HTMLElement) => {
      setAttachmentPreview({ source, name, trigger });
    },
    [],
  );
  const requestEditUserTurn = useCallback(
    (item: Extract<TranscriptItem, { kind: "user_message" }>) => {
      if (!item.durableEntryId) return;
      setBranchAction({
        kind: "edit",
        item,
        entryId: item.durableEntryId,
      });
    },
    [],
  );
  const requestRetryResponse = useCallback(
    (
      item: Extract<TranscriptItem, { kind: "assistant_message" }>,
      withModel: boolean,
    ) => {
      if (!item.durableEntryId) return;
      setBranchAction({
        kind: "retry",
        entryId: item.durableEntryId,
        withModel,
      });
    },
    [],
  );
  const requestForkConversation = useCallback((item: TranscriptItem) => {
    if (!item.durableEntryId) return;
    setBranchAction({ kind: "fork", entryId: item.durableEntryId });
  }, []);
  const selectedModel = bootstrap.models.find(
    (model) => model.id === session.modelId,
  );
  const conversationBranching =
    bootstrap.capabilities.conversationBranching &&
    Boolean(onEditUserTurn && onRetryResponse && onForkConversation);

  useEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    shouldStickRef.current = true;
    setShowJump(false);
    const onScroll = () => {
      const distance =
        element.scrollHeight - element.scrollTop - element.clientHeight;
      shouldStickRef.current = distance < 24;
      setShowJump(distance >= 96);
    };
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) shouldStickRef.current = false;
    };
    const onTouchStart = () => {
      shouldStickRef.current = false;
    };
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      const target = event.target;
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.isContentEditable)
      ) {
        return;
      }
      if (
        event.key === "ArrowUp" ||
        event.key === "PageUp" ||
        event.key === "Home"
      ) {
        shouldStickRef.current = false;
      }
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    element.addEventListener("wheel", onWheel, { passive: true });
    element.addEventListener("touchstart", onTouchStart, { passive: true });
    document.addEventListener("keydown", onKeyDown);
    return () => {
      element.removeEventListener("scroll", onScroll);
      element.removeEventListener("wheel", onWheel);
      element.removeEventListener("touchstart", onTouchStart);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [session.sessionId]);

  const scheduleFollowToBottom = useCallback(() => {
    if (scrollFrameRef.current !== null) return;
    scrollFrameRef.current = window.requestAnimationFrame(() => {
      scrollFrameRef.current = null;
      const element = scrollRef.current;
      if (!element || !shouldStickRef.current) return;
      element.scrollTop = element.scrollHeight;
    });
  }, []);

  useLayoutEffect(() => {
    scheduleFollowToBottom();
  }, [scheduleFollowToBottom, session.items, session.sessionId]);

  useEffect(() => {
    const element = scrollRef.current;
    const transcript = element?.firstElementChild;
    if (
      !element ||
      !(transcript instanceof HTMLElement) ||
      typeof ResizeObserver === "undefined"
    ) {
      return;
    }
    const observer = new ResizeObserver(() => {
      if (!shouldStickRef.current) return;
      scheduleFollowToBottom();
    });
    observer.observe(element);
    observer.observe(transcript);
    return () => {
      observer.disconnect();
    };
  }, [scheduleFollowToBottom, session.sessionId]);

  useEffect(
    () => () => {
      if (scrollFrameRef.current !== null) {
        window.cancelAnimationFrame(scrollFrameRef.current);
      }
    },
    [],
  );

  const jumpToLive = () => {
    const element = scrollRef.current;
    if (!element) return;
    shouldStickRef.current = true;
    setShowJump(false);
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
          data-item-count={session.items.length}
          data-session-sequence={session.sequence}
        >
          {session.items.length === 0 ? (
            <EmptySession
              key={session.sessionId}
              attachments={
                bootstrap.capabilities.attachments &&
                bootstrap.capabilities.attachmentIngest &&
                Boolean(bootstrap.capabilities.attachmentPolicy) &&
                Boolean(
                  selectedModel?.inputModalities.includes("image"),
                )
              }
            />
          ) : (
            rows.map((row) =>
              row.kind === "work" ? (
                <WorkGroup
                  key={row.id}
                  row={row}
                  active={row.id === activeWorkRowId}
                  initialItemIds={initialItemIds}
                  sessionId={session.sessionId}
                  resourceContentUrl={resourceContentUrl}
                  onResolveApproval={onResolveApproval}
                  onResolveUserInput={onResolveUserInput}
                  onOpenOutput={onOpenOutput}
                  onOpenSource={onOpenSource}
                  onOpenResource={onOpenResource}
                  availableOutputs={availableOutputs}
                  availableSources={availableSources}
                  outputsByOrigin={outputsByOrigin}
                  sourcesByOrigin={sourcesByOrigin}
                  attachmentContentUrl={attachmentContentUrl}
                  onPreviewAttachment={previewAttachment}
                />
              ) : (
                <AnchoredTranscriptItem
                  key={row.item.id}
                  item={row.item}
                  animate={!initialItemIds.has(row.item.id)}
                  onResolveApproval={onResolveApproval}
                  onResolveUserInput={onResolveUserInput}
                  onOpenOutput={onOpenOutput}
                  onOpenSource={onOpenSource}
                  onOpenResource={onOpenResource}
                  availableOutputs={availableOutputs}
                  availableSources={availableSources}
                  outputsByOrigin={outputsByOrigin}
                  sourcesByOrigin={sourcesByOrigin}
                  attachmentContentUrl={attachmentContentUrl}
                  onPreviewAttachment={previewAttachment}
                  conversationBranching={conversationBranching}
                  onEditUserTurn={requestEditUserTurn}
                  onRetryResponse={(item) =>
                    requestRetryResponse(item, false)
                  }
                  onRetryResponseWithModel={(item) =>
                    requestRetryResponse(item, true)
                  }
                  onForkConversation={requestForkConversation}
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
      <MemoizedComposer
        key={session.sessionId}
        session={session}
        bootstrap={bootstrap}
        goal={goal}
        onGoalCommand={onGoalCommand}
        onSubmit={onSubmit}
        onInterrupt={onInterrupt}
        onConfigure={onConfigure}
        onIngestAttachment={onIngestAttachment}
        onIngestDocument={onIngestDocument}
        onListProjectFiles={onListProjectFiles}
        onSearchProjectFiles={onSearchProjectFiles}
        onReadProjectFile={onReadProjectFile}
        onGetCommandDiscovery={onGetCommandDiscovery}
        onInvokeSlashCommand={onInvokeSlashCommand}
        onExportSession={onExportSession}
        onForkSession={onForkSession}
        onOpenRuntimeStatus={onOpenRuntimeStatus}
        attachmentContentUrl={attachmentContentUrl}
        onPreviewAttachment={previewAttachment}
      />
      {attachmentPreview ? (
        <AttachmentPreviewDialog
          key={attachmentPreview.source}
          source={attachmentPreview.source}
          name={attachmentPreview.name}
          onClose={closeAttachmentPreview}
        />
      ) : null}
      {branchAction &&
      onEditUserTurn &&
      onRetryResponse &&
      onForkConversation ? (
        <ConversationBranchDialog
          action={branchAction}
          models={bootstrap.models}
          currentModelId={session.modelId}
          currentReasoning={session.reasoning}
          onEdit={onEditUserTurn}
          onRetry={onRetryResponse}
          onFork={onForkConversation}
          onClose={() => setBranchAction(null)}
        />
      ) : null}
    </section>
  );
}
