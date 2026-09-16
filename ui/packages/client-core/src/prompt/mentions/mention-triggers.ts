export type PluginMentionTrigger = "@" | "#" | "$" | "!" | "~";
export type MentionTrigger = PluginMentionTrigger;

export const DEFAULT_MENTION_TRIGGER: MentionTrigger = "@";
export const MENTION_TRIGGER_VALUES = [
  DEFAULT_MENTION_TRIGGER,
] as const satisfies readonly MentionTrigger[];

// Accepted only when decoding existing drafts and clipboard content. The
// active Loom composer advertises MENTION_TRIGGER_VALUES, which contains @.
export const PLUGIN_MENTION_TRIGGER_VALUES = [
  "@",
  "#",
  "$",
  "!",
  "~",
] as const satisfies readonly PluginMentionTrigger[];
