import {
  Check,
  Clipboard,
  Laptop,
  Link2,
  LoaderCircle,
  Plus,
  ShieldCheck,
  Smartphone,
  Unlink,
  Wifi,
  X,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import type {
  CompanionCatalog,
  CompanionDevice,
  CompanionPairingDecision,
  CompanionPairingInvitation,
  PendingCompanionPairing,
} from "../protocol";
import type { CompanionAdminTransport } from "../transport";

const catalogPollMs = 2_000;

function platformIcon(device: CompanionDevice) {
  return device.platform === "ios" || device.platform === "android" ? (
    <Smartphone aria-hidden="true" />
  ) : (
    <Laptop aria-hidden="true" />
  );
}

function platformLabel(platform: CompanionDevice["platform"]): string {
  switch (platform) {
    case "ios":
      return "iOS";
    case "android":
      return "Android";
    case "macos":
      return "macOS";
    case "other":
      return "Other";
  }
}

function expiresIn(expiresAtMs: number, now: number): string {
  const seconds = Math.max(0, Math.ceil((expiresAtMs - now) / 1_000));
  if (seconds === 0) return "Expired";
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return minutes > 0 ? `${minutes}:${remainder.toString().padStart(2, "0")}` : `${seconds}s`;
}

function lastSeen(device: CompanionDevice): string {
  if (device.revokedAtMs !== undefined) return "Access revoked";
  if (device.connected) return "Connected";
  if (device.lastSeenAtMs === undefined) return "Not connected yet";
  return `Last seen ${new Date(device.lastSeenAtMs).toLocaleString()}`;
}

function publicError(error: unknown): string {
  return error instanceof Error
    ? error.message
    : "The companion operation could not be completed.";
}

function PendingRequest({
  request,
  now,
  busy,
  onDecision,
}: {
  request: PendingCompanionPairing;
  now: number;
  busy: boolean;
  onDecision: (decision: CompanionPairingDecision) => void;
}) {
  const approved = request.state === "approved";
  return (
    <article className="companion-pending-request">
      <div className="device-copy">
        <strong>{request.device.name}</strong>
        <span>
          {platformLabel(request.device.platform)} · app {request.device.appVersion}
        </span>
        <span className="pairing-phrase-inline">{request.phrase}</span>
      </div>
      <span className="device-state">
        {approved
          ? "Approved · waiting for secure-storage confirmation"
          : `Expires in ${expiresIn(request.expiresAtMs, now)}`}
      </span>
      <div className="companion-decision-actions">
        {approved ? (
          <span className="secure-label">
            <Check aria-hidden="true" /> Approved
          </span>
        ) : (
          <>
            <button
              className="primary-button"
              disabled={busy || request.expiresAtMs <= now}
              onClick={() => onDecision("approve")}
            >
              Approve
            </button>
            <button
              className="device-revoke"
              disabled={busy || request.expiresAtMs <= now}
              onClick={() => onDecision("deny")}
            >
              Deny
            </button>
          </>
        )}
      </div>
    </article>
  );
}

export function DevicesView({
  hostName,
  companionAvailable,
  transport,
}: {
  hostName: string;
  companionAvailable: boolean;
  transport: CompanionAdminTransport;
}) {
  const [catalog, setCatalog] = useState<CompanionCatalog | null>(null);
  const [invitation, setInvitation] =
    useState<CompanionPairingInvitation | null>(null);
  const [pairingOpen, setPairingOpen] = useState(false);
  const [loading, setLoading] = useState(companionAvailable);
  const [pairingBusy, setPairingBusy] = useState(false);
  const [busyRequestId, setBusyRequestId] = useState<string | null>(null);
  const [busyDeviceId, setBusyDeviceId] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(0);
  const pairButtonRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLElement>(null);

  const refresh = useCallback(async () => {
    const next = await transport.getCompanionCatalog();
    setCatalog(next);
    return next;
  }, [transport]);

  const cancelPairing = useCallback(async () => {
    if (pairingBusy) return;
    setPairingBusy(true);
    setError(null);
    try {
      await transport.closeCompanionPairing();
      setInvitation(null);
      setPairingOpen(false);
      setCopied(false);
      await refresh();
    } catch (closeError) {
      setError(publicError(closeError));
    } finally {
      setPairingBusy(false);
    }
  }, [pairingBusy, refresh, transport]);

  useEffect(() => {
    if (!companionAvailable) return;
    let stopped = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const next = await transport.getCompanionCatalog();
        if (!stopped) {
          setCatalog(next);
          setNow(Date.now());
          setError(null);
        }
      } catch (pollError) {
        if (!stopped) setError(publicError(pollError));
      } finally {
        if (!stopped) {
          setLoading(false);
          timer = window.setTimeout(poll, catalogPollMs);
        }
      }
    };
    void poll();
    return () => {
      stopped = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [companionAvailable, transport]);

  useEffect(() => {
    if (!pairingOpen && !catalog?.pending.length) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, [catalog?.pending.length, pairingOpen]);

  useEffect(() => {
    if (!pairingOpen) return;
    const dialog = dialogRef.current;
    const restoreTarget = pairButtonRef.current;
    const focusable = () =>
      Array.from(
        dialog?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );
    window.requestAnimationFrame(() => focusable()[0]?.focus());
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        void cancelPairing();
        return;
      }
      if (event.key !== "Tab") return;
      const targets = focusable();
      if (!targets.length) return;
      const first = targets[0];
      const last = targets.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      restoreTarget?.focus();
    };
  }, [cancelPairing, pairingOpen]);

  const openPairing = async () => {
    setPairingBusy(true);
    setError(null);
    setCopied(false);
    try {
      const next = await transport.openCompanionPairing();
      setInvitation(next);
      setNow(Date.now());
      setPairingOpen(true);
      await refresh();
    } catch (openError) {
      setError(publicError(openError));
    } finally {
      setPairingBusy(false);
    }
  };

  const decide = async (
    request: PendingCompanionPairing,
    decision: CompanionPairingDecision,
  ) => {
    setBusyRequestId(request.requestId);
    setError(null);
    try {
      await transport.decideCompanionPairing(request.requestId, decision);
      await refresh();
    } catch (decisionError) {
      setError(publicError(decisionError));
    } finally {
      setBusyRequestId(null);
    }
  };

  const revoke = async (device: CompanionDevice) => {
    if (
      device.revokedAtMs !== undefined ||
      !window.confirm(
        `Revoke ${device.name}? Its active companion connection will close immediately.`,
      )
    ) {
      return;
    }
    setBusyDeviceId(device.id);
    setError(null);
    try {
      await transport.revokeCompanionDevice(device.id);
      await refresh();
    } catch (revokeError) {
      setError(publicError(revokeError));
    } finally {
      setBusyDeviceId(null);
    }
  };

  const copyInvitation = async () => {
    if (!invitation) return;
    try {
      await navigator.clipboard.writeText(invitation.ticket);
      setCopied(true);
    } catch {
      setCopied(false);
      setError("Copy was blocked. Select and copy the pairing ticket manually.");
    }
  };

  if (!companionAvailable) {
    return (
      <main className="utility-view" aria-labelledby="devices-title">
        <header className="utility-header">
          <span>Worldwide companion</span>
          <h1 id="devices-title">Not available on this host</h1>
          <p>
            Restart ygg serve with both --companion and --companion-relay n0 to
            pair a native companion.
          </p>
        </header>
      </main>
    );
  }

  const devices = catalog?.devices ?? [];
  const pending = catalog?.pending ?? [];

  return (
    <main className="utility-view" aria-labelledby="devices-title">
      <header className="utility-header devices-header">
        <div>
          <span>Worldwide companion</span>
          <h1 id="devices-title">Connected devices</h1>
          <p>
            Native companions connect directly or through the explicitly enabled
            n0 relay. There is no Ygg account or hosted control plane.
          </p>
        </div>
        <button
          ref={pairButtonRef}
          className="primary-button"
          onClick={() => void openPairing()}
          disabled={pairingBusy}
        >
          {pairingBusy ? (
            <LoaderCircle className="spin" aria-hidden="true" />
          ) : (
            <Plus aria-hidden="true" />
          )}
          Pair a device
        </button>
      </header>

      <section className="connection-summary">
        <div className="connection-pulse" aria-hidden="true">
          <Wifi />
        </div>
        <div>
          <span>Companion endpoint online</span>
          <strong>{hostName}</strong>
          <p>
            Payloads are end-to-end encrypted. The relay can observe connection
            timing, region, and traffic volume.
          </p>
        </div>
        <span className="secure-label">
          <ShieldCheck aria-hidden="true" /> Companion ready
        </span>
      </section>

      {error ? <p className="device-operation-error" role="alert">{error}</p> : null}

      {pending.length ? (
        <section className="companion-pending-list" aria-label="Pending pairing requests">
          <h2>Pending approval</h2>
          {pending.map((request) => (
            <PendingRequest
              key={request.requestId}
              request={request}
              now={now}
              busy={busyRequestId === request.requestId}
              onDecision={(decision) => void decide(request, decision)}
            />
          ))}
        </section>
      ) : null}

      <section className="device-list" aria-label="Paired devices" aria-busy={loading}>
        {loading && !catalog ? (
          <p className="device-list-empty">
            <LoaderCircle className="spin" aria-hidden="true" /> Loading paired devices…
          </p>
        ) : devices.length === 0 ? (
          <p className="device-list-empty">
            No native companion is paired with this host yet.
          </p>
        ) : (
          devices.map((device) => {
            const revoked = device.revokedAtMs !== undefined;
            const status = revoked
              ? "offline"
              : device.connected
                ? "connected"
                : "offline";
            return (
              <article key={device.id}>
                <span className={`device-glyph is-${status}`}>
                  {platformIcon(device)}
                </span>
                <div className="device-copy">
                  <strong>{device.name}</strong>
                  <span>
                    {platformLabel(device.platform)} · paired {new Date(device.pairedAtMs).toLocaleDateString()}
                  </span>
                </div>
                <span className={`device-state is-${status}`}>{lastSeen(device)}</span>
                <button
                  className="device-revoke"
                  onClick={() => void revoke(device)}
                  disabled={revoked || busyDeviceId === device.id}
                >
                  <Unlink aria-hidden="true" />
                  {revoked ? "Revoked" : busyDeviceId === device.id ? "Revoking…" : "Revoke"}
                </button>
              </article>
            );
          })
        )}
      </section>

      <section className="device-security-note">
        <ShieldCheck aria-hidden="true" />
        <div>
          <strong>Pairing is separate from agent authority</strong>
          <p>
            A paired device keeps each session’s selected authority and cannot
            access the host terminal or administer other devices. Revocation is
            immediate and durable.
          </p>
        </div>
      </section>

      {pairingOpen && invitation ? (
        <div className="modal-layer" role="dialog" aria-modal="true" aria-labelledby="pairing-title">
          <button
            className="modal-backdrop"
            onClick={() => void cancelPairing()}
            aria-label="Cancel pairing"
            tabIndex={-1}
            disabled={pairingBusy}
          />
          <section ref={dialogRef} className="pairing-dialog companion-pairing-dialog">
            <header>
              <span className="pairing-icon">
                <Link2 aria-hidden="true" />
              </span>
              <button
                className="icon-button"
                onClick={() => void cancelPairing()}
                disabled={pairingBusy}
              >
                <X aria-hidden="true" />
                <span className="sr-only">Cancel pairing</span>
              </button>
            </header>
            <h2 id="pairing-title">Pair a native Ygg companion</h2>
            <p>
              Paste this one-time ticket into the Ygg mobile app. After its
              authenticated request arrives, compare the phrase shown on both
              surfaces before approving.
            </p>
            <label className="pairing-ticket">
              <span>One-time pairing ticket</span>
              <textarea value={invitation.ticket} readOnly rows={4} />
            </label>
            <button className="primary-button pairing-copy" onClick={() => void copyInvitation()}>
              {copied ? <Check aria-hidden="true" /> : <Clipboard aria-hidden="true" />}
              {copied ? "Copied" : "Copy ticket"}
            </button>
            <div className="pairing-status" aria-live="polite">
              {pending.length ? (
                <>
                  <Check aria-hidden="true" /> {pending.length} device request{pending.length === 1 ? "" : "s"} received
                </>
              ) : invitation.expiresAtMs <= now ? (
                <>Invitation expired. Cancel and create a new ticket.</>
              ) : (
                <>
                  <LoaderCircle className="spin" aria-hidden="true" /> Waiting for a native companion
                </>
              )}
            </div>
            {pending.map((request) => (
              <PendingRequest
                key={request.requestId}
                request={request}
                now={now}
                busy={busyRequestId === request.requestId}
                onDecision={(decision) => void decide(request, decision)}
              />
            ))}
            <footer>
              <ShieldCheck aria-hidden="true" />
              Ticket expires in {expiresIn(invitation.expiresAtMs, now)} and works once.
            </footer>
          </section>
        </div>
      ) : null}
    </main>
  );
}
