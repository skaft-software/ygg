"use strict";

const byId = (id) => document.getElementById(id);
const statusText = byId("status");
const phrasePanel = byId("phrase-panel");
const phrase = byId("phrase");
const expiry = byId("expiry");
const importPanel = byId("import-panel");
const pairButton = byId("pair");
const cancelButton = byId("cancel");
const removeButton = byId("remove");
const scanButton = byId("scan");
const scanOverlay = byId("scan-overlay");
const scanVideo = byId("scan-video");
const scanStatus = byId("scan-status");
const scanCloseButton = byId("scan-close");
let pollTimer = 0;

async function nativeRequest(path, method = "GET", body) {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    cache: "no-store",
    headers: body ? { "Content-Type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const value = await response.json();
  if (!response.ok) throw new Error(typeof value.message === "string" ? value.message : "The native app rejected the request.");
  return value;
}

function render(state) {
  statusText.textContent = state.message;
  const pending = state.phase === "pending";
  const paired = state.phase === "paired";
  importPanel.hidden = pending || paired || state.phase === "restartRequired";
  if (importPanel.hidden) stopScanner();
  cancelButton.hidden = !pending;
  removeButton.hidden = !["revoked", "restartRequired"].includes(state.phase);
  phrasePanel.hidden = !pending || !state.phrase;
  phrase.textContent = pending ? state.phrase || "" : "";
  expiry.textContent = pending && state.expiresAtMs
    ? `Invitation expires ${new Date(state.expiresAtMs).toLocaleTimeString()}`
    : "";
  if (paired) {
    window.clearTimeout(pollTimer);
    window.location.replace("/");
    return;
  }
  if (pending) schedulePoll();
}

function schedulePoll() {
  window.clearTimeout(pollTimer);
  pollTimer = window.setTimeout(async () => {
    try {
      render(await nativeRequest("/_native/pair/poll", "POST"));
    } catch (error) {
      statusText.textContent = error instanceof Error ? error.message : "The host is unavailable.";
      schedulePoll();
    }
  }, 1500);
}

// ---------------------------------------------------------------------------
// Pairing-code scanner
//
// The companion app ships jsQR (served from /_native/jsqr.js) so the
// phone can decode the host's QR code completely offline. A decoded value
// is only accepted into the ticket field when it has the ticket scheme, so
// an unrelated code in the room cannot be confused with an invitation.
// ---------------------------------------------------------------------------

let scanStream = null;
let scanTimer = 0;
let scanningFrame = false;
const scanCanvas = document.createElement("canvas");
const scanContext = scanCanvas.getContext("2d", { willReadFrequently: true });

const SCAN_PROMPT = "Point the camera at the pairing code.";

function isPairingTicket(value) {
  return (
    typeof value === "string" &&
    value.startsWith("ygg://pair/v1/") &&
    value.length > "ygg://pair/v1/".length &&
    value.length <= 4096
  );
}

function setScanStatus(message) {
  scanStatus.textContent = message;
}

function stopScanner() {
  if (scanTimer) {
    window.clearInterval(scanTimer);
    scanTimer = 0;
  }
  scanningFrame = false;
  if (scanStream) {
    for (const track of scanStream.getTracks()) track.stop();
    scanStream = null;
  }
  scanVideo.srcObject = null;
  if (!scanOverlay.hidden) {
    scanOverlay.hidden = true;
    setScanStatus(SCAN_PROMPT);
  }
}

function handleDecodedTicket(ticket) {
  stopScanner();
  byId("ticket").value = ticket;
  statusText.textContent = "Pairing code recognized. Add a device name, then request approval.";
  byId("device-name").focus();
}

async function scanFrame() {
  if (scanningFrame || scanOverlay.hidden || !scanStream) return;
  if (!scanVideo.videoWidth || !scanVideo.videoHeight) {
    return; // The camera is still producing its first frames.
  }
  scanningFrame = true;
  try {
    const width = scanVideo.videoWidth;
    const height = scanVideo.videoHeight;
    scanCanvas.width = width;
    scanCanvas.height = height;
    scanContext.drawImage(scanVideo, 0, 0, width, height);
    const rgba = scanContext.getImageData(0, 0, width, height).data;
    const decoded = window.jsQR(rgba, width, height, { inversionAttempts: "dontInvert" });
    if (decoded && isPairingTicket(decoded.data)) {
      handleDecodedTicket(decoded.data);
    }
  } finally {
    scanningFrame = false;
  }
}

async function startScanner() {
  if (scanStream) return;
  if (typeof window.jsQR !== "function") {
    scanOverlay.hidden = false;
    setScanStatus("The built-in code reader is unavailable. Paste the ticket instead.");
    return;
  }
  try {
    scanStream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: { ideal: "environment" }, width: { ideal: 1280 }, height: { ideal: 720 } },
      audio: false,
    });
  } catch (error) {
    scanStream = null;
    const denied = error && (error.name === "NotAllowedError" || error.name === "PermissionDeniedError");
    scanOverlay.hidden = false;
    setScanStatus(
      denied
        ? "Camera access was denied. Allow the camera in Settings → Ygg, or paste the ticket below."
        : "The camera is unavailable right now. Paste the ticket instead.",
    );
    return;
  }
  scanVideo.srcObject = scanStream;
  try {
    await scanVideo.play();
  } catch {
    // Autoplay can be rejected before the overlay is visible; the frames
    // still flow through the stream, which is all the decoder needs.
  }
  scanOverlay.hidden = false;
  setScanStatus(SCAN_PROMPT);
  scanTimer = window.setInterval(scanFrame, 300);
}

scanButton.addEventListener("click", () => {
  if (scanStream) {
    stopScanner();
    scanButton.focus();
  } else {
    void startScanner();
  }
});

scanCloseButton.addEventListener("click", () => {
  stopScanner();
  scanButton.focus();
});

pairButton.addEventListener("click", async () => {
  const ticket = byId("ticket").value.trim();
  const deviceName = byId("device-name").value.trim();
  if (!ticket || !deviceName) {
    statusText.textContent = "Enter both a device name and the complete invitation.";
    return;
  }
  pairButton.disabled = true;
  try {
    render(await nativeRequest("/_native/pair", "POST", { ticket, deviceName }));
    byId("ticket").value = "";
  } catch (error) {
    statusText.textContent = error instanceof Error ? error.message : "Pairing failed.";
  } finally {
    pairButton.disabled = false;
  }
});

cancelButton.addEventListener("click", async () => {
  cancelButton.disabled = true;
  try {
    render(await nativeRequest("/_native/pair", "DELETE"));
  } catch (error) {
    statusText.textContent = error instanceof Error ? error.message : "Cancellation failed.";
  } finally {
    cancelButton.disabled = false;
  }
});

removeButton.addEventListener("click", async () => {
  if (!window.confirm("Remove local endpoint identity and companion access from this device?")) return;
  removeButton.disabled = true;
  try {
    render(await nativeRequest("/_native/access", "DELETE"));
  } catch (error) {
    statusText.textContent = error instanceof Error ? error.message : "Removal failed.";
  } finally {
    removeButton.disabled = false;
  }
});

nativeRequest("/_native/state")
  .then(render)
  .catch((error) => { statusText.textContent = error instanceof Error ? error.message : "Native state is unavailable."; });
