/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SettingsView } from "./Settings";

const values = new Map<string, string>();

function renderSettings() {
  render(
    <SettingsView
      notificationsSupported={false}
      notificationsEnabled={false}
      notificationPermission="unsupported"
      onNotificationsChange={vi.fn(async () => false)}
    />,
  );
}

describe("native companion settings entry", () => {
  beforeEach(() => {
    values.clear();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      clear: () => values.clear(),
    });
  });

  afterEach(() => {
    cleanup();
    document
      .querySelectorAll('meta[name="ygg-native-settings-url"]')
      .forEach((node) => node.remove());
    vi.unstubAllGlobals();
  });

  it("links only to the isolated bounded loopback settings launch URL", () => {
    const url =
      "http://127.0.0.1:43123/_native/settings/start/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const meta = document.createElement("meta");
    meta.name = "ygg-native-settings-url";
    meta.content = url;
    document.head.append(meta);

    renderSettings();

    expect(
      screen.getByRole("link", { name: /open native companion settings/i }),
    ).toHaveAttribute("href", url);
  });

  it("does not expose malformed or non-loopback settings targets", () => {
    const meta = document.createElement("meta");
    meta.name = "ygg-native-settings-url";
    meta.content =
      "https://attacker.invalid/_native/settings/start/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    document.head.append(meta);

    renderSettings();

    expect(
      screen.queryByRole("link", { name: /open native companion settings/i }),
    ).toBeNull();
  });
});
