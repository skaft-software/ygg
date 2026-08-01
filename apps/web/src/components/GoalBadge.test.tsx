import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { GoalState } from "../protocol";
import { GoalBadge } from "./GoalBadge";

const goal: GoalState = {
  objective: "ship the release",
  status: "active",
  turnBudget: 4,
  turnsUsed: 1,
  createdAt: "2026-01-01T00:00:00Z",
};

describe("GoalBadge", () => {
  it("shows the goal lifecycle and working state", () => {
    const { rerender } = render(<GoalBadge goal={goal} />);
    expect(screen.getByLabelText("Goal Active: ship the release")).toHaveAttribute(
      "data-status",
      "active",
    );
    expect(screen.getByText("3 left")).toBeVisible();

    rerender(<GoalBadge goal={goal} working />);
    expect(screen.getByText("Goal Working")).toBeVisible();
    expect(screen.getByLabelText("Goal Working: ship the release")).toHaveClass(
      "is-working",
    );
  });

  it("renders no badge without a configured goal", () => {
    const { container } = render(<GoalBadge goal={null} />);
    expect(container).toBeEmptyDOMElement();
  });
});
