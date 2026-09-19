import type { ProviderInfo, ReasoningLevel } from "@bb/domain";

/**
 * Labels for the level ids this build already knows.
 *
 * An id outside the table — any agent's own naming, such as a level added after
 * this build — falls through to the id itself rather than disappearing from the
 * picker. `off` and `minimal` are here because pi advertises them.
 */
const FALLBACK_REASONING_LABELS: Record<string, string> = {
  off: "Off",
  minimal: "Minimal",
  none: "None",
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Extra High",
  ultracode: "Ultracode",
  max: "Max",
  ultra: "Ultra",
};

export type ReasoningLabelSource = Pick<ProviderInfo, "reasoningLevels">;

export function reasoningLevelLabel(
  level: ReasoningLevel,
  provider: ReasoningLabelSource | undefined,
): string {
  const declared = provider?.reasoningLevels?.find(
    (option) => option.id === level,
  );
  return declared?.label ?? FALLBACK_REASONING_LABELS[level] ?? level;
}

const FAST_SERVICE_TIER_ID = "fast";

export function fastServiceTierLabel(
  provider: Pick<ProviderInfo, "serviceTiers"> | undefined,
): string {
  return (
    provider?.serviceTiers?.find((tier) => tier.id === FAST_SERVICE_TIER_ID)
      ?.label ?? "Fast"
  );
}
