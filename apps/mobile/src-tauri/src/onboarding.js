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
