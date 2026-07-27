/// <reference types="vite/client" />

import {
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
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

  it("exposes durable redacted command history without inventing output links", () => {
    render(
      <ActivityRail
        session={structuredClone(fixtureSessions["session-done"]!)}
        open
        onClose={vi.fn()}
        onOpenOutput={vi.fn()}
        onOpenSource={vi.fn()}
        onOpenResource={vi.fn()}
        modal={false}
        onRestoreFocus={vi.fn()}
        resourcesAvailable
      />,
    );

    const heading = screen.getByText("Command history");
    fireEvent.click(heading);
    const section = heading.closest("details");
    expect(section).not.toBeNull();
    expect(
      within(section!).getByText("cargo test --workspace"),
    ).toBeVisible();
    expect(within(section!).getByText(/83[,.]?240ms · exit 0/)).toBeVisible();
    expect(within(section!).queryByRole("button", { name: "Output" })).toBeNull();
  });
});
