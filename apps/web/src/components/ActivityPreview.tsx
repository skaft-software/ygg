import { FileDiff, FileText, LoaderCircle } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { SourceRef } from "../protocol";

export type ResourceContentUrl = (sessionId: string, handle: string) => string;

type ResourceState =
  | { state: "idle" }
  | { state: "loading" }
  | { state: "ready"; text: string }
  | { state: "unavailable"; message: string };

const MAX_PREVIEW_BYTES = 8 * 1024 * 1024;
const MAX_RENDERED_LINES = 1_200;

function useResourceText(
  url: string | undefined,
  enabled: boolean,
  sessionId: string,
  handle: string | undefined,
): ResourceState {
  const [preview, setPreview] = useState<{
    url: string;
    state: ResourceState;
  } | null>(null);
  const requestHandle = enabled && url && handle ? handle : undefined;
  const requestUrl = requestHandle ? url : undefined;

  useEffect(() => {
    if (!requestUrl || !requestHandle) return;

    const controller = new AbortController();
    void (async () => {
      try {
        const response = await fetch(
          `/api/v1/sessions/${encodeURIComponent(sessionId)}/resources/${encodeURIComponent(requestHandle)}`,
          {
            credentials: "same-origin",
            cache: "no-store",
            redirect: "error",
            signal: controller.signal,
            headers: {
              Accept: "text/plain, application/octet-stream;q=0.8",
            },
          },
        );
        if (!response.ok) {
          if (controller.signal.aborted) return;
          setPreview({
            url: requestUrl,
            state: {
              state: "unavailable",
              message:
                response.status === 404
                  ? "This preview is no longer available."
                  : "This preview could not be loaded.",
            },
          });
          return;
        }
        const declaredLength = Number(response.headers.get("content-length"));
        if (Number.isFinite(declaredLength) && declaredLength > MAX_PREVIEW_BYTES) {
          if (controller.signal.aborted) return;
          setPreview({
            url: requestUrl,
            state: {
              state: "unavailable",
              message: "This file is too large to preview.",
            },
          });
          return;
        }
        const text = await response.text();
        if (new TextEncoder().encode(text).byteLength > MAX_PREVIEW_BYTES) {
          if (controller.signal.aborted) return;
          setPreview({
            url: requestUrl,
            state: {
              state: "unavailable",
              message: "This file is too large to preview.",
            },
          });
          return;
        }
        if (text.includes("\0")) {
          if (controller.signal.aborted) return;
          setPreview({
            url: requestUrl,
            state: {
              state: "unavailable",
              message: "This binary file is available to open separately.",
            },
          });
          return;
        }
        if (!controller.signal.aborted) {
          setPreview({ url: requestUrl, state: { state: "ready", text } });
        }
      } catch {
        if (!controller.signal.aborted) {
          setPreview({
            url: requestUrl,
            state: {
              state: "unavailable",
              message: "The preview connection was interrupted.",
            },
          });
        }
      }
    })();
    return () => controller.abort();
  }, [requestHandle, requestUrl, sessionId]);

  if (!requestUrl) return { state: "idle" };
  if (!preview || preview.url !== requestUrl) return { state: "loading" };
  return preview.state;
}

type ShellTokenKind =
  | "command"
  | "string"
  | "variable"
  | "flag"
  | "operator"
  | "comment"
  | "path"
  | "number"
  | "word";

interface ShellToken {
  kind: ShellTokenKind | "whitespace";
  text: string;
}

const shellOperators = [
  "<<<",
  "&&",
  "||",
  ">>",
  "2>",
  "<&",
  ">&",
  "|",
  ";",
  "<",
  ">",
  "(",
  ")",
];

const shellBuiltins = new Set([
  "alias",
  "cd",
  "echo",
  "env",
  "export",
  "printf",
  "pwd",
  "read",
  "set",
  "source",
  "test",
  "unalias",
  "unset",
  "wait",
]);

function shellTokens(command: string): ShellToken[] {
  const tokens: ShellToken[] = [];
  let index = 0;
  let commandPosition = true;

  while (index < command.length) {
    const start = index;
    const character = command[index]!;
    if (/\s/.test(character)) {
      while (index < command.length && /\s/.test(command[index]!)) index += 1;
      tokens.push({ kind: "whitespace", text: command.slice(start, index) });
      continue;
    }

    if (character === "#" && (index === 0 || /\s/.test(command[index - 1]!))) {
      while (index < command.length && command[index] !== "\n") index += 1;
      tokens.push({ kind: "comment", text: command.slice(start, index) });
      commandPosition = true;
      continue;
    }

    if (character === "'" || character === '"' || character === "`") {
      const quote = character;
      index += 1;
      while (index < command.length) {
        if (command[index] === "\\" && quote !== "'") {
          index += 2;
          continue;
        }
        const closed = command[index] === quote;
        index += 1;
        if (closed) break;
      }
      tokens.push({ kind: "string", text: command.slice(start, index) });
      commandPosition = false;
      continue;
    }

    if (character === "$") {
      index += 1;
      if (command[index] === "{") {
        index += 1;
        while (index < command.length && command[index] !== "}") index += 1;
        if (index < command.length) index += 1;
      } else if (/[A-Za-z0-9_?@#$*-]/.test(command[index] ?? "")) {
        index += 1;
        while (index < command.length && /[A-Za-z0-9_]/.test(command[index]!)) {
          index += 1;
        }
      }
      tokens.push({ kind: "variable", text: command.slice(start, index) });
      commandPosition = false;
      continue;
    }

    const operator = shellOperators.find((candidate) =>
      command.startsWith(candidate, index),
    );
    if (operator) {
      index += operator.length;
      tokens.push({ kind: "operator", text: operator });
      commandPosition = operator === "|" || operator === ";" || operator === "&&" || operator === "||";
      continue;
    }

    while (index < command.length) {
      const next = command[index]!;
      if (/\s/.test(next) || shellOperators.some((candidate) => command.startsWith(candidate, index))) {
        break;
      }
      index += 1;
    }
    const text = command.slice(start, index);
    const kind: ShellTokenKind = commandPosition
      ? "command"
      : text.startsWith("-")
        ? "flag"
        : /^\d+(?:\.\d+)?$/.test(text)
          ? "number"
          : text.includes("/") || text.startsWith(".")
            ? "path"
            : shellBuiltins.has(text)
              ? "command"
              : "word";
    tokens.push({ kind, text });
    commandPosition = false;
  }

  return tokens;
}

export function BashLogo() {
  return (
    <span className="bash-logo" aria-hidden="true">
      <span>$_</span>
    </span>
  );
}

export function ShellCommand({ command }: { command: string }) {
  const tokens = useMemo(() => shellTokens(command), [command]);
  return (
    <code className="shell-command" aria-label={`bash: ${command}`}>
      {tokens.map((token, index) => (
        <span
          className={`shell-token shell-token-${token.kind}`}
          key={`${token.kind}-${index}`}
        >
          {token.text}
        </span>
      ))}
    </code>
  );
}

interface DiffLine {
  kind: "addition" | "context" | "deletion" | "hunk" | "meta";
  oldNumber?: number;
  newNumber?: number;
  marker: string;
  content: string;
}

function parseUnifiedDiff(text: string): {
  lines: DiffLine[];
  additions: number;
  deletions: number;
} {
  const lines: DiffLine[] = [];
  let oldNumber = 0;
  let newNumber = 0;
  let inHunk = false;
  let additions = 0;
  let deletions = 0;

  for (const line of text.split("\n")) {
    const coordinates = /^@@ -(?<old>\d+)(?:,\d+)? \+(?<new>\d+)(?:,\d+)? @@/.exec(line);
    if (coordinates?.groups) {
      oldNumber = Number(coordinates.groups.old);
      newNumber = Number(coordinates.groups.new);
      inHunk = true;
      lines.push({ kind: "hunk", marker: "", content: line });
      continue;
    }
    if (
      line.startsWith("diff --git ") ||
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ") ||
      line.startsWith("new file mode ") ||
      line.startsWith("deleted file mode ") ||
      line.startsWith("old mode ") ||
      line.startsWith("new mode ") ||
      line.startsWith("\\ No newline at end of file")
    ) {
      if (line.startsWith("diff --git ")) inHunk = false;
      lines.push({ kind: "meta", marker: "", content: line });
      continue;
    }
    if (line.startsWith("+")) {
      lines.push({
        kind: "addition",
        newNumber: inHunk ? newNumber : undefined,
        marker: "+",
        content: line.slice(1),
      });
      if (inHunk) newNumber += 1;
      additions += 1;
      continue;
    }
    if (line.startsWith("-")) {
      lines.push({
        kind: "deletion",
        oldNumber: inHunk ? oldNumber : undefined,
        marker: "−",
        content: line.slice(1),
      });
      if (inHunk) oldNumber += 1;
      deletions += 1;
      continue;
    }
    const content = inHunk && line.startsWith(" ") ? line.slice(1) : line;
    lines.push({
      kind: "context",
      oldNumber: inHunk ? oldNumber : undefined,
      newNumber: inHunk ? newNumber : undefined,
      marker: inHunk ? " " : "",
      content,
    });
    if (inHunk) {
      oldNumber += 1;
      newNumber += 1;
    }
  }

  return { lines, additions, deletions };
}

function PreviewMessage({ children }: { children: string }) {
  return (
    <div className="activity-preview-state" role="status">
      <LoaderCircle aria-hidden="true" />
      {children}
    </div>
  );
}

export function InlineDiffPreview({
  title,
  url,
  enabled,
  sessionId,
  handle,
}: {
  title: string;
  url?: string;
  enabled: boolean;
  sessionId: string;
  handle: string;
}) {
  const preview = useResourceText(url, enabled, sessionId, handle);
  if (preview.state === "loading") return <PreviewMessage>Loading diff…</PreviewMessage>;
  if (preview.state === "unavailable") {
    return <div className="activity-preview-state" role="status">{preview.message}</div>;
  }
  if (preview.state !== "ready") return null;

  const diff = parseUnifiedDiff(preview.text);
  const renderedLines = diff.lines.slice(0, MAX_RENDERED_LINES);
  return (
    <section className="activity-preview activity-diff-preview" aria-label={`${title} diff`}>
      <header className="activity-preview-heading">
        <FileDiff aria-hidden="true" />
        <code>{title}</code>
        <span className="activity-diff-stats" aria-label={`${diff.additions} added, ${diff.deletions} removed`}>
          <em>+{diff.additions}</em>
          <b>−{diff.deletions}</b>
        </span>
      </header>
      <pre className="activity-diff-code">
        <code>
          {renderedLines.map((line, index) => (
            <span className={`activity-diff-line is-${line.kind}`} key={index}>
              <span className="activity-diff-number" aria-hidden="true">{line.oldNumber ?? ""}</span>
              <span className="activity-diff-number" aria-hidden="true">{line.newNumber ?? ""}</span>
              <span className="activity-diff-marker">{line.marker}</span>
              <span className="activity-diff-content">{line.content || " "}</span>
            </span>
          ))}
        </code>
      </pre>
      {diff.lines.length > MAX_RENDERED_LINES ? (
        <p className="activity-preview-truncation">Showing the first {MAX_RENDERED_LINES.toLocaleString()} lines.</p>
      ) : null}
    </section>
  );
}

function SourceLines({ text }: { text: string }) {
  const lines = text.split(/\r?\n/);
  const renderedLines = lines.slice(0, MAX_RENDERED_LINES);
  const width = String(Math.max(1, lines.length)).length;
  return (
    <pre className="activity-source-code">
      <code>
        {renderedLines.map((line, index) => (
          <span className="activity-source-line" key={index}>
            <span className="activity-source-number" aria-hidden="true">
              {String(index + 1).padStart(width, " ")}
            </span>
            <span className="activity-source-content">{line || " "}</span>
          </span>
        ))}
      </code>
    </pre>
  );
}

export function InlineFilePreview({
  source,
  url,
  enabled,
  sessionId,
}: {
  source: SourceRef;
  url?: string;
  enabled: boolean;
  sessionId: string;
}) {
  const preview = useResourceText(url, enabled, sessionId, source.handle);
  const fallback = source.excerpt;
  const text = preview.state === "ready" ? preview.text : fallback;
  return (
    <section className="activity-preview activity-file-preview" aria-label={`Read ${source.title}`}>
      <header className="activity-preview-heading">
        <FileText aria-hidden="true" />
        <code>{source.title}</code>
        <small>{source.subtitle}</small>
      </header>
      {text ? <SourceLines text={text} /> : null}
      {preview.state === "loading" && !text ? <PreviewMessage>Loading file…</PreviewMessage> : null}
      {preview.state === "unavailable" && !text ? (
        <div className="activity-preview-state" role="status">{preview.message}</div>
      ) : null}
    </section>
  );
}
