/// <reference types="vite/client" />

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fixtureSessions } from "../fixtures";
import { ActivityRail } from "./ActivityRail";

describe("activity rail", () => {
  afterEach(cleanup);

  it("keeps work detail in the transcript while exposing session resources", () => {
    render(
      <ActivityRail
        session={structuredClone(fixtureSessions["session-live"]!)}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    expect(screen.getByText("Progress")).toBeVisible();
    expect(
      screen.getByText("Verifying keyboard and touch behavior"),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: /Onboarding preview/ }),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: /OnboardingFlow\.tsx/ }),
    ).toBeVisible();

    expect(screen.queryByText("Activity")).toBeNull();
    expect(screen.queryByText("Read onboarding flow")).toBeNull();
    expect(screen.queryByText("Checking the narrow layout")).toBeNull();
  });
});
