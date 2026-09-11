export type MentionTrigger = "@";

export const DEFAULT_MENTION_TRIGGER: MentionTrigger = "@";
export const MENTION_TRIGGER_VALUES = [
  DEFAULT_MENTION_TRIGGER,
] as const satisfies readonly MentionTrigger[];

export function isMentionTrigger(value: unknown): value is MentionTrigger {
  return value === DEFAULT_MENTION_TRIGGER;
}

export function normalizeMentionTriggers(
  value: unknown,
): readonly MentionTrigger[] | null {
  if (value === undefined) {
    return [DEFAULT_MENTION_TRIGGER];
  }
  if (!Array.isArray(value) || value.length === 0) {
    return null;
  }
  const triggers: MentionTrigger[] = [];
  for (const trigger of value) {
    if (!isMentionTrigger(trigger) || triggers.includes(trigger)) {
      return null;
    }
    triggers.push(trigger);
  }
  return triggers;
}
