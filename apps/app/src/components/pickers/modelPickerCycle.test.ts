import { describe, expect, it } from "vitest";
import type { ReasoningLevel } from "@bb/domain";
import {
  cycleReasoningValue,
  nextCycleValue,
  previousCycleValue,
} from "./modelPickerCycle";

const options = [
  { value: "a", label: "A" },
  { value: "b", label: "B" },
  { value: "c", label: "C" },
];

describe("nextCycleValue", () => {
  it("wraps from the last option to the first", () => {
    expect(nextCycleValue(options, "b")).toBe("c");
    expect(nextCycleValue(options, "c")).toBe("a");
  });

  it("starts at the first option when the value is absent", () => {
    expect(nextCycleValue(options, "gone")).toBe("a");
  });

  it("returns null when there is nowhere to move", () => {
    expect(nextCycleValue([], "a")).toBeNull();
    expect(nextCycleValue([{ value: "a", label: "A" }], "a")).toBeNull();
  });
});

describe("previousCycleValue", () => {
  it("moves backward and wraps from the first option", () => {
    expect(previousCycleValue(options, "b")).toBe("a");
    expect(previousCycleValue(options, "a")).toBe("c");
  });

  it("starts at the last option when the value is absent", () => {
    expect(previousCycleValue(options, "gone")).toBe("c");
  });

  it("returns null when there is nowhere to move", () => {
    expect(previousCycleValue([], "a")).toBeNull();
    expect(previousCycleValue([{ value: "a", label: "A" }], "a")).toBeNull();
  });
});

describe("cycleReasoningValue", () => {
  // The agent published these in its own order. That order is the ladder, and
  // it is deliberately not bb's canonical rank — `off`/`minimal` are ids bb
  // never named.
  const agentOptions = [
    { value: "off", label: "Off" },
    { value: "minimal", label: "Minimal" },
    { value: "low", label: "Low" },
    { value: "high", label: "High" },
  ] satisfies readonly { value: ReasoningLevel; label: string }[];

  it("cycles in the agent's own ladder order, including ids loom does not name", () => {
    expect(cycleReasoningValue(agentOptions, "off", "forward")).toBe("minimal");
    expect(cycleReasoningValue(agentOptions, "minimal", "forward")).toBe("low");
    expect(cycleReasoningValue(agentOptions, "minimal", "backward")).toBe("off");
  });

  it("does not treat provider response order as meaningful beyond the ladder", () => {
    const unordered = [
      { value: "max", label: "Max" },
      { value: "low", label: "Low" },
      { value: "high", label: "High" },
    ] satisfies readonly { value: ReasoningLevel; label: string }[];
    expect(cycleReasoningValue(unordered, "high", "forward")).toBe("max");
    expect(cycleReasoningValue(unordered, "high", "backward")).toBe("low");
  });

  it("wraps at both edges of the ladder", () => {
    expect(cycleReasoningValue(agentOptions, "high", "forward")).toBe("off");
    expect(cycleReasoningValue(agentOptions, "off", "backward")).toBe("high");
  });

  it("enters the ladder when the current effort is not offered", () => {
    expect(cycleReasoningValue(agentOptions, "medium", "forward")).toBe("off");
    expect(cycleReasoningValue(agentOptions, "medium", "backward")).toBe("high");
  });

  it("returns null when there is nowhere to move", () => {
    expect(
      cycleReasoningValue(
        [{ value: "high", label: "High" }],
        "high",
        "forward",
      ),
    ).toBeNull();
    expect(cycleReasoningValue([], "medium", "forward")).toBeNull();
  });
});
