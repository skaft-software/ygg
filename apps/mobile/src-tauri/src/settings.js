"use strict";

const root = document.documentElement;
const returnUrl = root.dataset.returnUrl;
const statusText = document.getElementById("status");
const removeButton = document.getElementById("remove");
const backButton = document.getElementById("back");

async function nativeRequest(path, method = "GET") {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    cache: "no-store",
  });
  const value = await response.json();
  if (!response.ok) {
    throw new Error(
      typeof value.message === "string"
        ? value.message
        : "The native app rejected the request.",
    );
  }
  return value;
}

function render(state) {
  statusText.textContent = state.message;
  const removable = ["paired", "revoked", "restartRequired"].includes(state.phase);
  removeButton.hidden = !removable;
  removeButton.disabled = state.phase === "restartRequired";
  if (state.phase === "restartRequired") {
    removeButton.textContent = "Local access removed — restart required";
  }
}

removeButton.addEventListener("click", async () => {
  if (
    !window.confirm(
      "Remove this device's endpoint identity and pinned host access? The app must restart before it can pair again.",
    )
  ) {
    return;
  }
  removeButton.disabled = true;
  try {
    render(await nativeRequest("/_native/access", "DELETE"));
  } catch (error) {
    statusText.textContent =
      error instanceof Error ? error.message : "Local removal failed.";
    removeButton.disabled = false;
  }
});

backButton.addEventListener("click", () => {
  if (typeof returnUrl === "string" && returnUrl.startsWith("http://127.0.0.1:")) {
    window.location.replace(returnUrl);
  }
});

nativeRequest("/_native/state")
  .then(render)
  .catch((error) => {
    statusText.textContent =
      error instanceof Error ? error.message : "Native state is unavailable.";
    removeButton.hidden = true;
  });
