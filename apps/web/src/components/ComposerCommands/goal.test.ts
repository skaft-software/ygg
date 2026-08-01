import { describe, expect, it } from "vitest";
import {
  goalStatusMessage,
  parseGoalCommand,
} from "./goal";

describe("goal composer commands", () => {
  it("parses objectives and lifecycle commands without treating normal prompts as commands", () => {
    expect(parseGoalCommand("/goal ship the release")).toEqual({
      type: "set",
      objective: "ship the release",
    });
    expect(parseGoalCommand("/goal STATUS")).toEqual({ type: "status" });
    expect(parseGoalCommand(" /goal pause ")).toEqual({ type: "pause" });
    expect(parseGoalCommand("/goal resume")).toEqual({ type: "resume" });
    expect(parseGoalCommand("/goal clear")).toEqual({ type: "clear" });
    expect(parseGoalCommand("please /goal ship it")).toBeNull();
  });

  it("reports an empty goal command as usage and formats turn budgets", () => {
    expect(parseGoalCommand("/goal")).toEqual({ type: "help" });
    expect(
      goalStatusMessage({
        objective: "ship the release",
        status: "active",
        turnBudget: 4,
        turnsUsed: 1,
        createdAt: "2026-01-01T00:00:00Z",
      }),
    ).toBe("Active goal: ship the release · 3 turns remaining");
  });
});
