import { expect, test, type Page } from "@playwright/test";

async function ensureSidebar(page: Page) {
  const opener = page.getByRole("button", { name: "Open sidebar" });
  if (await opener.isVisible()) {
    await opener.click();
  }
  const newSession = page.getByRole("button", {
    name: "New session",
    exact: true,
  });
  await expect(newSession).toBeVisible();
}

async function selectSession(page: Page, title: string) {
  await ensureSidebar(page);
  await page.getByRole("button", { name: new RegExp(title) }).click();
}

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "What should we work on?" })).toBeVisible();
});

test("opens in a fresh, quiet session with the standard composer", async ({
  page,
}) => {
  await expect(
    page.getByRole("button", { name: "New session", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Live", { exact: true })).toBeVisible();
  await expect(page.getByText("Pinned", { exact: true })).toBeVisible();
  await expect(page.getByText("Recents", { exact: true })).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toBeVisible();
  await expect(page.getByLabel("Model")).toHaveValue("claude-sonnet-4-6");
  await expect(page.getByLabel("Session activity")).toBeHidden();
});

test("shows typed work and a conditional activity rail", async ({ page }) => {
  await selectSession(page, "Refine onboarding preview");
  await expect(page.getByText("Read onboarding flow")).toBeVisible();
  await expect(page.getByText("Checking the narrow layout")).toBeVisible();
  await page.getByRole("button", { name: "Open activity" }).click();
  await expect(page.getByLabel("Session activity")).toBeVisible();
  await expect(page.getByText("Verifying keyboard and touch behavior")).toBeVisible();
  await expect(page.getByRole("button", { name: /Onboarding preview/ })).toBeVisible();
});

test("opens an output into the dominant preview inspector", async (
  { page },
  testInfo,
) => {
  await selectSession(page, "Review release readiness");
  await page.getByRole("button", { name: "Open activity" }).click();
  await page
    .getByLabel("Session activity")
    .getByRole("button", { name: /Release pulse/ })
    .click();
  await expect(page.getByLabel("Release pulse inspector")).toBeVisible();
  await expect(page.getByTitle("Release pulse")).toBeVisible();
  await expect(page.getByText("Live preview")).toBeVisible();
  await page.screenshot({
    path: testInfo.outputPath(`preview-${testInfo.project.name}.png`),
    fullPage: true,
  });
  await page.getByRole("button", { name: "Close inspector" }).click();
  await expect(page.getByLabel("Release pulse inspector")).toHaveCount(0);
});

test("renders an explicit approval decision", async ({ page }) => {
  await selectSession(page, "Prepare signed macOS build");
  await expect(page.getByText("Your approval is needed")).toBeVisible();
  await page.getByRole("button", { name: "Allow once" }).click();
  await expect(page.getByText("Allowed once")).toBeVisible();
});

test("uses the projected ten-theme catalog and changes pigment", async (
  { page },
  testInfo,
) => {
  await ensureSidebar(page);
  await page.getByRole("button", { name: /Settings/ }).click();
  await expect(page.getByRole("heading", { name: "Appearance" })).toBeVisible();
  await expect(page.locator(".theme-options button")).toHaveCount(10);

  await page.getByRole("button", { name: /Field Notes/ }).click();
  await expect
    .poll(() =>
      page.evaluate(() =>
        getComputedStyle(document.documentElement)
          .getPropertyValue("--theme-pigment")
          .trim(),
      ),
    )
    .toBe("rgb(110 126 53)");
  if (testInfo.project.name === "desktop") {
    await page.screenshot({
      path: testInfo.outputPath("theme-field-notes.png"),
      fullPage: true,
    });
  }

  await page.getByRole("button", { name: /Signal Noir/ }).click();
  await expect
    .poll(() =>
      page.evaluate(() =>
        getComputedStyle(document.documentElement)
          .getPropertyValue("--theme-pigment")
          .trim(),
      ),
    )
    .toBe("rgb(181 44 58)");
  if (testInfo.project.name === "desktop") {
    await page.screenshot({
      path: testInfo.outputPath("theme-signal-noir.png"),
      fullPage: true,
    });
    await page.getByRole("button", { name: /Circuit Garden/ }).click();
    await page.screenshot({
      path: testInfo.outputPath("theme-circuit-garden.png"),
      fullPage: true,
    });
  }
});

test("does not make outbound requests or overflow the viewport", async ({
  page,
}) => {
  const external: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.origin !== "http://127.0.0.1:4178") external.push(request.url());
  });
  await page.reload();
  await selectSession(page, "Review release readiness");
  await page.getByRole("button", { name: "Open activity" }).click();
  await page
    .getByLabel("Session activity")
    .getByRole("button", { name: /Release pulse/ })
    .click();
  await expect(page.getByTitle("Release pulse")).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    )
    .toBe(true);
  expect(external).toEqual([]);
});
