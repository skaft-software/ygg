import { AlertTriangle, GitFork, Pencil, RefreshCw, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type {
  ModelSummary,
  ReasoningEffort,
  UserMessageItem,
} from "../protocol";
import "./ConversationBranchDialog.css";

export type ConversationBranchAction =
  | {
      kind: "edit";
      item: UserMessageItem;
      entryId: string;
    }
  | {
      kind: "retry";
      entryId: string;
      withModel: boolean;
    }
  | {
      kind: "fork";
      entryId: string;
    };

interface ConversationBranchDialogProps {
  action: ConversationBranchAction;
  models: readonly ModelSummary[];
  currentModelId: string;
  currentReasoning: ReasoningEffort;
  onEdit: (entryId: string, text: string) => Promise<void>;
  onRetry: (
    entryId: string,
    model?: { id: string; reasoning: ReasoningEffort },
  ) => Promise<void>;
  onFork: (entryId: string) => Promise<void>;
  onClose: () => void;
}

export function ConversationBranchDialog({
  action,
  models,
  currentModelId,
  currentReasoning,
  onEdit,
  onRetry,
  onFork,
  onClose,
}: ConversationBranchDialogProps) {
  const availableModels = useMemo(
    () => models.filter((model) => model.available),
    [models],
  );
  const [text, setText] = useState(
    action.kind === "edit" ? action.item.content : "",
  );
  const [modelId, setModelId] = useState(currentModelId);
  const selectedModel =
    availableModels.find((model) => model.id === modelId) ??
    availableModels[0];
  const [reasoning, setReasoning] = useState<ReasoningEffort>(() => {
    if (selectedModel?.reasoning.includes(currentReasoning)) {
      return currentReasoning;
    }
    return (
      selectedModel?.defaultReasoning ??
      selectedModel?.reasoning[0] ??
      "off"
    );
  });
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    closeButtonRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || submitting) return;
      event.preventDefault();
      onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose, submitting]);

  const title =
    action.kind === "edit"
      ? "Edit this turn"
      : action.kind === "fork"
        ? "Fork into a new session"
        : action.withModel
          ? "Retry with another model"
          : "Retry this response";
  const submitLabel =
    action.kind === "edit"
      ? "Create edited branch"
      : action.kind === "fork"
        ? "Fork conversation"
        : action.withModel
          ? "Retry with model"
          : "Retry response";
  const Icon =
    action.kind === "edit"
      ? Pencil
      : action.kind === "fork"
        ? GitFork
        : RefreshCw;

  const submit = async () => {
    if (submitting || (action.kind === "edit" && !text.trim())) return;
    setSubmitting(true);
    setError(null);
    try {
      if (action.kind === "edit") {
        await onEdit(action.entryId, text.trim());
      } else if (action.kind === "fork") {
        await onFork(action.entryId);
      } else {
        await onRetry(
          action.entryId,
          action.withModel && selectedModel
            ? { id: selectedModel.id, reasoning }
            : undefined,
        );
      }
      onClose();
    } catch (cause) {
      setError(
        cause instanceof Error
          ? cause.message
          : "ygg could not create this conversation branch.",
      );
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="conversation-branch-backdrop" role="presentation">
      <section
        className="conversation-branch-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="conversation-branch-title"
      >
        <header>
          <span aria-hidden="true">
            <Icon />
          </span>
          <div>
            <small>Conversation branch</small>
            <h2 id="conversation-branch-title">{title}</h2>
          </div>
          <button
            ref={closeButtonRef}
            type="button"
            onClick={onClose}
            disabled={submitting}
            aria-label="Close conversation branch dialog"
          >
            <X aria-hidden="true" />
          </button>
        </header>

        {action.kind === "edit" ? (
          <label className="conversation-branch-field">
            <span>Replacement message</span>
            <textarea
              value={text}
              onChange={(event) => setText(event.target.value)}
              rows={6}
              autoFocus
            />
          </label>
        ) : null}

        {action.kind === "retry" && action.withModel ? (
          <div className="conversation-branch-fields">
            <label className="conversation-branch-field">
              <span>Model</span>
              <select
                value={selectedModel?.id ?? ""}
                onChange={(event) => {
                  const next = availableModels.find(
                    (model) => model.id === event.target.value,
                  );
                  setModelId(event.target.value);
                  if (next) {
                    setReasoning(
                      next.defaultReasoning ?? next.reasoning[0] ?? "off",
                    );
                  }
                }}
              >
                {availableModels.map((model) => (
                  <option key={model.id} value={model.id}>
                    {model.name} · {model.provider}
                  </option>
                ))}
              </select>
            </label>
            <label className="conversation-branch-field">
              <span>Reasoning</span>
              <select
                value={reasoning}
                onChange={(event) => setReasoning(event.target.value)}
              >
                {(selectedModel?.reasoning ?? []).map((effort) => (
                  <option key={effort} value={effort}>
                    {effort}
                  </option>
                ))}
              </select>
            </label>
          </div>
        ) : null}

        <div className="conversation-branch-warning">
          <AlertTriangle aria-hidden="true" />
          <p>
            <strong>External effects are preserved.</strong>
            Files, commands, network requests, and other work already performed
            are not rolled back. This creates a transcript branch only.
          </p>
        </div>

        {error ? <p role="alert">{error}</p> : null}

        <footer>
          <button
            type="button"
            className="secondary-button"
            onClick={onClose}
            disabled={submitting}
          >
            Cancel
          </button>
          <button
            type="button"
            className="primary-button"
            onClick={() => void submit()}
            disabled={
              submitting ||
              (action.kind === "edit" && !text.trim()) ||
              (action.kind === "retry" &&
                action.withModel &&
                !selectedModel)
            }
          >
            {submitLabel}
          </button>
        </footer>
      </section>
    </div>
  );
}
