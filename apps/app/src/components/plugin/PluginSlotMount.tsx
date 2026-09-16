import type { ReactNode } from "react";

export function resetCrashedPluginSlots(_pluginId: string): void {}

export function resetAllCrashedPluginSlotsForTest(): void {}

interface PluginSlotMountProps {
  pluginId: string;
  slotKind: string;
  slotId: string;
  children: ReactNode;
  crashFallback?: ReactNode;
  instanceId?: string;
  onCrash?: (pluginId: string) => void;
}

export function PluginSlotMount({ children }: PluginSlotMountProps) {
  return children;
}
