import type { ReactNode } from "react";

export interface ComposerCompletionOption {
  id: string;
  title: string;
  description?: string;
  meta?: string;
  icon?: ReactNode;
  disabled?: boolean;
}

interface ComposerCompletionProps {
  label: string;
  heading: string;
  options: ComposerCompletionOption[];
  activeIndex: number;
  loading?: boolean;
  error?: string | null;
  emptyLabel: string;
  onSelect: (option: ComposerCompletionOption) => void;
}

/**
 * An input-owned completion list. Focus intentionally remains in the textarea
 * so the composer can retain its normal editing and submit behavior.
 */
export function ComposerCompletion({
  label,
  heading,
  options,
  activeIndex,
  loading = false,
  error,
  emptyLabel,
  onSelect,
}: ComposerCompletionProps) {
  return (
    <div
      id="composer-completion-list"
      className="composer-completion"
      role="listbox"
      aria-label={label}
      aria-busy={loading || undefined}
    >
      <span className="composer-completion-heading">{heading}</span>
      {loading ? (
        <p className="composer-completion-state" role="status">
          Loading…
        </p>
      ) : error ? (
        <p className="composer-completion-state is-error" role="alert">
          {error}
        </p>
      ) : options.length ? (
        <div className="composer-completion-options">
          {options.map((option, index) => (
            <button
              id={`composer-completion-${option.id}`}
              key={option.id}
              type="button"
              role="option"
              aria-selected={index === activeIndex}
              disabled={option.disabled}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onSelect(option)}
            >
              {option.icon ? (
                <span className="composer-completion-icon" aria-hidden="true">
                  {option.icon}
                </span>
              ) : null}
              <span className="composer-completion-copy">
                <strong>{option.title}</strong>
                {option.description ? <small>{option.description}</small> : null}
              </span>
              {option.meta ? (
                <em className="composer-completion-meta">{option.meta}</em>
              ) : null}
            </button>
          ))}
        </div>
      ) : (
        <p className="composer-completion-state">{emptyLabel}</p>
      )}
    </div>
  );
}
