import type { AttachmentRef } from "./protocol";

const DRAFT_VERSION = 1;
const MAX_DRAFT_TEXT_BYTES = 256 * 1024;
const MAX_DRAFT_ATTACHMENTS = 32;
const MAX_DRAFT_NAME_CHARS = 512;
const MAX_DRAFT_MEDIA_TYPE_CHARS = 128;
const MAX_DRAFT_HANDLE_CHARS = 512;

export type DraftDelivery = "submit" | "steer" | "followUp";

export interface SessionDraft {
  text: string;
  delivery: DraftDelivery;
  attachments: AttachmentRef[];
  updatedAt: string;
}

interface StoredSessionDraft extends SessionDraft {
  version: typeof DRAFT_VERSION;
}

export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

function draftKey(hostId: string, sessionId: string): string {
  return `ygg.draft.v${DRAFT_VERSION}.${encodeURIComponent(hostId)}.${encodeURIComponent(sessionId)}`;
}

function validBoundedString(
  value: unknown,
  maxChars: number,
  allowEmpty = false,
): value is string {
  if (
    typeof value !== "string" ||
    (!allowEmpty && value.length === 0) ||
    value.length > maxChars
  ) {
    return false;
  }
  return !Array.from(value).some((character) => {
    const code = character.charCodeAt(0);
    return (
      code <= 0x08 ||
      code === 0x0b ||
      code === 0x0c ||
      (code >= 0x0e && code <= 0x1f) ||
      code === 0x7f
    );
  });
}

function validAttachment(value: unknown): value is AttachmentRef {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const candidate = value as Partial<AttachmentRef>;
  return (
    validBoundedString(candidate.id, MAX_DRAFT_HANDLE_CHARS) &&
    (candidate.handle === undefined ||
      validBoundedString(candidate.handle, MAX_DRAFT_HANDLE_CHARS)) &&
    validBoundedString(candidate.name, MAX_DRAFT_NAME_CHARS) &&
    validBoundedString(
      candidate.mediaType,
      MAX_DRAFT_MEDIA_TYPE_CHARS,
    ) &&
    typeof candidate.size === "number" &&
    Number.isSafeInteger(candidate.size) &&
    candidate.size >= 0
  );
}

function parseDraft(value: string): SessionDraft | null {
  if (new TextEncoder().encode(value).byteLength > MAX_DRAFT_TEXT_BYTES * 2) {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return null;
  }
  const candidate = parsed as Partial<StoredSessionDraft>;
  if (
    candidate.version !== DRAFT_VERSION ||
    !validBoundedString(candidate.text, MAX_DRAFT_TEXT_BYTES, true) ||
    new TextEncoder().encode(candidate.text).byteLength >
      MAX_DRAFT_TEXT_BYTES ||
    !["submit", "steer", "followUp"].includes(candidate.delivery ?? "") ||
    !Array.isArray(candidate.attachments) ||
    candidate.attachments.length > MAX_DRAFT_ATTACHMENTS ||
    !candidate.attachments.every(validAttachment) ||
    typeof candidate.updatedAt !== "string" ||
    !Number.isFinite(Date.parse(candidate.updatedAt))
  ) {
    return null;
  }
  return {
    text: candidate.text,
    delivery: candidate.delivery as DraftDelivery,
    attachments: candidate.attachments,
    updatedAt: new Date(candidate.updatedAt).toISOString(),
  };
}

function normalizedDraft(draft: SessionDraft): StoredSessionDraft | null {
  const textBytes = new TextEncoder().encode(draft.text).byteLength;
  const updatedAt = Date.parse(draft.updatedAt);
  if (
    textBytes > MAX_DRAFT_TEXT_BYTES ||
    !["submit", "steer", "followUp"].includes(draft.delivery) ||
    draft.attachments.length > MAX_DRAFT_ATTACHMENTS ||
    !draft.attachments.every(validAttachment) ||
    !Number.isFinite(updatedAt)
  ) {
    return null;
  }
  return {
    version: DRAFT_VERSION,
    text: draft.text,
    delivery: draft.delivery,
    attachments: draft.attachments.map((attachment) => ({ ...attachment })),
    updatedAt: new Date(updatedAt).toISOString(),
  };
}

export class SessionDraftStore {
  private readonly fallback = new Map<string, string>();

  constructor(private readonly storage?: DraftStorage) {}

  load(hostId: string, sessionId: string): SessionDraft | null {
    const key = draftKey(hostId, sessionId);
    let value: string | null;
    try {
      value = this.storage?.getItem(key) ?? this.fallback.get(key) ?? null;
    } catch {
      value = this.fallback.get(key) ?? null;
    }
    if (value === null) return null;
    const draft = parseDraft(value);
    if (draft) return draft;
    this.clear(hostId, sessionId);
    return null;
  }

  save(
    hostId: string,
    sessionId: string,
    draft: SessionDraft,
  ): boolean {
    const normalized = normalizedDraft(draft);
    if (!normalized) return false;
    const key = draftKey(hostId, sessionId);
    const value = JSON.stringify(normalized);
    this.fallback.set(key, value);
    try {
      this.storage?.setItem(key, value);
    } catch {
      // In-memory persistence still preserves drafts for this page lifetime.
    }
    return true;
  }

  clear(hostId: string, sessionId: string): void {
    const key = draftKey(hostId, sessionId);
    this.fallback.delete(key);
    try {
      this.storage?.removeItem(key);
    } catch {
      // A disabled storage backend has nothing durable to clear.
    }
  }
}
