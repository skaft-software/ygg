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
  const rail = page.locator(".activity-rail");
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

function releasePulseArtifact(page: Page) {
  return page
    .locator(".activity-rail .resource-list")
    .getByRole("button", { name: /Release pulse/ });
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
  await expect(page.locator(".brand-row .ygg-glyph")).toHaveCount(0);
  await expect(page.locator(".local-identity")).toHaveCount(0);
  await expect(page.getByText("Connected to local ygg")).toHaveCount(0);
  await expect(page.getByText("Pinned", { exact: true })).toBeVisible();
  await expect(page.getByText("Recents", { exact: true })).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(() => {
        const composer = document.querySelector(".composer")!;
        return {
          theme: document.documentElement.dataset.theme,
          border: getComputedStyle(composer).borderColor,
          shimmer: getComputedStyle(composer, "::before").animationName,
        };
      }),
    )
    .toEqual({
      theme: "tidepool",
      border: "rgba(0, 0, 0, 0)",
      shimmer: "none",
    });
  await expect(page.getByRole("button", { name: /Model and effort/ })).toHaveAttribute(
    "data-value",
    "claude-sonnet-4-6",
  );
  await expect(
    page.locator(".activity-rail"),
  ).toBeHidden();
});

test("keeps composer controls keyboard focusable with a visible focus ring", async ({
  page,
}) => {
  const attach = page.getByRole("button", { name: "Add files or photos" });
  const model = page.getByRole("button", { name: /Model and effort/ });
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
  await expect(authority).toBeFocused();
});

test("uses a static model-colored status and quiet blue activity dots", async ({
  page,
}) => {
  const working = page.getByRole("button", {
    name: /Refine onboarding preview, Working/,
  });
  const status = working.locator(".session-status-dot");
  await expect(status).toBeVisible();
  await expect(status).toHaveCSS("animation-name", "none");
  await expect(working).toHaveCSS("--session-model-color", "#10a37f");

  const attention = page.getByRole("button", {
    name: /Prepare signed macOS build, Needs attention/,
  });
  const attentionDot = attention.locator(".session-unread");
  await expect(attentionDot).toBeVisible();
  await expect(attentionDot).toHaveCSS("background-color", "rgb(110, 184, 255)");

  const unread = page.getByRole("button", {
    name: /Review release readiness, Done/,
  });
  await expect(unread.locator(".session-unread")).toBeVisible();
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
  await expect
    .poll(() =>
      staged.locator("img").evaluate((image) => ({
        complete: image.complete,
        naturalWidth: image.naturalWidth,
        source: image.currentSrc,
      })),
    )
    .toEqual({
      complete: true,
      naturalWidth: 1,
      source: expect.stringMatching(/^blob:/),
    });
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
  await page.getByRole("button", { name: /Model and effort/ }).click();
  const slider = page.getByRole("slider", { name: "Reasoning effort" });
  await expect(slider).toHaveAttribute("aria-valuetext", "Max");
  await expect(page.locator(".power-slider-root")).toHaveAttribute(
    "data-overdrive",
    "true",
  );
  await expect(page.locator(".power-slider-fast-particles i")).toHaveCount(10);
  await expect
    .poll(() =>
      page.evaluate(() => ({
        particle: getComputedStyle(
          document.querySelector(".power-slider-fast-particles i")!,
        ).backgroundColor,
        thumb: getComputedStyle(
          document.querySelector(".power-slider-thumb")!,
        ).backgroundColor,
      })),
    )
    .toEqual({
      particle: "rgb(255, 255, 255)",
      thumb: "rgb(255, 255, 255)",
    });
  await slider.press("ArrowLeft");
  await expect(slider).toHaveAttribute("aria-valuetext", "Medium");
  await expect(page.locator(".power-slider-root")).toHaveAttribute(
    "data-overdrive",
    "false",
  );
  await expect(page.locator(".power-slider-fast-particles i")).toHaveCount(0);
  await slider.press("ArrowRight");
  await expect(slider).toHaveAttribute("aria-valuetext", "Max");
  await expect(page.locator(".power-slider-max-fill")).toBeVisible();
  await expect(page.locator(".power-slider-burst")).toHaveCount(0);
  await expect
    .poll(() =>
      page
        .locator(".power-slider-max-fill")
        .evaluate((element) => {
          const style = getComputedStyle(element);
          const flow = getComputedStyle(element, "::after");
          return {
            background: style.backgroundImage,
            animation: flow.animationName,
            duration: flow.animationDuration,
          };
        }),
    )
    .toEqual({
      background: expect.stringContaining("rgb(66, 207, 155)"),
      animation: "power-rainbow-flow",
      duration: "9s",
    });

  await page.evaluate(() => {
    document.documentElement.dataset.motion = "none";
  });
  await expect
    .poll(() =>
      page
        .locator(".power-slider-fast-particles i")
        .first()
        .evaluate((element) => getComputedStyle(element).animationDuration),
    )
    .toBe("1e-06s");
});

test("shows typed work and a conditional activity rail", async ({ page }) => {
  await selectSession(page, "Refine onboarding preview");
  const conversation = page.getByRole("region", { name: "Conversation" });
  await expect(
    conversation.getByText("Read onboarding flow"),
  ).toBeVisible();
  await expect(
    conversation.getByRole("button", {
      name: "Checking the narrow layout",
    }),
  ).toBeVisible();
  await expect
    .poll(() =>
      page.locator(".composer").evaluate((composer) => ({
        border: getComputedStyle(composer).borderColor,
        shimmer: getComputedStyle(composer, "::before").animationName,
        perimeter:
          composer.querySelector(".composer-running-edge-chase") !== null,
      })),
    )
    .toEqual({
      border: "rgba(0, 0, 0, 0)",
      shimmer: "none",
      perimeter: false,
    });
  await expect(page.getByRole("button", { name: "Stop ygg" })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Queue follow-up" }),
  ).toHaveCount(0);
  await ensureActivityOpen(page);
  await expect(page.getByText("Verifying keyboard and touch behavior")).toBeVisible();
  await expect(page.getByRole("button", { name: /Onboarding preview/ })).toBeVisible();
});

test("keeps the 1,000-item performance fixture bounded and quiet", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.goto("/?transport=fixture&fixture=performance");

  const conversation = page.getByRole("region", { name: "Conversation" });
  const transcript = conversation.locator(".transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  await expect(conversation.locator(".command-batch")).toHaveCount(1);
  await expect(
    conversation.locator(".command-batch > summary"),
  ).toContainText("Ran 100 bash commands");
  await expect(
    conversation.locator(".command-batch > summary"),
  ).toContainText("100 succeeded");
  await expect
    .poll(() =>
      conversation
        .locator(".assistant-message:not(.is-streaming)")
        .first()
        .evaluate((element) => getComputedStyle(element).contentVisibility),
    )
    .toBe("auto");
  expect(
    await conversation.evaluate((element) =>
      element
        .getAnimations({ subtree: true })
        .filter(
          (animation) =>
            animation.effect instanceof KeyframeEffect &&
            animation.effect.getTiming().iterations === Infinity,
        ).length,
    ),
  ).toBeLessThanOrEqual(1);
});

test("rehydrates a replayed background run while concurrent sessions remain live", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.goto("/?transport=fixture&fixture=performance");
  await ensureSidebar(page);

  await expect(page.locator(".session-row[data-status='working']")).toHaveCount(
    3,
  );
  await selectSession(page, "Recovered replay after reconnect");
  const transcript = page.locator(".transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "3");
  await expect(transcript).toHaveAttribute("data-session-sequence", "3407");
  await expect(
    page.getByText(
      "Replay is current through sequence 3,407. The background verification is continuing from the recovered projection.",
    ),
  ).toBeVisible();
  await expect(page).toHaveURL(
    /\/session\/session-performance-replay\?transport=fixture&fixture=performance$/,
  );

  await page.reload();
  await expect(transcript).toHaveAttribute("data-session-sequence", "3407");
  await expect(
    page.getByText("Replayed durable session events"),
  ).toBeVisible();
  await ensureSidebar(page);
  await expect(page.locator(".session-row[data-status='working']")).toHaveCount(
    3,
  );

  await selectSession(page, "Profile 1,000-item transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  await expect(transcript).toHaveAttribute("data-session-sequence", "1001");
});

test("does not pull a scrolled-away performance transcript to the latest item", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.goto("/?transport=fixture&fixture=performance");

  const transcript = page.locator(".transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  const scroll = page.locator(".transcript-scroll");
  await scroll.evaluate((element) => {
    element.scrollTop = element.scrollHeight;
    element.tabIndex = -1;
  });
  await scroll.focus();
  await page.keyboard.press("Home");
  await expect(page.getByRole("button", { name: "Jump to latest" })).toBeVisible();
  const manualScrollPosition = await scroll.evaluate((element) => ({
    top: element.scrollTop,
    maximum: element.scrollHeight - element.clientHeight,
  }));
  await page.getByLabel("Message ygg").fill("Stream 60 fixture deltas");
  await page.evaluate(() => {
    const probe = {
      frameTimestamps: [] as number[],
      longTaskObservationSupported:
        PerformanceObserver.supportedEntryTypes.includes("longtask"),
      longTasks: [] as number[],
      running: true,
      startedAt: performance.now(),
      streamCompletedAt: undefined as number | undefined,
      samplingCompletedAt: undefined as number | undefined,
      observer: undefined as PerformanceObserver | undefined,
    };
    const probeWindow = window as typeof window & {
      __yggPerformanceProbe?: typeof probe;
    };
    probeWindow.__yggPerformanceProbe = probe;
    const transcript = document.querySelector(".transcript");
    const completionObserver = new MutationObserver(() => {
      const active = probeWindow.__yggPerformanceProbe;
      if (
        !active?.running ||
        transcript?.getAttribute("data-session-sequence") !== "1061"
      ) {
        return;
      }
      active.streamCompletedAt = performance.now();
      completionObserver.disconnect();
    });
    if (transcript) {
      completionObserver.observe(transcript, {
        attributeFilter: ["data-session-sequence"],
      });
    }
    const frame = (timestamp: number) => {
      const active = probeWindow.__yggPerformanceProbe;
      if (!active?.running) return;
      active.frameTimestamps.push(timestamp);
      if (
        active.streamCompletedAt !== undefined &&
        timestamp - active.streamCompletedAt >= 1_500
      ) {
        active.samplingCompletedAt = timestamp;
        active.running = false;
        active.observer?.disconnect();
        return;
      }
      window.requestAnimationFrame(frame);
    };
    window.requestAnimationFrame(frame);
    if (probe.longTaskObservationSupported) {
      try {
        const observer = new PerformanceObserver((list) => {
          const active = probeWindow.__yggPerformanceProbe;
          if (!active?.running) return;
          for (const entry of list.getEntries()) {
            active.longTasks.push(entry.duration);
          }
        });
        observer.observe({ type: "longtask" });
        probe.observer = observer;
      } catch {
        probe.longTaskObservationSupported = false;
      }
    }
    const submit = document.querySelector<HTMLButtonElement>(
      'button[aria-label="Queue follow-up"]',
    );
    if (!submit) throw new Error("Performance fixture submit button is missing.");
    submit.click();
  });
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  await expect(transcript).toHaveAttribute("data-session-sequence", "1061");
  await expect(
    transcript.locator(".assistant-message.is-streaming").last(),
  ).toContainText("[stream 60/60]");
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (
            window as typeof window & {
              __yggPerformanceProbe?: { running: boolean };
            }
          ).__yggPerformanceProbe?.running,
      ),
    )
    .toBe(false);

  const position = await scroll.evaluate((element) => ({
    top: element.scrollTop,
    maximum: element.scrollHeight - element.clientHeight,
  }));
  const performanceProbe = await page.evaluate(async () => {
    await new Promise<void>((resolve) =>
      window.requestAnimationFrame(() =>
        window.requestAnimationFrame(() => resolve()),
      ),
    );
    const probeWindow = window as typeof window & {
      __yggPerformanceProbe?: {
        frameTimestamps: number[];
        longTaskObservationSupported: boolean;
        longTasks: number[];
        running: boolean;
        startedAt: number;
        streamCompletedAt?: number;
        samplingCompletedAt?: number;
      };
    };
    const probe = probeWindow.__yggPerformanceProbe;
    if (!probe) return null;
    probe.running = false;
    const steadyFrameTimestamps =
      probe.streamCompletedAt === undefined
        ? []
        : probe.frameTimestamps.filter(
            (timestamp) => timestamp >= probe.streamCompletedAt!,
          );
    const firstFrame = steadyFrameTimestamps.at(0);
    const lastFrame = steadyFrameTimestamps.at(-1);
    const frameElapsedMs =
      firstFrame === undefined || lastFrame === undefined
        ? 0
        : lastFrame - firstFrame;
    const frameGaps = probe.frameTimestamps
      .slice(1)
      .map((timestamp, index) => timestamp - probe.frameTimestamps[index]!);
    return {
      elapsedMs:
        (probe.samplingCompletedAt ?? performance.now()) - probe.startedAt,
      streamElapsedMs:
        (probe.streamCompletedAt ?? performance.now()) - probe.startedAt,
      frameCount: probe.frameTimestamps.length,
      frameElapsedMs,
      steadyFramesPerSecond:
        frameElapsedMs > 0
          ? ((steadyFrameTimestamps.length - 1) * 1_000) / frameElapsedMs
          : 0,
      maximumFrameGapMs: Math.max(0, ...frameGaps),
      longTaskObservationSupported: probe.longTaskObservationSupported,
      longTaskCount: probe.longTasks.length,
      maximumLongTaskMs: Math.max(0, ...probe.longTasks),
    };
  });
  await testInfo.attach("performance-delta-burst.json", {
    body: JSON.stringify(performanceProbe, null, 2),
    contentType: "application/json",
  });

  expect(manualScrollPosition.maximum - manualScrollPosition.top).toBeGreaterThan(
    1_000,
  );
  expect(position.maximum - position.top).toBeGreaterThan(1_000);
  expect(position.top).toBeLessThan(position.maximum / 4);
  expect(performanceProbe?.frameCount).toBeGreaterThanOrEqual(80);
  expect(performanceProbe?.steadyFramesPerSecond).toBeGreaterThanOrEqual(55);
  expect(performanceProbe?.streamElapsedMs).toBeLessThan(1_000);
  expect(performanceProbe?.longTaskObservationSupported).toBe(true);
  expect(performanceProbe?.longTaskCount).toBe(0);
  expect(performanceProbe?.maximumLongTaskMs).toBeLessThanOrEqual(50);
  await expect(page.getByRole("button", { name: "Jump to latest" })).toBeVisible();
});

test("matches the settled 1,000-item performance viewport", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  test.skip(
    process.platform !== "darwin",
    "The checked-in visual baseline targets the macOS desktop host.",
  );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/?transport=fixture&fixture=performance");

  const conversation = page.getByRole("region", { name: "Conversation" });
  const transcript = conversation.locator(".transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  await page.evaluate(() => document.fonts.ready);
  await conversation.locator(".transcript-scroll").evaluate((element) => {
    element.scrollTop = 0;
    element.dispatchEvent(new Event("scroll"));
  });

  await expect(conversation).toHaveScreenshot("performance-settled.png", {
    animations: "disabled",
    caret: "hide",
  });
});

test("resizes the desktop activity pane and remembers its width", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await selectSession(page, "Refine onboarding preview");
  await ensureActivityOpen(page);

  const separator = page.getByRole("separator", {
    name: "Resize session activity",
  });
  await expect(separator).toHaveAttribute("aria-valuenow", "320");
  await separator.press("ArrowLeft");
  await expect(separator).toHaveAttribute("aria-valuenow", "336");

  await page.reload();
  await selectSession(page, "Refine onboarding preview");
  await ensureActivityOpen(page);
  await expect(
    page.getByRole("separator", { name: "Resize session activity" }),
  ).toHaveAttribute("aria-valuenow", "336");
});

test("keeps the model picker inside the phone viewport", async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name !== "mobile" &&
      testInfo.project.name !== "mobile-small",
  );
  const trigger = page.getByRole("button", { name: /Model and effort/ });
  await trigger.click();
  const picker = page.getByRole("dialog", { name: "Model and effort" });
  await expect(picker).toBeVisible();
  await expect
    .poll(() =>
      picker.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        return {
          left: Math.round(bounds.left),
          right: Math.round(bounds.right),
          bottom: Math.round(bounds.bottom),
          viewportWidth: window.innerWidth,
          viewportHeight: window.innerHeight,
          position: getComputedStyle(element).position,
        };
      }),
    )
    .toEqual({
      left: 8,
      right: viewportByProject[testInfo.project.name].width - 8,
      bottom: viewportByProject[testInfo.project.name].height - 8,
      viewportWidth: viewportByProject[testInfo.project.name].width,
      viewportHeight: viewportByProject[testInfo.project.name].height,
      position: "fixed",
    });
  await page.keyboard.press("Escape");
  await expect(picker).toHaveCount(0);
  await expect(trigger).toBeFocused();
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

  const rail = page.locator(".activity-rail");
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
  await releasePulseArtifact(page).click();
  await expect(page.getByLabel("Release pulse inspector")).toBeVisible();
  if (testInfo.project.name !== "desktop") {
    await expect
      .poll(() =>
        page
          .getByLabel("Release pulse inspector")
          .evaluate((element) => ({
            opacity: getComputedStyle(element).opacity,
            animation: getComputedStyle(element).animationName,
          })),
      )
      .toEqual({ opacity: "1", animation: "none" });
  }
  await expect(page.getByTitle("Release pulse")).toBeVisible();
  await expect(page.getByText("Live preview")).toBeVisible();
  await page.screenshot({
    path: testInfo.outputPath(`preview-${testInfo.project.name}.png`),
    fullPage: true,
  });
  await page.getByRole("button", { name: "Close inspector" }).click();
  await expect(page.getByLabel("Release pulse inspector")).toHaveCount(0);
});

test("matches the settled mobile inspector overlay", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "mobile");
  test.skip(
    process.platform !== "darwin",
    "The checked-in visual baseline targets the macOS mobile browser host.",
  );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Review release readiness");
  await ensureActivityOpen(page);
  await releasePulseArtifact(page).click();

  const inspector = page.getByLabel("Release pulse inspector");
  await expect(inspector).toBeVisible();
  await expect
    .poll(() =>
      inspector.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        const style = getComputedStyle(element);
        return {
          opacity: style.opacity,
          visibility: style.visibility,
          pointerEvents: style.pointerEvents,
          position: style.position,
          left: Math.round(bounds.left),
          top: Math.round(bounds.top),
          right: Math.round(bounds.right),
          bottom: Math.round(bounds.bottom),
        };
      }),
    )
    .toEqual({
      opacity: "1",
      visibility: "visible",
      pointerEvents: "auto",
      position: "fixed",
      left: 0,
      top: 0,
      right: viewportByProject.mobile.width,
      bottom: viewportByProject.mobile.height,
    });
  await page.evaluate(() => document.fonts.ready);
  await expect(inspector).toHaveScreenshot("mobile-inspector-settled.png", {
    animations: "disabled",
    caret: "hide",
  });
});

test("matches the settled mobile completion review", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "mobile");
  test.skip(
    process.platform !== "darwin",
    "The checked-in visual baseline targets the macOS mobile browser host.",
  );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Review release readiness");
  const review = page.locator(".completion-review:visible").first();
  await expect(review).toBeVisible();
  await review.scrollIntoViewIfNeeded();
  await page.evaluate(() => document.fonts.ready);
  await expect(review).toHaveScreenshot("completion-review-settled.png", {
    animations: "disabled",
    caret: "hide",
  });
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

  await rail
    .locator(".resource-list")
    .getByRole("button", { name: /Release pulse/ })
    .click();
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
  const output = releasePulseArtifact(page);
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

test("opens connected devices only when the host advertises them", async ({
  page,
}) => {
  await ensureSidebar(page);
  await page.getByRole("button", { name: "Connected devices" }).click();
  await expect(
    page.getByRole("heading", { name: "Connected devices" }),
  ).toBeVisible();
  await expect(page.getByText("Secure local network")).toBeVisible();
});

test("honors reduced motion for live status animation", async (
  { page },
  testInfo,
) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Refine onboarding preview");
  const liveDot = page.locator(".live-dots i").first();
  await expect(liveDot).toBeVisible();
  await expect
    .poll(() =>
      liveDot.evaluate((element) => {
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
  await releasePulseArtifact(page).click();
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
  await releasePulseArtifact(page).click();
  await expect(page.getByTitle("Release pulse")).toBeVisible();
  await expectNoViewportOverflow(page);
  expect(external).toEqual([]);
});
