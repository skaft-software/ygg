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
  await page.locator("button.session-row").filter({ hasText: title }).click();
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
          document.querySelector(".app-shell")?.getBoundingClientRect()
            .width === window.innerWidth,
      })),
    )
    .toEqual({ documentFits: true, shellFits: true });
}

test.beforeEach(async ({ page }) => {
  await page.goto("/?transport=fixture");
  await expect(
    page.getByRole("heading", { name: "What should we work on?" }),
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

test("browses and saves a trusted project file", async ({ page }) => {
  await ensureSidebar(page);
  await page.getByRole("button", { name: "Files", exact: true }).click();

  await expect(page.getByRole("heading", { name: "Files", exact: true })).toBeVisible();
  await page.getByRole("button", { name: /README\.md/ }).click();
  await expect(page.getByRole("heading", { name: "Fixture" })).toBeVisible();
  await page.getByRole("button", { name: "Edit Markdown" }).click();
  const editor = page.getByRole("textbox", { name: "Contents of README.md" });
  await expect(editor).toBeVisible();
  await editor.fill("# Updated fixture project\n");
  await expect(page.getByText("Unsaved", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Unsaved", { exact: true })).toHaveCount(0);
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
  await expect(
    page
      .getByRole("tabpanel", { name: "Active sessions" })
      .getByText("Sessions", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "ygg", exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Research notes")).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(() => {
        const composer = document.querySelector(".composer")!;
        return {
          theme: document.documentElement.dataset.theme ?? null,
          shimmer: getComputedStyle(composer, "::before").animationName,
          perimeter:
            composer.querySelector(".composer-running-edge-chase") !== null,
        };
      }),
    )
    .toEqual({
      theme: null,
      shimmer: "none",
      perimeter: false,
    });
  await expect(
    page.getByRole("button", { name: /Model and effort/ }),
  ).toHaveAttribute("data-value", "claude-sonnet-4-6");
  await expect(page.locator(".activity-rail")).toBeHidden();
});

test("keeps composer controls keyboard focusable with a visible focus ring", async ({
  page,
}) => {
  const attach = page.getByRole("button", { name: "Add files or photos" });
  const context = page.getByRole("button", { name: "Context", exact: true });
  const model = page.getByRole("button", { name: /Model and effort/ });
  const authority = page.getByLabel("Authority");

  await attach.focus();
  await expect(attach).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(context).toBeFocused();
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

test("shows pull-request marks only for structured PR evidence", async ({
  page,
}) => {
  await ensureSidebar(page);

  const noEvidence = page.getByRole("button", {
    name: "Open session New session, Ready",
  });
  await expect(noEvidence.locator(".session-pull-request")).toHaveCount(0);
  await expect(noEvidence).toHaveText("New session");

  const inProgress = page.getByRole("button", {
    name: /Refine onboarding preview, Working, Pull request in progress/,
  });
  await expect(inProgress).toHaveText("Refine onboarding preview");
  await expect(inProgress.locator(".session-pull-request")).toHaveCSS(
    "color",
    "rgb(139, 143, 152)",
  );

  const ready = page.getByRole("button", {
    name: /Prepare signed macOS build, Needs attention, Pull request ready for review/,
  });
  await expect(ready).toHaveText("Prepare signed macOS build");
  await expect(ready.locator(".session-pull-request")).toHaveCSS(
    "color",
    "rgb(82, 199, 123)",
  );

  const merged = page.getByRole("button", {
    name: /Review release readiness, Done, Pull request merged/,
  });
  await expect(merged).toHaveText("Review release readiness");
  await expect(merged.locator(".session-pull-request")).toHaveCSS(
    "color",
    "rgb(167, 139, 250)",
  );
});

test("uploads an image, sends it without text, and restores thumbnail focus", async ({
  page,
}, testInfo) => {
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
  await expect(
    page.getByRole("dialog", { name: "Preview tiny.png" }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("dialog", { name: "Preview tiny.png" }),
  ).toHaveCount(0);
  await expect(thumbnail).toBeFocused();
});

test("uses blue effort fill, sparkling white dots, and a max-only rainbow", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.getByRole("button", { name: /Model and effort/ }).click();
  const slider = page.getByRole("slider", { name: "Reasoning effort" });
  const root = page.locator(".power-slider-root");
  const track = page.locator(".power-slider-track");
  const range = page.locator(".power-slider-range");
  const fill = page.locator(".power-slider-max-fill");
  const particles = page.locator(".power-slider-fast-particles");
  const particle = particles.locator(":scope > span").first();
  const compactEffort = page.locator(".model-picker-trigger-effort");
  const ordinaryFillGeometry = () =>
    page.evaluate(() => {
      const track = document.querySelector<HTMLElement>(".power-slider-track")!;
      const range = document.querySelector<HTMLElement>(".power-slider-range")!;
      const thumb = document.querySelector<HTMLElement>(".power-slider-thumb")!;
      const trackRect = track.getBoundingClientRect();
      const rangeRect = range.getBoundingClientRect();
      const thumbRect = thumb.getBoundingClientRect();
      return {
        fillRatio: rangeRect.width / trackRect.width,
        thumbEndGap: trackRect.right - thumbRect.right,
        thumbOverlap: rangeRect.right - (thumbRect.left + thumbRect.width / 2),
      };
    });

  await expect(compactEffort).toHaveCSS("background-image", "none");
  await expect(slider).toHaveAttribute("aria-valuetext", "Max");
  await expect(root).toHaveAttribute("data-max", "false");
  await expect(root).toHaveAttribute("data-overdrive", "false");
  await expect(range).toHaveCSS("background-color", "rgb(79, 141, 247)");
  await expect(range).toHaveCSS("background-image", "none");
  await expect(fill).toHaveCSS("animation-name", "none");
  await expect(fill).toHaveCSS("opacity", "0");
  await expect(particles).toHaveCSS("opacity", "0");
  await expect(particles.locator(":scope > span")).toHaveCount(12);
  const highGeometry = await ordinaryFillGeometry();
  expect(highGeometry.fillRatio).toBeGreaterThan(0.9);
  expect(highGeometry.fillRatio).toBeLessThan(1);
  expect(highGeometry.thumbOverlap).toBeCloseTo(6, 0);
  expect(highGeometry.thumbEndGap).toBeCloseTo(0, 0);

  await root.evaluate((element) => {
    element.dataset.overdrive = "true";
  });
  await expect(particles).toHaveCSS("opacity", "0.9");
  await expect(particle).toHaveCSS(
    "animation-name",
    "reasoning-particle-float",
  );
  const floatingParticle = await particle.evaluate((element) => {
    const style = getComputedStyle(element);
    const animation = element.getAnimations()[0];
    const keyframes =
      animation?.effect instanceof KeyframeEffect
        ? animation.effect.getKeyframes()
        : [];
    return {
      background: style.backgroundColor,
      width: style.width,
      height: style.height,
      radius: style.borderRadius,
      changesTrackPosition: keyframes.some((frame) => "left" in frame),
    };
  });
  expect(floatingParticle).toEqual({
    background: "rgb(255, 255, 255)",
    width: "2px",
    height: "2px",
    radius: "50%",
    changesTrackPosition: false,
  });

  const particleSizes = await particles
    .locator(":scope > span")
    .evaluateAll((dots) =>
      [...new Set(dots.map((dot) => getComputedStyle(dot).width))].sort(),
    );
  expect(particleSizes).toEqual(["1.5px", "2.5px", "2px", "3px"]);

  await root.evaluate((element) => {
    element.dataset.overdrive = "false";
    element.dataset.max = "true";
  });
  await expect(particles).toHaveCSS("opacity", "0.9");
  await expect(particle).toHaveCSS(
    "animation-name",
    "reasoning-particle-float",
  );
  await expect(range).toHaveCSS("opacity", "0");
  await expect(fill).toHaveCSS("opacity", "1");
  await expect(fill).toHaveCSS("animation-name", "reasoning-spectrum-shift");
  await expect(fill).toHaveCSS("background-repeat", "no-repeat");
  await expect(fill).toHaveCSS("background-size", "200% 100%");
  const maxPresentation = await fill.evaluate((element) => {
    const style = getComputedStyle(element);
    const keyframes = element
      .getAnimations()
      .flatMap((animation) =>
        animation.effect instanceof KeyframeEffect
          ? animation.effect.getKeyframes()
          : [],
      )
      .filter((frame) => frame.backgroundPositionX !== undefined);
    return {
      image: style.backgroundImage,
      animationEnd: String(keyframes.at(-1)?.backgroundPositionX ?? ""),
      fillRatio:
        element.getBoundingClientRect().width /
        document
          .querySelector<HTMLElement>(".power-slider-track")!
          .getBoundingClientRect().width,
      thumb: getComputedStyle(
        document.querySelector<HTMLElement>(".power-slider-thumb")!,
      ).backgroundColor,
    };
  });
  expect(maxPresentation.image).toContain("linear-gradient");
  expect(maxPresentation.animationEnd).toContain("100%");
  expect(maxPresentation.fillRatio).toBeCloseTo(1, 2);
  expect(maxPresentation.thumb).toBe("rgb(236, 236, 239)");

  await root.evaluate((element) => {
    element.dataset.max = "false";
  });
  await slider.press("ArrowLeft");
  await expect(slider).toHaveAttribute("aria-valuetext", "Medium");
  await expect(root).toHaveAttribute("data-max", "false");
  await expect(root).toHaveAttribute("data-overdrive", "false");
  await expect
    .poll(async () => {
      const rangeRect = await range.evaluate((element) => {
        const rect = element.getBoundingClientRect();
        return { right: rect.right, width: rect.width };
      });
      const thumbCenter = await page
        .locator(".power-slider-thumb")
        .evaluate((element) => {
          const rect = element.getBoundingClientRect();
          return rect.left + rect.width / 2;
        });
      const trackWidth = await track.evaluate(
        (element) => element.getBoundingClientRect().width,
      );
      const fillRatio = rangeRect.width / trackWidth;
      return (
        fillRatio > 0.5 && fillRatio < 0.6 && rangeRect.right - thumbCenter >= 5
      );
    })
    .toBe(true);
  const mediumGeometry = await page.evaluate(() => {
    const track = document.querySelector<HTMLElement>(".power-slider-track")!;
    const range = document.querySelector<HTMLElement>(".power-slider-range")!;
    const thumb = document.querySelector<HTMLElement>(".power-slider-thumb")!;
    const trackRect = track.getBoundingClientRect();
    const rangeRect = range.getBoundingClientRect();
    const thumbRect = thumb.getBoundingClientRect();
    return {
      fillRatio: rangeRect.width / trackRect.width,
      thumbOverlap: rangeRect.right - (thumbRect.left + thumbRect.width / 2),
      rightRadius: getComputedStyle(range).borderTopRightRadius,
    };
  });
  expect(mediumGeometry.fillRatio).toBeGreaterThan(0.5);
  expect(mediumGeometry.fillRatio).toBeLessThan(0.6);
  expect(mediumGeometry.thumbOverlap).toBeGreaterThanOrEqual(5);
  expect(mediumGeometry.rightRadius).toBe("0px");

  await slider.press("ArrowRight");
  await expect(slider).toHaveAttribute("aria-valuetext", "Max");
  await expect
    .poll(async () => {
      const geometry = await ordinaryFillGeometry();
      return (
        geometry.fillRatio > 0.9 &&
        geometry.fillRatio < 1 &&
        geometry.thumbOverlap >= 5 &&
        Math.abs(geometry.thumbEndGap) < 0.5
      );
    })
    .toBe(true);

  await root.evaluate((element) => {
    element.dataset.max = "true";
    element.dataset.overdrive = "true";
  });
  await page.evaluate(() => {
    document.documentElement.dataset.motion = "none";
  });
  await expect(particles).toHaveCSS("display", "none");
  await expect
    .poll(() =>
      fill.evaluate((element) => {
        const style = getComputedStyle(element);
        return {
          animation: style.animationName,
          image: style.backgroundImage,
          transition: style.transitionDuration,
        };
      }),
    )
    .toEqual({
      animation: "none",
      image: expect.stringContaining("linear-gradient"),
      transition: "0s",
    });
});

test("keeps the transcript on a compact conversational cadence", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  await selectSession(page, "Review release readiness");

  const geometry = await page.evaluate(() => {
    const transcript = document.querySelector<HTMLElement>(".transcript")!;
    const userMessage = transcript.querySelector<HTMLElement>(".user-message")!;
    const composer = document.querySelector<HTMLElement>(".composer")!;
    const transcriptBounds = transcript.getBoundingClientRect();
    const userBounds = userMessage.getBoundingClientRect();
    const composerBounds = composer.getBoundingClientRect();
    const transcriptStyle = getComputedStyle(transcript);
    const contentLeft =
      transcriptBounds.left + Number.parseFloat(transcriptStyle.paddingLeft);
    const contentRight =
      transcriptBounds.right - Number.parseFloat(transcriptStyle.paddingRight);

    return {
      contentWidth: contentRight - contentLeft,
      gap: transcriptStyle.gap,
      composerWidth: composerBounds.width,
      composerAligned:
        Math.abs(composerBounds.left - contentLeft) < 0.5 &&
        Math.abs(composerBounds.right - contentRight) < 0.5,
      userWidth: userBounds.width,
      userRightAligned: Math.abs(userBounds.right - contentRight) < 0.5,
      userPadding: getComputedStyle(userMessage).padding,
    };
  });

  expect(geometry).toEqual({
    contentWidth: 740,
    gap: "16px",
    composerWidth: 740,
    composerAligned: true,
    userWidth: 518,
    userRightAligned: true,
    userPadding: "8px 12px",
  });

  await selectSession(page, "Summarize provider notes");
  const shortMessage = await page.evaluate(() => {
    const transcript = document.querySelector<HTMLElement>(".transcript")!;
    const message = transcript.querySelector<HTMLElement>(".user-message")!;
    const transcriptBounds = transcript.getBoundingClientRect();
    const messageBounds = message.getBoundingClientRect();
    const transcriptStyle = getComputedStyle(transcript);
    const contentRight =
      transcriptBounds.right - Number.parseFloat(transcriptStyle.paddingRight);
    return {
      width: messageBounds.width,
      rightAligned: Math.abs(messageBounds.right - contentRight) < 0.5,
    };
  });
  expect(shortMessage.width).toBeLessThan(400);
  expect(shortMessage.rightAligned).toBe(true);
});

test("shows typed work and a conditional activity rail", async ({ page }) => {
  await selectSession(page, "Refine onboarding preview");
  const conversation = page.getByRole("region", { name: "Conversation" });
  const historySummary = conversation.getByRole("button", {
    name: "Working",
    exact: true,
  });
  await expect(historySummary).toBeVisible();
  await expect(historySummary).toHaveAttribute("aria-expanded", "false");
  await expect(historySummary.locator(".work-group-glyph")).toHaveClass(
    /is-live/,
  );
  const historyContent = conversation.locator(".work-group-content-clip");
  await expect(historyContent).toHaveAttribute("inert", "");
  await expect(conversation.locator(".work-group-live-item")).toHaveCount(0);
  const liveReasoning = conversation.locator(
    ".work-group-content .reasoning-block",
  );
  await expect(liveReasoning).toContainText("Checking the narrow layout");
  await expect(liveReasoning).not.toHaveAttribute("open", "");
  await historySummary.click();
  await expect(historySummary).toHaveAttribute("aria-expanded", "true");
  await expect(historyContent).not.toHaveAttribute("inert");
  await expect(conversation.getByText("Read onboarding flow")).toBeVisible();
  const expandedAction = conversation
    .locator(".action-cell")
    .filter({ hasText: "Read onboarding flow" });
  await expandedAction.locator(":scope > summary").click();
  await expect(expandedAction).toHaveAttribute("open", "");
  expect(
    await expandedAction.evaluate((action) => {
      const detail = action.querySelector<HTMLElement>(".action-detail")!;
      const surfaces = [action, action.querySelector("summary")!, detail];
      return {
        borderless: surfaces.every((surface) => {
          const style = getComputedStyle(surface);
          return [
            style.borderTopWidth,
            style.borderRightWidth,
            style.borderBottomWidth,
            style.borderLeftWidth,
          ].every((width) => width === "0px");
        }),
        transparentRow:
          getComputedStyle(action).backgroundColor === "rgba(0, 0, 0, 0)",
        tonalDetail:
          getComputedStyle(detail).backgroundColor !== "rgba(0, 0, 0, 0)",
      };
    }),
  ).toEqual({ borderless: true, tonalDetail: true, transparentRow: true });
  await expect
    .poll(() =>
      page.locator(".composer").evaluate((composer) => {
        const style = getComputedStyle(composer);
        const wrap = composer.closest<HTMLElement>(".composer-wrap");
        return {
          borderWidth: style.borderWidth,
          shadedSurface:
            style.backgroundColor !== "rgba(0, 0, 0, 0)" &&
            style.backgroundColor !== getComputedStyle(wrap!).backgroundColor,
          shimmer: getComputedStyle(composer, "::before").animationName,
          perimeter:
            composer.querySelector(".composer-running-edge-chase") !== null,
        };
      }),
    )
    .toEqual({
      borderWidth: "0px",
      shadedSurface: true,
      shimmer: "none",
      perimeter: true,
    });
  await expect
    .poll(() =>
      page
        .locator(".composer-running-edge-chase")
        .evaluate((edge) => getComputedStyle(edge).animationName),
    )
    .toBe("composer-ring-chase");
  await expect(page.getByRole("button", { name: "Stop ygg" })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Queue follow-up" }),
  ).toHaveCount(0);
  await ensureActivityOpen(page);
  await expect(
    page.getByText("Verifying keyboard and touch behavior"),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Onboarding preview/ }),
  ).toBeVisible();
});

test("matches the settled desktop workbench shell", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  test.skip(
    process.platform !== "darwin",
    "The checked-in visual baseline targets the macOS desktop host.",
  );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Review release readiness");
  await ensureActivityOpen(page);
  await expect(
    page.locator(".activity-rail").getByText("release-pulse.html", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByLabel("Message ygg")).toHaveAttribute(
    "placeholder",
    "Reply…",
  );
  await page.evaluate(() => document.fonts.ready);

  await expect(page.locator(".app-shell")).toHaveScreenshot(
    "workbench-shell-settled.png",
    {
      animations: "disabled",
      caret: "hide",
    },
  );
});

test("keeps the 1,000-item performance fixture bounded and quiet", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.goto("/?transport=fixture&fixture=performance");

  const conversation = page.getByRole("region", { name: "Conversation" });
  const transcript = conversation.locator(".transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  const commandGroup = conversation.locator(
    ".work-group:has(.command-batch)",
  );
  await expect(commandGroup).toHaveCount(1);
  await expect(commandGroup.locator(".work-group-summary")).toContainText(
    "Ran commands",
  );
  await expect(
    commandGroup.locator(".work-group-summary"),
  ).not.toContainText(/bash|succeeded/i);
  await expect(commandGroup.locator(".command-batch > summary")).toHaveCount(0);
  await expect
    .poll(() =>
      conversation
        .locator(".assistant-message:not(.is-streaming)")
        .first()
        .evaluate((element) => getComputedStyle(element).contentVisibility),
    )
    .toBe("auto");
  expect(
    await conversation.evaluate(
      (element) =>
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

test("rehydrates a replayed background run while concurrent sessions remain live", async ({
  page,
}, testInfo) => {
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
  await expect(page.getByText("Replayed durable session events")).toBeVisible();
  await ensureSidebar(page);
  await expect(page.locator(".session-row[data-status='working']")).toHaveCount(
    3,
  );

  await selectSession(page, "Profile 1,000-item transcript");
  await expect(transcript).toHaveAttribute("data-item-count", "1000");
  await expect(transcript).toHaveAttribute("data-session-sequence", "1001");
});

test("does not pull a scrolled-away performance transcript to the latest item", async ({
  page,
}, testInfo) => {
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
  await expect(
    page.getByRole("button", { name: "Jump to latest" }),
  ).toBeVisible();
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
      longTaskRecords: [] as { duration: number; startTime: number }[],
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
        timestamp - active.streamCompletedAt >= 2_500
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
            active.longTaskRecords.push({
              duration: entry.duration,
              startTime: entry.startTime,
            });
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
    if (!submit)
      throw new Error("Performance fixture submit button is missing.");
    submit.click();
  });
  await page.evaluate(() =>
    new Promise<void>((resolve) => {
      const check = () => {
        const transcript = document.querySelector('.transcript');
        const probe = (window as typeof window & {
          __yggPerformanceProbe?: { running: boolean };
        }).__yggPerformanceProbe;
        const streamingMessage = transcript?.querySelector<HTMLElement>(
          '.assistant-message.is-streaming',
        );
        const messageText = streamingMessage?.textContent ?? '';
        if (
          transcript?.getAttribute('data-item-count') === '1000' &&
          transcript?.getAttribute('data-session-sequence') === '1061' &&
          messageText.includes('[stream 60/60]') &&
          probe?.running === false
        ) {
          resolve();
          return;
        }
        requestAnimationFrame(check);
      };
      requestAnimationFrame(check);
    }),
  );

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
        longTaskRecords: { duration: number; startTime: number }[];
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
    const longTasksInSamplingWindow = probe.longTaskRecords.filter(
      (entry) =>
        // Treat long tasks occurring after stream completion as post-stream pressure,
        // which matches the benchmark scope we're trying to measure.
        probe.streamCompletedAt === undefined ||
        (entry.startTime >= probe.streamCompletedAt - 10 &&
          (probe.samplingCompletedAt ?? performance.now()) >= entry.startTime),
    );
    const longTaskDurationsInWindow = longTasksInSamplingWindow.map(
      (entry) => entry.duration,
    );
    const longTaskSamples = longTasksInSamplingWindow
      .map((entry) => ({
        duration: entry.duration,
        startTime: entry.startTime,
      }))
      .slice(0, 5);
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
      longTaskCount: longTaskDurationsInWindow.length,
      longTaskSamples,
      maximumLongTaskMs: Math.max(0, ...longTaskDurationsInWindow),
      streamCompletedAt: probe.streamCompletedAt,
      startedAt: probe.startedAt,
      samplingCompletedAt: probe.samplingCompletedAt,
    };
  });
  await testInfo.attach("performance-delta-burst.json", {
    body: JSON.stringify(performanceProbe, null, 2),
    contentType: "application/json",
  });

  expect(
    manualScrollPosition.maximum - manualScrollPosition.top,
  ).toBeGreaterThan(1_000);
  expect(position.maximum - position.top).toBeGreaterThan(1_000);
  expect(position.top).toBeLessThan(position.maximum / 4);
  expect(performanceProbe?.frameCount).toBeGreaterThanOrEqual(80);
  expect(performanceProbe?.steadyFramesPerSecond).toBeGreaterThanOrEqual(55);
  expect(performanceProbe?.streamElapsedMs).toBeLessThan(1_000);
  expect(performanceProbe?.longTaskObservationSupported).toBe(true);
  expect(performanceProbe?.longTaskCount).toBe(0);
  expect(performanceProbe?.maximumLongTaskMs).toBeLessThanOrEqual(50);
  await expect(
    page.getByRole("button", { name: "Jump to latest" }),
  ).toBeVisible();
});

test("matches the settled 1,000-item performance viewport", async ({
  page,
}, testInfo) => {
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

test("resizes the desktop activity pane and remembers its width", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  await selectSession(page, "Refine onboarding preview");
  await ensureActivityOpen(page);

  const separator = page.getByRole("separator", {
    name: "Resize session activity",
  });
  await expect(separator).toHaveAttribute("aria-valuenow", "400");
  await separator.press("ArrowLeft");
  await expect(separator).toHaveAttribute("aria-valuenow", "416");

  await page.reload();
  await selectSession(page, "Refine onboarding preview");
  await ensureActivityOpen(page);
  await expect(
    page.getByRole("separator", { name: "Resize session activity" }),
  ).toHaveAttribute("aria-valuenow", "416");
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

test("closes the session actions menu with Escape and restores its trigger", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  const trigger = page.getByRole("button", { name: "Session actions" });
  await trigger.focus();
  await trigger.press("Enter");
  await expect(page.getByRole("menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("menu")).toHaveCount(0);
  await expect(trigger).toBeFocused();
});

test("treats the narrow activity rail as a dismissible keyboard overlay", async ({
  page,
}, testInfo) => {
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

test("opens an output into the dominant preview inspector", async ({
  page,
}, testInfo) => {
  await selectSession(page, "Review release readiness");
  await ensureActivityOpen(page);
  await releasePulseArtifact(page).click();
  await expect(page.getByLabel("Release pulse inspector")).toBeVisible();
  if (testInfo.project.name !== "desktop") {
    await expect
      .poll(() =>
        page.getByLabel("Release pulse inspector").evaluate((element) => ({
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

test("does not render mobile completion review", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "mobile");
  test.skip(
    process.platform !== "darwin",
    "The checked-in visual baseline targets the macOS mobile browser host.",
  );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Review release readiness");
  await expect(page.locator(".completion-review-disclosure")).toHaveCount(0);
  await expect(page.locator(".completion-review")).toHaveCount(0);
});

test("keeps the activity rail and dominant viewer usable at 1024px", async ({
  page,
}, testInfo) => {
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

test("returns focus to a visible activity trigger after closing an inspector", async ({
  page,
}, testInfo) => {
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
      inspector.evaluate((element) => element.contains(document.activeElement)),
    )
    .toBe(true);
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Release pulse inspector")).toHaveCount(0);
  await expect(activityTrigger).toBeFocused();
});

test("renders an explicit approval decision", async ({ page }) => {
  await selectSession(page, "Prepare signed macOS build");
  await expect(page.getByText("Your approval is needed")).toBeVisible();
  await expect(page.locator(".composer-running-edge")).toHaveCount(0);
  await page.getByRole("button", { name: "Allow once" }).click();
  await expect(page.getByText("Allowed once")).toBeVisible();
});

test("uses one fixed workbench appearance", async ({ page }) => {
  await ensureSidebar(page);
  await page.getByRole("button", { name: /Settings/ }).click();
  await expect(
    page.getByRole("heading", { name: "Interface type" }),
  ).toBeVisible();
  await expect(page.getByRole("heading", { name: "Appearance" })).toHaveCount(
    0,
  );
  await expect(page.locator(".theme-options")).toHaveCount(0);
  await expect(page.locator("html")).not.toHaveAttribute("data-theme");
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

test("honors reduced motion for live status animation", async ({
  page,
}, testInfo) => {
  test.skip(testInfo.project.name !== "desktop");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await selectSession(page, "Refine onboarding preview");
  const liveDot = page.locator(".live-dots i").first();
  const runningEdge = page.locator(".composer-running-edge-chase");
  await expect(liveDot).toBeVisible();
  await expect
    .poll(() =>
      runningEdge.evaluate((element) => getComputedStyle(element).animationName),
    )
    .toBe("none");
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

test("preserves core flows at a 200-percent equivalent reflow", async ({
  page,
}, testInfo) => {
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

test("opens and dismisses the mobile sidebar with Escape", async ({
  page,
}, testInfo) => {
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
