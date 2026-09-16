import type { ReactNode } from "react";
import type { ComposerView } from "./plugin-composer-host";

export interface PluginComposerPlusMenuContribution {
  key: string;
  pluginId: string;
  customizationId: string;
  generation: number;
  item: {
    id: string;
    label: string;
    description?: string;
    icon?: unknown;
  };
}

export interface PluginComposerPlusMenuSelection {
  restoreComposerFocus(): void;
  selectedElement: Element | null;
}

export function ComposerActionsSlot({
  children,
}: {
  view?: ComposerView;
  children?: ReactNode;
  includePluginContributions?: boolean;
}) {
  return children ?? null;
}

export function PluginComposerPlusMenuEntry(_props: {
  contribution: PluginComposerPlusMenuContribution;
  showPluginLabel: boolean;
  onSelected(selection: PluginComposerPlusMenuSelection): void;
}) {
  return null;
}
