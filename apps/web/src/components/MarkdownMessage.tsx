import { Check, Copy } from "lucide-react";
import {
  isValidElement,
  memo,
  type ReactNode,
  useMemo,
  useState,
} from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

type DiffLineKind =
  | "addition"
  | "context"
  | "deletion"
  | "hunk"
  | "meta";

interface DiffLine {
  kind: DiffLineKind;
  oldNumber?: number;
  newNumber?: number;
  marker: string;
  content: string;
}

interface ParsedDiff {
  lines: DiffLine[];
  additions: number;
  deletions: number;
  title: string;
}

function nodeText(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") {
    return String(node);
  }
  if (Array.isArray(node)) return node.map(nodeText).join("");
  if (isValidElement<{ children?: ReactNode }>(node)) {
    return nodeText(node.props.children);
  }
  return "";
}

function diffTitle(lines: readonly string[]): string {
  for (const prefix of ["+++ ", "--- "]) {
    const header = lines.find(
      (line) => line.startsWith(prefix) && !line.includes("/dev/null"),
    );
    if (!header) continue;
    return header
      .slice(prefix.length)
      .split("\t", 1)[0]!
      .replace(/^['"]|['"]$/g, "")
      .replace(/^[ab]\//, "");
  }
  return "diff";
}

function isDiffMetadata(line: string): boolean {
  return [
    "diff --git ",
    "index ",
    "--- ",
    "+++ ",
    "new file mode ",
    "deleted file mode ",
    "old mode ",
    "new mode ",
    "similarity index ",
    "rename from ",
    "rename to ",
    "Binary files ",
    "\\ No newline at end of file",
  ].some((prefix) => line.startsWith(prefix));
}

function parseDiff(code: string): ParsedDiff {
  const sourceLines = code.split("\n");
  const lines: DiffLine[] = [];
  let oldNumber = 0;
  let newNumber = 0;
  let inHunk = false;
  let additions = 0;
  let deletions = 0;

  for (const line of sourceLines) {
    const coordinates =
      /^@@ -(?<old>\d+)(?:,\d+)? \+(?<new>\d+)(?:,\d+)? @@/.exec(
        line,
      );
    if (coordinates?.groups) {
      oldNumber = Number(coordinates.groups.old);
      newNumber = Number(coordinates.groups.new);
      inHunk = true;
      lines.push({ kind: "hunk", marker: "", content: line });
      continue;
    }

    if (isDiffMetadata(line)) {
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

  return {
    lines,
    additions,
    deletions,
    title: diffTitle(sourceLines),
  };
}

function MarkdownDiff({ diff }: { diff: ParsedDiff }) {
  return (
    <pre
      className="markdown-diff"
      aria-label={`Diff with ${diff.additions} lines added and ${diff.deletions} lines removed`}
    >
      <code>
        {diff.lines.map((line, index) => (
          <span
            className={`markdown-diff-line is-${line.kind}`}
            key={index}
          >
            <span className="markdown-diff-number" aria-hidden="true">
              {line.oldNumber ?? ""}
            </span>
            <span className="markdown-diff-number" aria-hidden="true">
              {line.newNumber ?? ""}
            </span>
            <span className="markdown-diff-marker">{line.marker}</span>
            <span className="markdown-diff-content">
              {line.content || " "}
            </span>
          </span>
        ))}
      </code>
    </pre>
  );
}

function MarkdownCodeBlock({ children }: { children?: ReactNode }) {
  const [copied, setCopied] = useState(false);
  const child = Array.isArray(children) ? children[0] : children;
  const code = isValidElement<{ children?: ReactNode }>(child)
    ? nodeText(child.props.children).replace(/\n$/, "")
    : "";
  const className = isValidElement<{ className?: string }>(child)
    ? child.props.className ?? ""
    : "";
  const language = className.match(/language-([\w-]+)/)?.[1] ?? "code";
  const isDiff = /^(?:diff|patch|udiff)$/i.test(language);
  const diff = useMemo(() => (isDiff ? parseDiff(code) : null), [code, isDiff]);

  return (
    <div className={`markdown-code-block ${diff ? "is-diff" : ""}`}>
      <div className="markdown-code-header">
        <span className="markdown-code-language">
          {diff?.title ?? language}
        </span>
        {diff ? (
          <span
            className="markdown-diff-stats"
            aria-label={`${diff.additions} added, ${diff.deletions} removed`}
          >
            <em>+{diff.additions}</em>
            <b>−{diff.deletions}</b>
          </span>
        ) : null}
        <button
          type="button"
          onClick={() => {
            void navigator.clipboard.writeText(code).then(() => {
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1_500);
            });
          }}
          aria-label={copied ? "Code copied" : "Copy code"}
        >
          {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {diff ? <MarkdownDiff diff={diff} /> : <pre>{children}</pre>}
    </div>
  );
}

const markdownComponents: Components = {
  a: ({ children, href }) => (
    <a href={href} target="_blank" rel="noreferrer noopener">
      {children}
    </a>
  ),
  img: ({ alt }) => (
    <span className="markdown-image-placeholder">
      Image{alt ? `: ${alt}` : ""}
    </span>
  ),
  pre: ({ children }) => (
    <MarkdownCodeBlock>{children}</MarkdownCodeBlock>
  ),
};

function MarkdownMessage({ content }: { content: string }) {
  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={markdownComponents}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

export default memo(
  MarkdownMessage,
  (previous, next) => previous.content === next.content,
);
