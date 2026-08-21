import type { SessionSnapshot } from "./protocol";

export function resolveDelegatedParentSessionId(
  session: SessionSnapshot | null | undefined,
  inferredParentSessionId: string | null,
): string | null {
  if (!session?.sessionId.startsWith("agent-session:")) {
    return null;
  }
  return session.delegatedParentSessionId ?? inferredParentSessionId;
}
