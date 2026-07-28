import type { GoalState } from "../protocol";

export interface GoalBadgeProps {
  goal: GoalState | null;
  working?: boolean;
  compact?: boolean;
}

function statusLabel(goal: GoalState, working: boolean): string {
  if (working && goal.status === "active") return "Working";
  switch (goal.status) {
    case "active":
      return "Active";
    case "paused":
      return "Paused";
    case "complete":
      return "Complete";
    case "blocked":
      return "Blocked";
    case "budget_limited":
      return "Budget limited";
  }
}

export function GoalBadge({ goal, working = false, compact = false }: GoalBadgeProps) {
  if (!goal) return null;
  const label = statusLabel(goal, working);
  const remaining =
    goal.turnBudget === null
      ? null
      : Math.max(0, goal.turnBudget - goal.turnsUsed);
  return (
    <span
      className={`goal-badge is-${working && goal.status === "active" ? "working" : goal.status}${compact ? " is-compact" : ""}`}
      data-status={goal.status}
      title={`${goal.objective} · ${label}`}
      aria-label={`Goal ${label}: ${goal.objective}`}
    >
      <span className="goal-badge-indicator" aria-hidden="true" />
      <span className="goal-badge-label">Goal {label}</span>
      {remaining === null ? null : (
        <span className="goal-badge-budget">{remaining} left</span>
      )}
    </span>
  );
}
