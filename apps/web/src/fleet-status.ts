import type { SessionSummary } from "./protocol";

export function taskNeedsAttention(session: SessionSummary): boolean {
  return (
    session.attentionCount > 0 ||
    session.status === "needs_attention" ||
    session.status === "failed" ||
    session.status === "disconnected"
  );
}

export function taskNeedsReview(session: SessionSummary): boolean {
  if (session.pullRequest?.state === "merged") return false;
  return (
    session.pullRequest?.state === "ready" ||
    (session.status === "done" && session.unread)
  );
}

export function formatTaskAge(
  updatedAt: string,
  now: number = Date.now(),
): string {
  const timestamp = Date.parse(updatedAt);
  if (!Number.isFinite(timestamp)) return "Unknown";
  const elapsed = Math.max(0, now - timestamp);
  const minutes = Math.floor(elapsed / 60_000);
  if (minutes < 1) return "Now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
  }).format(new Date(timestamp));
}
