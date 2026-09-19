import type { ReasoningLevel } from "@bb/domain";
import type { PickerOption } from "./OptionPicker";

export function nextCycleValue<T extends string>(
  options: readonly PickerOption<T>[],
  current: T,
): T | null {
  if (options.length === 0) return null;
  const index = options.findIndex((option) => option.value === current);
  const next = options[(index + 1) % options.length];
  if (next === undefined || next.value === current) return null;
  return next.value;
}

export function previousCycleValue<T extends string>(
  options: readonly PickerOption<T>[],
  current: T,
): T | null {
  return nextCycleValue([...options].reverse(), current);
}

/**
 * Cycles the reasoning picker through the levels the current model offers, in
 * the order the agent published them.
 *
 * That order *is* the ladder — pi lists `off`, `minimal`, `low`, … — so ranking
 * by a list loom keeps would reintroduce the fixed vocabulary this picker
 * exists to avoid, and would silently drop any level it does not name. The
 * generic cycle helpers already express "the picker's own order", including a
 * current value that is no longer offered.
 */
export function cycleReasoningValue(
  options: readonly PickerOption<ReasoningLevel>[],
  current: ReasoningLevel,
  direction: "forward" | "backward",
): ReasoningLevel | null {
  return direction === "forward"
    ? nextCycleValue(options, current)
    : previousCycleValue(options, current);
}
