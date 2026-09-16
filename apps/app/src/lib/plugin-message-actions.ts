import type { MarkdownMessageDirectiveOpenThreadPanel } from "@/components/ui/markdown-message-directives";
import type { PluginMessageActionSlot } from "./plugin-slots";

interface RunPluginMessageActionArgs {
  slot: PluginMessageActionSlot;
  threadId: string;
  message: unknown;
  selectedText?: string;
  openThreadPanel: MarkdownMessageDirectiveOpenThreadPanel | undefined;
}

export function runPluginMessageAction(
  _args: RunPluginMessageActionArgs,
): void {}
