import { Check, Copy } from "lucide-react";
import { type ReactNode, useState } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

function MarkdownCodeBlock({ children }: { children?: ReactNode }) {
  const [copied, setCopied] = useState(false);
  const child = Array.isArray(children) ? children[0] : children;
  const code =
    child &&
    typeof child === "object" &&
    "props" in child &&
    child.props &&
    typeof child.props === "object" &&
    "children" in child.props
      ? String(child.props.children ?? "").replace(/\n$/, "")
      : "";
  const className =
    child &&
    typeof child === "object" &&
    "props" in child &&
    child.props &&
    typeof child.props === "object" &&
    "className" in child.props &&
    typeof child.props.className === "string"
      ? child.props.className
      : "";
  const language = className.match(/language-([\w-]+)/)?.[1] ?? "code";

  return (
    <div className="markdown-code-block">
      <div className="markdown-code-header">
        <span>{language}</span>
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
      <pre>{children}</pre>
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

export default function MarkdownMessage({ content }: { content: string }) {
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
