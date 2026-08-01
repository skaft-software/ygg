import type { GoalState } from "../../protocol";

export type GoalCommand =
  | { type: "set"; objective: string }
  | { type: "status" }
  | { type: "pause" }
  | { type: "resume" }
  | { type: "clear" }
  | { type: "help" };

export const goalCommandHelp =
  "Use /goal <objective>, /goal status, /goal pause, /goal resume, or /goal clear.";

export function parseGoalCommand(input: string): GoalCommand | null {
  const match = /^\/goal(?:\s+([\s\S]*))?$/iu.exec(input.trim());
  if (!match) return null;
  const argument = match[1]?.trim() ?? "";
  if (!argument) return { type: "help" };
  switch (argument.toLocaleLowerCase()) {
    case "status":
      return { type: "status" };
    case "pause":
      return { type: "pause" };
    case "resume":
      return { type: "resume" };
    case "clear":
      return { type: "clear" };
    default:
      return { type: "set", objective: argument };
  }
}

export function goalStatusLabel(goal: GoalState): string {
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

export function goalStatusMessage(goal: GoalState | null): string {
  if (!goal) return "No goal is configured for this task.";
  const remaining =
    goal.turnBudget === null
      ? ""
      : ` · ${Math.max(0, goal.turnBudget - goal.turnsUsed)} turn${goal.turnBudget - goal.turnsUsed === 1 ? "" : "s"} remaining`;
  return `${goalStatusLabel(goal)} goal: ${goal.objective}${remaining}`;
}
