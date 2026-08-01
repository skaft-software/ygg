import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, lineNumbers } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useRef, useState } from "react";
import { languageForPath } from "./fileLanguage";

const fileHighlightStyle = HighlightStyle.define([
  { tag: tags.comment, color: "var(--files-syntax-comment)" },
  {
    tag: [
      tags.keyword,
      tags.controlKeyword,
      tags.operatorKeyword,
      tags.definitionKeyword,
      tags.moduleKeyword,
      tags.modifier,
    ],
    color: "var(--files-syntax-keyword)",
  },
  {
    tag: [tags.typeName, tags.className, tags.namespace, tags.macroName],
    color: "var(--files-syntax-type)",
  },
  {
    tag: [
      tags.function(tags.variableName),
      tags.definition(tags.function(tags.variableName)),
      tags.labelName,
    ],
    color: "var(--files-syntax-function)",
  },
  {
    tag: [tags.variableName, tags.propertyName],
    color: "var(--files-syntax-variable)",
  },
  {
    tag: [tags.string, tags.special(tags.string), tags.regexp, tags.escape],
    color: "var(--files-syntax-string)",
  },
  {
    tag: [tags.number, tags.bool, tags.null, tags.atom, tags.unit],
    color: "var(--files-syntax-number)",
  },
  {
    tag: [
      tags.operator,
      tags.operatorKeyword,
      tags.arithmeticOperator,
      tags.logicOperator,
      tags.compareOperator,
      tags.updateOperator,
      tags.definitionOperator,
      tags.typeOperator,
      tags.derefOperator,
    ],
    color: "var(--files-syntax-operator)",
  },
  {
    tag: [
      tags.punctuation,
      tags.bracket,
      tags.angleBracket,
      tags.squareBracket,
      tags.paren,
      tags.separator,
    ],
    color: "var(--files-syntax-punctuation)",
  },
  {
    tag: [tags.meta, tags.documentMeta, tags.annotation, tags.processingInstruction],
    color: "var(--files-syntax-meta)",
  },
  { tag: tags.invalid, color: "var(--files-syntax-invalid)" },
]);

export interface FileCodeEditorProps {
  path: string;
  value: string;
  readOnly: boolean;
  showLineNumbers?: boolean;
  onChange: (value: string) => void;
}

export function FileCodeEditor({
  path,
  value,
  readOnly,
  showLineNumbers = false,
  onChange,
}: FileCodeEditorProps) {
  const highlightRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const [languageCompartment] = useState(() => new Compartment());
  const [initialValue] = useState(value);
  const [languageLoadError, setLanguageLoadError] = useState(false);

  useEffect(() => {
    const parent = highlightRef.current;
    if (!parent) return;

    const view = new EditorView({
      state: EditorState.create({
        doc: initialValue,
        extensions: [
          languageCompartment.of([]),
          ...(showLineNumbers ? [lineNumbers()] : []),
          EditorState.readOnly.of(true),
          EditorView.editable.of(false),
          EditorState.tabSize.of(2),
          syntaxHighlighting(fileHighlightStyle),
          EditorView.contentAttributes.of({ "aria-hidden": "true" }),
        ],
      }),
      parent,
    });
    viewRef.current = view;

    const description = languageForPath(path);
    setLanguageLoadError(false);
    let cancelled = false;
    if (description) {
      void description
        .load()
        .then((support) => {
          if (cancelled || viewRef.current !== view) return;
          view.dispatch({
            effects: languageCompartment.reconfigure(support),
          });
        })
        .catch(() => {
          if (!cancelled) setLanguageLoadError(true);
        });
    }

    return () => {
      cancelled = true;
      viewRef.current = null;
      view.destroy();
    };
  }, [initialValue, languageCompartment, path, showLineNumbers]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view || view.state.doc.toString() === value) return;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value },
    });
  }, [value]);

  return (
    <div
      className={`files-code-editor ${showLineNumbers ? "is-numbered" : ""}`}
      data-language-error={languageLoadError || undefined}
    >
      <div
        ref={highlightRef}
        className="files-code-highlight"
        aria-hidden="true"
      />
      <textarea
        className="files-editor-input"
        aria-label={`Contents of ${path}`}
        value={value}
        readOnly={readOnly}
        spellCheck={false}
        onChange={(event) => onChange(event.target.value)}
        onScroll={(event) => {
          viewRef.current?.scrollDOM.scrollTo({
            top: event.currentTarget.scrollTop,
            left: event.currentTarget.scrollLeft,
          });
        }}
      />
    </div>
  );
}
