import {
  useSyncExternalStore,
  type ComponentType,
  type ReactNode,
} from "react";

interface RemovedPluginSlot {
  pluginId: string;
  id: string;
  generation: number;
  title: string;
  description?: string;
  path: string;
  icon?: string;
  kind: string;
  component: ComponentType<any>;
  extensions: readonly string[];
  run(...args: any[]): unknown;
  isAvailable?: (...args: any[]) => boolean;
  [key: string]: any;
}

export type PluginHomepageSectionSlot = RemovedPluginSlot;
export type PluginSettingsSectionSlot = RemovedPluginSlot;
export type ExperimentalAppOverlaySlot = RemovedPluginSlot;
export type PluginNavPanelSlot = RemovedPluginSlot;
export type PluginThreadPanelActionSlot = RemovedPluginSlot;
export type PluginNewThreadPanelActionSlot = RemovedPluginSlot;
export type PluginComposerCustomizationSlot = RemovedPluginSlot;
export type PluginPendingInteractionSlot = RemovedPluginSlot;
export type PluginSidebarFooterItemSlot = RemovedPluginSlot;
export type ExperimentalSidebarNavigationSlot = RemovedPluginSlot;
export type PluginThreadListSlot = RemovedPluginSlot;
export type PluginThreadHeaderActionSlot = RemovedPluginSlot;
export type PluginFileOpenerSlot = RemovedPluginSlot;
export type PluginSourceCodeRendererSlot = RemovedPluginSlot;
export type PluginDiffRendererSlot = RemovedPluginSlot;
export type PluginMessageDirectiveSlot = RemovedPluginSlot;
export type PluginMessageActionSlot = RemovedPluginSlot;
export type PluginCommandPaletteActionSlot = RemovedPluginSlot;
export type PluginProviderIconSlot = RemovedPluginSlot;
export type PluginTimelineRendererSlot = RemovedPluginSlot;
export type PluginEnvironmentProviderInputsSlot = RemovedPluginSlot;

export interface PluginRegistrationSet {
  [key: string]: readonly unknown[] | undefined;
}

export interface PluginSlotSnapshot {
  homepageSections: readonly PluginHomepageSectionSlot[];
  settingsSections: readonly PluginSettingsSectionSlot[];
  appOverlays: readonly ExperimentalAppOverlaySlot[];
  navPanels: readonly PluginNavPanelSlot[];
  threadPanelActions: readonly PluginThreadPanelActionSlot[];
  newThreadPanelActions: readonly PluginNewThreadPanelActionSlot[];
  composerCustomizations: readonly PluginComposerCustomizationSlot[];
  pendingInteractions: readonly PluginPendingInteractionSlot[];
  sidebarFooterItems: readonly PluginSidebarFooterItemSlot[];
  experimentalSidebarNavigations: readonly ExperimentalSidebarNavigationSlot[];
  threadLists: readonly PluginThreadListSlot[];
  threadHeaderActions: readonly PluginThreadHeaderActionSlot[];
  fileOpeners: readonly PluginFileOpenerSlot[];
  sourceCodeRenderers: readonly PluginSourceCodeRendererSlot[];
  diffRenderers: readonly PluginDiffRendererSlot[];
  messageDirectives: readonly PluginMessageDirectiveSlot[];
  messageActions: readonly PluginMessageActionSlot[];
  commandPaletteActions: readonly PluginCommandPaletteActionSlot[];
  providerIcons: readonly PluginProviderIconSlot[];
  timelineRenderers: readonly PluginTimelineRendererSlot[];
  environmentProviderInputs: readonly PluginEnvironmentProviderInputsSlot[];
}

export const EMPTY_PLUGIN_SLOT_SNAPSHOT: PluginSlotSnapshot = {
  homepageSections: [],
  settingsSections: [],
  appOverlays: [],
  navPanels: [],
  threadPanelActions: [],
  newThreadPanelActions: [],
  composerCustomizations: [],
  pendingInteractions: [],
  sidebarFooterItems: [],
  experimentalSidebarNavigations: [],
  threadLists: [],
  threadHeaderActions: [],
  fileOpeners: [],
  sourceCodeRenderers: [],
  diffRenderers: [],
  messageDirectives: [],
  messageActions: [],
  commandPaletteActions: [],
  providerIcons: [],
  timelineRenderers: [],
  environmentProviderInputs: [],
};

export function subscribePluginSlots(): () => void {
  return () => {};
}

export function getPluginSlotSnapshot(): PluginSlotSnapshot {
  return EMPTY_PLUGIN_SLOT_SNAPSHOT;
}

export function usePluginSlots(): PluginSlotSnapshot {
  return useSyncExternalStore(
    subscribePluginSlots,
    getPluginSlotSnapshot,
    getPluginSlotSnapshot,
  );
}

export function beginPluginSlotBatch(_options: {
  maxHoldMs: number;
}): () => void {
  return () => {};
}

export function setPluginSlotRegistrations(
  _pluginId: string,
  _registrations: PluginRegistrationSet,
): void {}

export function removePluginSlotRegistrations(_pluginId: string): void {}

export function resetPluginSlotStoreForTest(): void {}

export function RemovedPluginContent({ children }: { children?: ReactNode }) {
  return children ?? null;
}
