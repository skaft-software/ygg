import {
  Check,
  Laptop,
  Link2,
  LoaderCircle,
  MoreHorizontal,
  Plus,
  ShieldCheck,
  Smartphone,
  Unlink,
  Wifi,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ConnectedDevice } from "../protocol";

const platformIcon = (device: ConnectedDevice) => {
  if (device.platform === "iOS" || device.platform === "Android") {
    return <Smartphone aria-hidden="true" />;
  }
  return <Laptop aria-hidden="true" />;
};

function PairingCode() {
  const cells = [
    1, 1, 1, 0, 1, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 1, 0, 1, 1, 0, 1, 0, 0,
    1, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 1, 0, 1, 0, 1, 1, 0, 1, 1, 1, 0, 1, 0,
    1, 1, 0, 0, 1, 1, 1, 0, 1, 0, 1, 1, 0, 1, 1, 0, 1, 0, 1, 1, 1, 0, 1, 1,
    1, 0, 1, 0, 1,
  ];
  return (
    <div className="pairing-code" aria-label="Pairing code visualization">
      {cells.map((active, index) => (
        <span key={index} className={active ? "is-active" : ""} />
      ))}
    </div>
  );
}

export function DevicesView({
  hostName,
  devices: initialDevices,
  lanAvailable,
}: {
  hostName: string;
  devices: ConnectedDevice[];
  lanAvailable: boolean;
}) {
  const [pairingOpen, setPairingOpen] = useState(false);
  const pairButtonRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLElement>(null);
  const [devices, setDevices] = useState(() =>
    initialDevices.map((device) =>
      device.status === "this_device" ? { ...device, name: hostName } : device,
    ),
  );

  const revoke = (deviceId: string) => {
    setDevices((current) =>
      current.map((device) =>
        device.id === deviceId
          ? { ...device, status: "offline", lastSeen: "Access revoked" }
          : device,
      ),
    );
  };

  useEffect(() => {
    if (!pairingOpen) return;
    const dialog = dialogRef.current;
    const restoreTarget = pairButtonRef.current;
    const focusable = () =>
      Array.from(
        dialog?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      );
    window.requestAnimationFrame(() => focusable()[0]?.focus());
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setPairingOpen(false);
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
  }, [pairingOpen]);

  if (!lanAvailable) {
    return (
      <main className="utility-view" aria-labelledby="devices-title">
        <header className="utility-header">
          <span>Connected devices</span>
          <h1 id="devices-title">Not available on this host</h1>
          <p>
            Start Ygg with local-network clients enabled to pair another
            device.
          </p>
        </header>
      </main>
    );
  }

  return (
    <main className="utility-view" aria-labelledby="devices-title">
      <header className="utility-header devices-header">
        <div>
          <span>Secure local network</span>
          <h1 id="devices-title">Connected devices</h1>
          <p>
            Ygg devices pair directly. There is no account, cloud sign-in, or
            hosted Ygg control plane.
          </p>
        </div>
        <button
          ref={pairButtonRef}
          className="primary-button"
          onClick={() => setPairingOpen(true)}
        >
          <Plus aria-hidden="true" />
          Pair a device
        </button>
      </header>

      <section className="connection-summary">
        <div className="connection-pulse" aria-hidden="true">
          <Wifi />
        </div>
        <div>
          <span>Available on this LAN</span>
          <strong>{hostName}</strong>
          <p>
            Paired devices can open and control the same Ygg sessions while
            they are on this network.
          </p>
        </div>
        <span className="secure-label">
          <ShieldCheck aria-hidden="true" />
          LAN ready
        </span>
      </section>

      <section className="device-list" aria-label="Paired devices">
        {devices.map((device) => (
          <article key={device.id}>
            <span className={`device-glyph is-${device.status}`}>
              {platformIcon(device)}
            </span>
            <div className="device-copy">
              <strong>{device.name}</strong>
              <span>
                {device.platform} ·{" "}
                {device.connection === "local" ? "This Mac" : "Local network"}
              </span>
            </div>
            <span className={`device-state is-${device.status}`}>
              {device.status === "this_device"
                ? "This device"
                : device.status === "connected"
                  ? "Connected"
                  : device.lastSeen}
            </span>
            {device.status === "this_device" ? (
              <button className="icon-button" aria-label="Device options">
                <MoreHorizontal aria-hidden="true" />
              </button>
            ) : (
              <button
                className="device-revoke"
                onClick={() => revoke(device.id)}
                disabled={device.lastSeen === "Access revoked"}
              >
                <Unlink aria-hidden="true" />
                {device.lastSeen === "Access revoked" ? "Revoked" : "Revoke"}
              </button>
            )}
          </article>
        ))}
      </section>

      <section className="device-security-note">
        <ShieldCheck aria-hidden="true" />
        <div>
          <strong>Pairing is separate from agent authority</strong>
          <p>
            A paired device still uses each session’s selected authority.
            Revoking a device closes its active connection immediately.
          </p>
        </div>
      </section>

      {pairingOpen ? (
        <div className="modal-layer" role="dialog" aria-modal="true">
          <button
            className="modal-backdrop"
            onClick={() => setPairingOpen(false)}
            aria-label="Close pairing"
            tabIndex={-1}
          />
          <section ref={dialogRef} className="pairing-dialog">
            <header>
              <span className="pairing-icon">
                <Link2 aria-hidden="true" />
              </span>
              <button
                className="icon-button"
                onClick={() => setPairingOpen(false)}
              >
                <X aria-hidden="true" />
                <span className="sr-only">Close</span>
              </button>
            </header>
            <h2>Pair a Ygg device</h2>
            <p>
              On another Ygg app, choose “Add device” and scan this one-time
              code. Keep both devices on the same local network.
            </p>
            <PairingCode />
            <div className="pairing-phrase">
              <span>Verification phrase</span>
              <strong>willow · lantern · cobalt</strong>
            </div>
            <div className="pairing-status">
              <LoaderCircle className="spin" aria-hidden="true" />
              Waiting for another device
            </div>
            <footer>
              <Check aria-hidden="true" />
              This code expires in 4 minutes and works once.
            </footer>
          </section>
        </div>
      ) : null}
    </main>
  );
}
