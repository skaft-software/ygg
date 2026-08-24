/// <reference types="vite/client" />

import {
  act,
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  CompanionCatalog,
  CompanionPairingDecision,
  PendingCompanionPairing,
} from "../protocol";
import type { CompanionAdminTransport } from "../transport";
import { projectCompanionCatalog } from "../wire";
import { DevicesView } from "./Devices";

const emptyCatalog = (): CompanionCatalog => ({
  revision: 1,
  devices: [],
  pending: [],
});

function pending(
  requestId: string,
  name: string,
  expiresAtMs = Date.now() + 60_000,
): PendingCompanionPairing {
  return {
    requestId,
    device: { name, platform: "ios", appVersion: "0.1.0" },
    state: "pending",
    phrase: "amber · birch · cedar · dune",
    expiresAtMs,
  };
}

function mockTransport(
  overrides: Partial<CompanionAdminTransport> = {},
): CompanionAdminTransport {
  return {
    getCompanionDevices: vi.fn(async () => []),
    getCompanionCatalog: vi.fn(async () => emptyCatalog()),
    openCompanionPairing: vi.fn(async () => ({
      ticket: "ygg://pair/v1/example",
      expiresAtMs: Date.now() + 120_000,
    })),
    closeCompanionPairing: vi.fn(async () => undefined),
    decideCompanionPairing: vi.fn(async () => undefined),
    revokeCompanionDevice: vi.fn(async () => undefined),
    ...overrides,
  };
}

describe("connected devices", () => {
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  it("polls the authoritative catalog and renders changes", async () => {
    const getCatalog = vi
      .fn<CompanionAdminTransport["getCompanionCatalog"]>()
      .mockResolvedValueOnce(emptyCatalog())
      .mockResolvedValue({
        revision: 2,
        pending: [],
        devices: [
          {
            id: "device-phone",
            name: "Daily phone",
            platform: "ios",
            pairedAtMs: 1,
            connected: true,
          },
        ],
      });
    const setTimeoutSpy = vi.spyOn(window, "setTimeout");
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport({ getCompanionCatalog: getCatalog })}
      />,
    );

    expect(
      await screen.findByText("No native companion is paired with this host yet."),
    ).toBeVisible();
    const scheduledPoll = setTimeoutSpy.mock.calls.find(
      ([, delay]) => delay === 2_000,
    )?.[0];
    expect(scheduledPoll).toBeTypeOf("function");
    if (typeof scheduledPoll !== "function") {
      throw new Error("catalog poll was not scheduled");
    }

    await act(async () => {
      scheduledPoll();
    });
    expect(await screen.findByText("Daily phone")).toBeVisible();
    expect(getCatalog).toHaveBeenCalledTimes(2);
  });

  it("approves and denies pending requests, and disables expired decisions", async () => {
    let catalog: CompanionCatalog = {
      revision: 1,
      devices: [],
      pending: [
        pending("pair-approve", "Approve phone"),
        pending("pair-deny", "Deny tablet"),
        pending("pair-expired", "Expired phone", Date.now() - 1),
      ],
    };
    const getCatalog = vi.fn(async () => catalog);
    const decide = vi.fn(
      async (requestId: string, decision: CompanionPairingDecision) => {
        catalog = {
          ...catalog,
          revision: catalog.revision + 1,
          pending:
            decision === "approve"
              ? catalog.pending.map((request) =>
                  request.requestId === requestId
                    ? { ...request, state: "approved" as const }
                    : request,
                )
              : catalog.pending.filter(
                  (request) => request.requestId !== requestId,
                ),
        };
      },
    );
    const user = userEvent.setup();
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport({
          getCompanionCatalog: getCatalog,
          decideCompanionPairing: decide,
        })}
      />,
    );

    const approveCard = (await screen.findByText("Approve phone")).closest("article");
    expect(approveCard).not.toBeNull();
    await user.click(within(approveCard!).getByRole("button", { name: "Approve" }));
    expect(decide).toHaveBeenCalledWith("pair-approve", "approve");
    expect(
      await screen.findByText("Approved · waiting for secure-storage confirmation"),
    ).toBeVisible();

    const denyCard = screen.getByText("Deny tablet").closest("article");
    expect(denyCard).not.toBeNull();
    await user.click(within(denyCard!).getByRole("button", { name: "Deny" }));
    expect(decide).toHaveBeenCalledWith("pair-deny", "deny");
    await waitFor(() => expect(screen.queryByText("Deny tablet")).toBeNull());

    const expiredCard = screen.getByText("Expired phone").closest("article");
    expect(expiredCard).not.toBeNull();
    expect(within(expiredCard!).getByRole("button", { name: "Approve" })).toBeDisabled();
    expect(within(expiredCard!).getByRole("button", { name: "Deny" })).toBeDisabled();
  });

  it("confirms revocation and refreshes the durable device state", async () => {
    let catalog: CompanionCatalog = {
      revision: 7,
      pending: [],
      devices: [
        {
          id: "device-phone",
          name: "Daily phone",
          platform: "ios",
          pairedAtMs: 1,
          connected: true,
        },
      ],
    };
    const getCatalog = vi.fn(async () => catalog);
    const revoke = vi.fn(async (deviceId: string) => {
      catalog = {
        ...catalog,
        revision: 8,
        devices: catalog.devices.map((device) =>
          device.id === deviceId
            ? { ...device, connected: false, revokedAtMs: Date.now() }
            : device,
        ),
      };
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const user = userEvent.setup();
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport({
          getCompanionCatalog: getCatalog,
          revokeCompanionDevice: revoke,
        })}
      />,
    );

    expect(await screen.findByText("Daily phone")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Revoke" }));
    expect(confirm).toHaveBeenCalledWith(
      "Revoke Daily phone? Its active companion connection will close immediately.",
    );
    expect(revoke).toHaveBeenCalledWith("device-phone");
    expect(await screen.findByRole("button", { name: "Revoked" })).toBeDisabled();
    expect(screen.getByText("Access revoked")).toBeVisible();
  });

  it("traps dialog focus, closes with Escape, and restores the trigger", async () => {
    const closePairing = vi.fn(async () => undefined);
    const user = userEvent.setup();
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport({ closeCompanionPairing: closePairing })}
      />,
    );

    const trigger = screen.getByRole("button", { name: "Pair a device" });
    await user.click(trigger);
    const dialog = await screen.findByRole("dialog", {
      name: "Pair a native Ygg companion",
    });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(within(dialog).queryByText("Verification phrase")).toBeNull();
    expect(
      within(dialog).queryByText("amber · birch · cedar · dune"),
    ).toBeNull();
    const closeButton = within(dialog).getAllByRole("button", {
      name: "Cancel pairing",
    })[1];
    expect(closeButton).toBeDefined();
    await waitFor(() => expect(closeButton).toHaveFocus());

    const copyButton = within(dialog).getByRole("button", { name: "Copy ticket" });
    copyButton.focus();
    await user.tab();
    expect(closeButton).toHaveFocus();

    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(closePairing).toHaveBeenCalledTimes(1);
    expect(trigger).toHaveFocus();
  });

  it("shows the pairing ticket as a QR code the companion can scan", async () => {
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport()}
      />,
    );
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Pair a device" }));
    expect(
      await screen.findByRole("img", { name: "Pairing code" }),
    ).toBeVisible();
  });

  it("keeps the manual ticket path when the ticket overflows a QR code", async () => {
    render(
      <DevicesView
        hostName="Desk host"
        companionAvailable
        transport={mockTransport({
          openCompanionPairing: vi.fn(async () => ({
            // Lowercase forces byte-mode encoding; 2332 total bytes is one
            // past the 2331-byte byte-mode capacity of QR version 40/M.
            ticket: `ygg://pair/v1/${"a".repeat(2318)}`,
            expiresAtMs: Date.now() + 120_000,
          })),
        })}
      />,
    );
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Pair a device" }));
    expect(await screen.findByText(/too long for a QR code/)).toBeVisible();
    expect(screen.queryByRole("img", { name: "Pairing code" })).toBeNull();
  });

  it("rejects malformed companion catalogs before UI state is updated", () => {
    expect(() =>
      projectCompanionCatalog({
        revision: 1,
        devices: [
          {
            id: "device-phone",
            name: "Phone",
            platform: "ios",
            pairedAtMs: 1,
            connected: true,
            revokedAtMs: 2,
          },
        ],
        pending: [],
      }),
    ).toThrow(/must be false after revocation/);
    expect(() =>
      projectCompanionCatalog({ revision: 1, devices: [], pending: [{ requestId: "x" }] }),
    ).toThrow(/companionCatalog\.pending\[0\]/);
    for (const name of ["Phone\t", "Phone\u007f", "Phone\u0085"]) {
      expect(() =>
        projectCompanionCatalog({
          revision: 1,
          devices: [
            {
              id: "device-phone",
              name,
              platform: "ios",
              pairedAtMs: 1,
              connected: false,
            },
          ],
          pending: [],
        }),
      ).toThrow(/must not contain control characters/);
    }
  });
});
