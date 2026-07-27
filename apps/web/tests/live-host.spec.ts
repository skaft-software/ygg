import { expect, request as playwrightRequest, test, type Page } from "@playwright/test";
import { readFile } from "node:fs/promises";

import {
  ABORT_PARTIAL,
  ABORT_PROMPT,
  BRANCH_A_PROMPT,
  BRANCH_A_REPLY,
  BRANCH_B_PROMPT,
  BRANCH_B_REPLY,
  EXPORT_CANARY,
  LIVE_API_MODEL,
  LIVE_MODEL_ID,
  LIVE_PROVIDER_TOKEN,
  LiveHostHarness,
  RESUME_PROMPT,
  RESUME_REPLY,
  STREAM_PARTIAL,
  STREAM_PROMPT,
  STREAM_REPLY,
  type RecordedChatRequest,
} from "./support/live-host";

interface RawBranchEntry {
  entryId: string;
  kind: string;
  checkoutable: boolean;
  label: string;
}

interface RawSessionSnapshot {
  sessionId: string;
  cursor: {
    actorGeneration: number;
    sequence: number;
  };
  durableHead?: string;
  liveState: string;
  activeRunId?: string;
  model: {
    model: string;
  };
  branches: {
    head?: string;
    entries: RawBranchEntry[];
  };
  items: unknown[];
}

function sessionIdFromUrl(url: string): string {
  const pathname = new URL(url).pathname;
  const match = /^\/session\/([^/]+)$/.exec(pathname);
  if (!match) throw new Error(`Expected a session route, received ${pathname}.`);
  return decodeURIComponent(match[1]);
}

async function sendPrompt(page: Page, prompt: string): Promise<void> {
  const composer = page.getByLabel("Message ygg");
  await expect(composer).toBeVisible();
  await composer.fill(prompt);
  const send = page.getByRole("button", { name: "Send message" });
  await expect(send).toBeEnabled();
  await send.click();
  await expect(composer).toHaveValue("");
}

async function expectDone(page: Page, reply: string): Promise<void> {
  await expect(page.getByText(reply, { exact: true })).toBeVisible();
  await expect(page.locator(".header-status")).toHaveText("Done");
  await expect(page.getByRole("button", { name: "Stop ygg" })).toHaveCount(0);
}

async function completePrompt(
  page: Page,
  host: LiveHostHarness,
  prompt: string,
  reply: string,
): Promise<RecordedChatRequest> {
  await sendPrompt(page, prompt);
  const request = await host.provider.waitForPrompt(prompt);
  await expectDone(page, reply);
  return request;
}

function expectDeterministicRequest(request: RecordedChatRequest): void {
  expect(request.authorization).toBe(`Bearer ${LIVE_PROVIDER_TOKEN}`);
  expect(request.body.model).toBe(LIVE_API_MODEL);
  expect(request.body.stream).toBe(true);
  expect(request.body.tools ?? []).toEqual([]);
}

async function sessionSnapshot(
  page: Page,
  origin: string,
  sessionId: string,
): Promise<RawSessionSnapshot> {
  const response = await page.request.get(
    `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}`,
  );
  expect(response.status()).toBe(200);
  return (await response.json()) as RawSessionSnapshot;
}

test.describe.configure({ mode: "serial" });

test("runs the authenticated production host lifecycle end to end", async ({
  context,
  page,
}) => {
  test.setTimeout(180_000);
  const host = await LiveHostHarness.create();
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  const expectedBundleHash = (
    await readFile(
      new URL(
        "../../../extensions/ygg-serve/web/bundle.sha256",
        import.meta.url,
      ),
      "utf8",
    )
  ).trim();

  try {
    const started = await host.start();
    const { origin, launchUrl } = started;

    await test.step("authenticates bootstrap with a one-use launch URL", async () => {
      const unauthenticated = await context.request.get(
        `${origin}/api/v1/bootstrap`,
      );
      expect(unauthenticated.status()).toBe(401);

      const exchanged = await context.request.get(launchUrl, {
        maxRedirects: 0,
      });
      expect(exchanged.status()).toBe(303);
      expect(exchanged.headers()["location"]).toBe("/");
      expect(exchanged.headers()["set-cookie"]).toMatch(/HttpOnly/i);
      expect(exchanged.headers()["set-cookie"]).toMatch(/SameSite=Strict/i);

      const reused = await context.request.get(launchUrl, {
        maxRedirects: 0,
      });
      expect(reused.status()).toBe(401);

      const bootstrapResponse = page.waitForResponse(
        (response) =>
          response.url() === `${origin}/api/v1/bootstrap` &&
          response.request().method() === "GET",
      );
      const documentResponse = await page.goto(
        `${origin}/?transport=fixture`,
      );
      expect(documentResponse?.status()).toBe(200);
      expect(documentResponse?.headers()["x-ygg-web-bundle"]).toBe(
        expectedBundleHash,
      );

      const bootstrap = await bootstrapResponse;
      expect(bootstrap.status()).toBe(200);
      const bootstrapBody = JSON.stringify(await bootstrap.json());
      expect(bootstrapBody).toContain(LIVE_MODEL_ID);
      expect(bootstrapBody).toContain('"sessionBranches":true');
      expect(bootstrapBody).toContain('"sessionExport":true');

      await expect(page.getByLabel("Message ygg")).toBeVisible();
      await expect(
        page.getByText("Demo data · responses and actions are simulated", {
          exact: true,
        }),
      ).toHaveCount(0);
    });

    let sessionId = "";
    await test.step("creates a real host session", async () => {
      const initialSessionId = sessionIdFromUrl(page.url());
      await page
        .getByRole("button", { name: "New session", exact: true })
        .click();
      await expect
        .poll(() => sessionIdFromUrl(page.url()))
        .not.toBe(initialSessionId);
      sessionId = sessionIdFromUrl(page.url());

      const snapshot = await sessionSnapshot(page, origin, sessionId);
      expect(snapshot.sessionId).toBe(sessionId);
      expect(snapshot.model.model).toBe(LIVE_MODEL_ID);
      expect(snapshot.liveState).toBe("idle");
    });

    await test.step("streams a real model response and commits it durably", async () => {
      await sendPrompt(page, STREAM_PROMPT);
      const request = await host.provider.waitForPrompt(STREAM_PROMPT);
      expectDeterministicRequest(request);
      await expect(page.getByText(STREAM_PARTIAL, { exact: true })).toBeVisible();
      await expect(page.getByRole("button", { name: "Stop ygg" })).toBeVisible();

      host.provider.release(STREAM_PROMPT);
      await expectDone(page, STREAM_REPLY);
      const snapshot = await sessionSnapshot(page, origin, sessionId);
      expect(snapshot.liveState).toBe("done");
      expect(JSON.stringify(snapshot.items)).toContain(STREAM_REPLY);
      expect(snapshot.durableHead).toBeTruthy();
    });

    await test.step("stops an in-flight provider stream", async () => {
      await sendPrompt(page, ABORT_PROMPT);
      const request = await host.provider.waitForPrompt(ABORT_PROMPT);
      expectDeterministicRequest(request);
      await expect(page.getByText(ABORT_PARTIAL, { exact: true })).toBeVisible();

      await page.getByRole("button", { name: "Stop ygg" }).click();
      await host.provider.waitForAbort(ABORT_PROMPT);
      await expect(page.locator(".header-status")).toHaveText("Stopped");
      await expect(page.getByRole("button", { name: "Stop ygg" })).toHaveCount(0);

      const snapshot = await sessionSnapshot(page, origin, sessionId);
      expect(snapshot.liveState).toBe("stopped");
      expect(snapshot.activeRunId).toBeUndefined();
    });

    let branchATarget = "";
    let staleBeforeCheckout!: RawSessionSnapshot;
    await test.step("replaces the branch projection and rejects a stale snapshot", async () => {
      const branchARequest = await completePrompt(
        page,
        host,
        BRANCH_A_PROMPT,
        BRANCH_A_REPLY,
      );
      expectDeterministicRequest(branchARequest);

      const afterA = await sessionSnapshot(page, origin, sessionId);
      branchATarget =
        afterA.branches.entries.find(
          (entry) =>
            entry.kind === "assistantMessage" &&
            entry.checkoutable &&
            entry.label === BRANCH_A_REPLY,
        )?.entryId ?? "";
      expect(branchATarget).not.toBe("");

      const branchBRequest = await completePrompt(
        page,
        host,
        BRANCH_B_PROMPT,
        BRANCH_B_REPLY,
      );
      expectDeterministicRequest(branchBRequest);
      staleBeforeCheckout = await sessionSnapshot(page, origin, sessionId);
      expect(JSON.stringify(staleBeforeCheckout.items)).toContain(
        BRANCH_B_PROMPT,
      );

      let checkoutStarted = false;
      let sessionGets = 0;
      let releaseFreshSnapshot!: () => void;
      const freshSnapshotGate = new Promise<void>((resolve) => {
        releaseFreshSnapshot = resolve;
      });
      let announceFreshSnapshot!: () => void;
      const freshSnapshotStarted = new Promise<void>((resolve) => {
        announceFreshSnapshot = resolve;
      });
      const snapshotUrl = `${origin}/api/v1/sessions/${encodeURIComponent(
        sessionId,
      )}`;

      await page.route(snapshotUrl, async (route) => {
        if (!checkoutStarted || route.request().method() !== "GET") {
          await route.continue();
          return;
        }
        sessionGets += 1;
        if (sessionGets === 1) {
          await route.fulfill({
            status: 200,
            contentType: "application/json",
            body: JSON.stringify(staleBeforeCheckout),
          });
          return;
        }
        if (sessionGets === 2) {
          announceFreshSnapshot();
          await freshSnapshotGate;
          const response = await route.fetch();
          await route.fulfill({ response });
          return;
        }
        await route.continue();
      });

      await page.getByRole("button", { name: "Session actions" }).click();
      await page.getByRole("menuitem", { name: "Session history" }).click();
      const branchARow = page
        .locator(".branch-history-row")
        .filter({ hasText: BRANCH_A_REPLY });
      await expect(branchARow).toBeVisible();
      checkoutStarted = true;
      await branchARow.getByRole("button", { name: "Switch here" }).click();

      await freshSnapshotStarted;
      expect(sessionGets).toBe(2);
      await expect(
        page.getByText(BRANCH_B_PROMPT, { exact: true }),
      ).toBeVisible();

      releaseFreshSnapshot();
      await expect(
        page.getByText(BRANCH_B_PROMPT, { exact: true }),
      ).toHaveCount(0);
      await expect(
        page.getByText(BRANCH_A_REPLY, { exact: true }),
      ).toBeVisible();
      await page.unroute(snapshotUrl);

      const checkedOut = await sessionSnapshot(page, origin, sessionId);
      expect(checkedOut.branches.head).toBe(branchATarget);
      expect(checkedOut.durableHead).toBe(branchATarget);
      expect(JSON.stringify(checkedOut.items)).not.toContain(BRANCH_B_PROMPT);
      expect(JSON.stringify(checkedOut.items)).not.toContain(BRANCH_B_REPLY);
    });

    await test.step("downloads only a bounded redacted safe export", async () => {
      const sourceText = await host.sessionSourceText();
      expect(sourceText).toContain(EXPORT_CANARY);

      const exportPath = `/api/v1/sessions/${encodeURIComponent(
        sessionId,
      )}/export`;
      const exportUrl = `${origin}${exportPath}`;
      const unauthenticated = await playwrightRequest.newContext();
      try {
        expect((await unauthenticated.get(exportUrl)).status()).toBe(401);
      } finally {
        await unauthenticated.dispose();
      }

      const wrongOrigin = await context.request.get(exportUrl, {
        headers: { Origin: "https://attacker.example" },
      });
      expect(wrongOrigin.status()).toBe(403);

      const queryAttempt = await context.request.get(
        `${exportUrl}?includeSecrets=true`,
      );
      expect(queryAttempt.status()).toBe(400);

      const cookieHeader = (await context.cookies(origin))
        .map((cookie) => `${cookie.name}=${cookie.value}`)
        .join("; ");
      const bodyAttempt = await host.rawRequest(exportPath, {
        headers: {
          Cookie: cookieHeader,
          "Content-Type": "application/json",
          "Content-Length": "2",
        },
        body: "{}",
      });
      expect(bodyAttempt.status).toBe(400);

      const exported = await context.request.get(exportUrl);
      expect(exported.status()).toBe(200);
      const headers = exported.headers();
      const exportedText = await exported.text();
      expect(headers["content-type"]).toBe("application/json; charset=utf-8");
      expect(headers["content-disposition"]).toBe(
        `attachment; filename="ygg-session-${sessionId}.json"`,
      );
      expect(headers["cache-control"]).toBe("no-store");
      expect(headers["x-content-type-options"]).toBe("nosniff");
      expect(headers["referrer-policy"]).toBe("no-referrer");
      expect(headers.etag).toBeUndefined();
      expect(Number(headers["content-length"])).toBe(
        Buffer.byteLength(exportedText),
      );

      const exportedJson = JSON.parse(exportedText) as Record<string, unknown>;
      expect(exportedJson.format).toBe("ygg-session-export");
      expect(exportedJson.redacted).toBe(true);
      expect(exportedJson.redaction_count).toEqual(expect.any(Number));
      expect(exportedJson.redaction_count).not.toBe(0);
      expect(exportedText).not.toContain(EXPORT_CANARY);
      expect(exportedText).toContain("[REDACTED]");

      await page.getByRole("button", { name: "Session actions" }).click();
      const downloadPromise = page.waitForEvent("download");
      await page
        .getByRole("menuitem", { name: "Download safe export" })
        .click();
      const download = await downloadPromise;
      const downloadedPath = await download.path();
      if (!downloadedPath) throw new Error("The safe export was not downloaded.");
      const downloadedText = await readFile(downloadedPath, "utf8");
      expect(downloadedText).not.toContain(EXPORT_CANARY);
      expect(downloadedText).toContain("[REDACTED]");
      await download.delete();

      expect(await host.exportTemporaryEntries()).toEqual([]);
    });

    await test.step("rotates authentication and resumes the checked-out backend after restart", async () => {
      const restartPort = host.port;
      await host.stop();
      const restarted = await host.start(restartPort);
      expect(restarted.origin).toBe(origin);

      const staleCookie = await context.request.get(
        `${origin}/api/v1/bootstrap?selectedSessionId=${encodeURIComponent(
          sessionId,
        )}`,
      );
      expect(staleCookie.status()).toBe(401);

      const reauthenticated = await context.request.get(restarted.launchUrl, {
        maxRedirects: 0,
      });
      expect(reauthenticated.status()).toBe(303);
      const bootstrapResponse = page.waitForResponse(
        (response) =>
          response.url().startsWith(
            `${origin}/api/v1/bootstrap?selectedSessionId=`,
          ) && response.request().method() === "GET",
      );
      const documentResponse = await page.goto(
        `${origin}/session/${encodeURIComponent(sessionId)}`,
      );
      expect(documentResponse?.status()).toBe(200);
      expect(documentResponse?.headers()["x-ygg-web-bundle"]).toBe(
        expectedBundleHash,
      );
      expect((await bootstrapResponse).status()).toBe(200);

      await expect(page.getByText(BRANCH_A_REPLY, { exact: true })).toBeVisible();
      await expect(
        page.getByText(BRANCH_B_PROMPT, { exact: true }),
      ).toHaveCount(0);
      await expect(
        page.getByText(BRANCH_B_REPLY, { exact: true }),
      ).toHaveCount(0);

      const resumed = await completePrompt(
        page,
        host,
        RESUME_PROMPT,
        RESUME_REPLY,
      );
      expectDeterministicRequest(resumed);
      const resumedRequest = JSON.stringify(resumed.body);
      expect(resumedRequest).toContain(BRANCH_A_PROMPT);
      expect(resumedRequest).toContain(BRANCH_A_REPLY);
      expect(resumedRequest).not.toContain(BRANCH_B_PROMPT);
      expect(resumedRequest).not.toContain(BRANCH_B_REPLY);
      expect(resumedRequest).not.toContain(EXPORT_CANARY);

      const snapshot = await sessionSnapshot(page, origin, sessionId);
      expect(snapshot.liveState).toBe("done");
      expect(snapshot.branches.head).not.toBe(branchATarget);
      expect(JSON.stringify(snapshot.items)).toContain(RESUME_REPLY);
      expect(JSON.stringify(snapshot.items)).not.toContain(BRANCH_B_PROMPT);
    });

    host.provider.assertHealthy();
    expect(pageErrors).toEqual([]);
  } finally {
    await host.close();
  }
});
