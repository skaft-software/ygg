import { expect, test, type Page } from "@playwright/test";

const viewportByProject: Record<string, { width: number; height: number }> = {
  desktop: { width: 1440, height: 900 },
  "tablet-landscape": { width: 1024, height: 768 },
  "tablet-portrait": { width: 768, height: 1024 },
  mobile: { width: 390, height: 844 },
  "mobile-small": { width: 360, height: 800 },
};

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

async function ensureActivityOpen(page: Page) {
  const rail = page.getByLabel("Session activity");
  const opener = page.getByRole("button", { name: "Open activity" });
  await expect
    .poll(async () => (await rail.isVisible()) || (await opener.isVisible()))
    .toBe(true);
  if (!(await rail.isVisible())) {
    await opener.click();
  }
  await expect(rail).toBeVisible();
  return rail;
}

async function expectNoViewportOverflow(page: Page) {
  await expect
    .poll(() =>
      page.evaluate(() => ({
        documentFits:
          document.documentElement.scrollWidth <= window.innerWidth + 1,
        shellFits:
          document.querySelector(".app-shell")?.getBoundingClientRect().width ===
          window.innerWidth,
      })),
    )
    .toEqual({ documentFits: true, shellFits: true });
}

test.beforeEach(async ({ page }) => {
  await page.goto("/?transport=fixture");
  await expect(
    page.getByRole("heading", { name: "What can I take off your plate?" }),
  ).toBeVisible();
});

test("runs at the locked acceptance viewport", async ({ page }, testInfo) => {
  const expected = viewportByProject[testInfo.project.name];
  expect(expected, `unexpected project ${testInfo.project.name}`).toBeDefined();
  await expect
    .poll(() =>
      page.evaluate(() => ({
        width: window.innerWidth,
        height: window.innerHeight,
      })),
    )
    .toEqual(expected);
});

test("labels simulated fixture mode", async ({ page }) => {
  await expect(
    page.getByText("Demo data · responses and actions are simulated", {
      exact: true,
    }),
  ).toBeVisible();
});

test("opens in a fresh, quiet session with the standard composer", async ({
  page,
}) => {
  await expect(
    page.getByRole("button", { name: "New session", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Pinned", { exact: true })).toBeVisible();
  await expect(page.getByText("Recents", { exact: true })).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toBeVisible();
  await expect(page.getByLabel("Model")).toHaveValue("claude-sonnet-4-6");
  await expect(page.getByLabel("Session activity")).toBeHidden();
});

test("keeps composer controls keyboard focusable with a visible focus ring", async ({
  page,
}) => {
  const attach = page.getByRole("button", { name: "Add files or photos" });
  const model = page.getByLabel("Model");
  const reasoning = page.getByLabel("Reasoning effort");
  const authority = page.getByLabel("Authority");

  await attach.focus();
  await expect(attach).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(model).toBeFocused();
  await expect
    .poll(() =>
      model.evaluate((element) => {
        const style = getComputedStyle(element);
        return `${style.outlineStyle} ${style.outlineWidth}`;
      }),
    )
    .toBe("solid 2px");

  await page.keyboard.press("Tab");
  await expect(reasoning).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(authority).toBeFocused();
});

test("uploads an image, sends it without text, and restores thumbnail focus", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  const picker = page.locator('input[type="file"]');
  await expect(picker).toHaveAttribute("accept", /image\/\*/);
  await picker.setInputFiles({
    name: "tiny.png",
    mimeType: "image/png",
    buffer: Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9ZQmcAAAAASUVORK5CYII=",
      "base64",
    ),
  });

  const staged = page.getByRole("button", {
    name: "Click to preview tiny.png",
  });
  await expect(staged).toBeVisible();
  await expect(
    page.getByLabel("Attached files").getByText("Ready", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Send message" }).click();

  const thumbnail = page.getByRole("button", {
    name: "View attached image 1",
  });
  await expect(thumbnail).toBeVisible();
  await thumbnail.focus();
  await thumbnail.press("Enter");
  await expect(page.getByRole("dialog", { name: "Preview tiny.png" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog", { name: "Preview tiny.png" })).toHaveCount(0);
  await expect(thumbnail).toBeFocused();
});

test("uses one keyboard-operable reasoning slider with static reduced motion", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  const slider = page.getByRole("slider", { name: "Reasoning effort" });
  await expect(slider).toHaveAttribute("aria-valuetext", "high");
  await slider.press("ArrowLeft");
  await expect(slider).toHaveAttribute("aria-valuetext", "medium");
  await slider.press("ArrowRight");
  await expect(slider).toHaveAttribute("aria-valuetext", "high");
  await expect(page.locator(".power-slider-max-fill")).toBeVisible();
  await expect(page.locator(".power-slider-burst")).toBeVisible();

  await page.evaluate(() => {
    document.documentElement.dataset.motion = "none";
  });
  await expect
    .poll(() =>
      page
        .locator(".power-slider-burst i")
        .first()
        .evaluate((element) => getComputedStyle(element).animationDuration),
    )
    .toBe("1e-06s");
});

test("shows typed work and a conditional activity rail", async ({ page }) => {
  await selectSession(page, "Refine onboarding preview");
  await expect(page.getByText("Read onboarding flow")).toBeVisible();
  await expect(page.getByText("Checking the narrow layout")).toBeVisible();
  await ensureActivityOpen(page);
  await expect(page.getByText("Verifying keyboard and touch behavior")).toBeVisible();
  await expect(page.getByRole("button", { name: /Onboarding preview/ })).toBeVisible();
});

test("closes the session actions menu with Escape and restores its trigger", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  const trigger = page.getByRole("button", { name: "Session actions" });
  await trigger.focus();
  await trigger.press("Enter");
  await expect(page.getByRole("menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("menu")).toHaveCount(0);
  await expect(trigger).toBeFocused();
});

test("treats the narrow activity rail as a dismissible keyboard overlay", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "mobile");
  await selectSession(page, "Refine onboarding preview");
  const trigger = page.getByRole("button", { name: "Open activity" });
  await trigger.focus();
  await trigger.press("Enter");

  const rail = page.getByLabel("Session activity");
  const close = rail.getByRole("button", { name: "Close activity" });
  await expect(rail).toBeVisible();
  await expect(close).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(rail).toBeHidden();
  await expect(trigger).toBeFocused();
});

test("opens an output into the dominant preview inspector", async (
  { page },
  testInfo,
) => {
  await selectSession(page, "Review release readiness");
  await ensureActivityOpen(page);
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

test("keeps the activity rail and dominant viewer usable at 1024px", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "tablet-landscape");
  await selectSession(page, "Review release readiness");
  const rail = await ensureActivityOpen(page);
  await expect
    .poll(() =>
      page.evaluate(() => ({
        composerUsable:
          (document.querySelector(".composer")?.getBoundingClientRect().width ??
            0) >= 640,
        railExact:
          Math.abs(
            (document.querySelector(".activity-rail")?.getBoundingClientRect()
              .width ?? 0) - 320,
          ) <= 1,
      })),
    )
    .toEqual({ composerUsable: true, railExact: true });

  await rail.getByRole("button", { name: /Release pulse/ }).click();
  const inspector = page.getByLabel("Release pulse inspector");
  await expect(inspector).toBeVisible();
  await expect
    .poll(() =>
      inspector.evaluate((element) => element.getBoundingClientRect().width),
    )
    .toBeGreaterThanOrEqual(689);
});

test("returns focus to a visible activity trigger after closing an inspector", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await selectSession(page, "Review release readiness");
  await ensureActivityOpen(page);
  const output = page
    .getByLabel("Session activity")
    .getByRole("button", { name: /Release pulse/ });
  await output.focus();
  await output.press("Enter");
  const activityTrigger = page.getByRole("button", { name: "Open activity" });

  const inspector = page.getByLabel("Release pulse inspector");
  await expect
    .poll(() =>
      inspector.evaluate((element) =>
        element.contains(document.activeElement),
      ),
    )
    .toBe(true);
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Release pulse inspector")).toHaveCount(0);
  await expect(activityTrigger).toBeFocused();
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

test("honors reduced motion for live status animation", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Refine onboarding preview");
  const spinner = page.locator(".spin").first();
  await expect(spinner).toBeVisible();
  await expect
    .poll(() =>
      spinner.evaluate((element) => {
        const style = getComputedStyle(element);
        const duration = style.animationDuration.trim();
        const durationMs = duration.endsWith("ms")
          ? Number.parseFloat(duration)
          : Number.parseFloat(duration) * 1_000;
        return {
          durationIsReduced: durationMs <= 0.001,
          iterations: style.animationIterationCount,
        };
      }),
    )
    .toEqual({ durationIsReduced: true, iterations: "1" });
});

test("preserves core flows at a 200-percent equivalent reflow", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.setViewportSize({ width: 720, height: 450 });

  await selectSession(page, "Prepare signed macOS build");
  await expect(page.getByText("Your approval is needed")).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toBeVisible();

  await selectSession(page, "Review release readiness");
  await expect(
    page.getByText("The candidate is ready for review.", { exact: false }),
  ).toBeVisible();
  await ensureActivityOpen(page);
  await page
    .getByLabel("Session activity")
    .getByRole("button", { name: /Release pulse/ })
    .click();
  await expect(page.getByLabel("Release pulse inspector")).toBeVisible();
  await page.getByRole("button", { name: "Close inspector" }).click();
  await expect(page.getByLabel("Release pulse inspector")).toHaveCount(0);
  await expectNoViewportOverflow(page);
});

test("opens and dismisses the mobile sidebar with Escape", async (
  { page },
  testInfo,
) => {
  test.skip(
    testInfo.project.name !== "mobile" &&
      testInfo.project.name !== "mobile-small",
  );
  const trigger = page.getByRole("button", { name: "Open sidebar" });
  await trigger.focus();
  await trigger.press("Enter");
  await expect(
    page.getByRole("button", { name: "New session", exact: true }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(trigger).toBeVisible();
  await expect(trigger).toBeFocused();
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
  await ensureActivityOpen(page);
  await page
    .getByLabel("Session activity")
    .getByRole("button", { name: /Release pulse/ })
    .click();
  await expect(page.getByTitle("Release pulse")).toBeVisible();
  await expectNoViewportOverflow(page);
  expect(external).toEqual([]);
});
