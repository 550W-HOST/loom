import type { ReactNode } from "react";
import { EmptyStatePanel } from "@bb/shared-ui/empty-state";
import type { PluginPanelFixedPanelTab } from "@/lib/fixed-panel-tabs-state";

export interface OpenPluginPanelArgs {
  pluginId: string;
  actionId: string;
  title: string;
  paramsJson: string | null;
}

export interface PluginPanelActionEntry {
  id: string;
  pluginId: string;
  icon: string | null;
  title: string;
  onSelect: () => void;
}

export function usePluginPanelActions(_args: {
  openPluginPanel: (args: OpenPluginPanelArgs) => void;
  threadId: string | null | undefined;
}): readonly PluginPanelActionEntry[] {
  return [];
}

export function usePluginNewThreadPanelActions(_args: {
  openPluginPanel: (args: OpenPluginPanelArgs) => void;
  projectId: string | null;
}): readonly PluginPanelActionEntry[] {
  return [];
}

type PluginPanelSurfaceContext =
  | { kind: "thread"; threadId: string }
  | { kind: "new-thread"; projectId: string | null };

export function PluginPanelTabContent({
  fileOpenerOriginal,
}: {
  tab: PluginPanelFixedPanelTab;
  context: PluginPanelSurfaceContext;
  fileOpenerOriginal?: ReactNode;
}) {
  if (fileOpenerOriginal !== undefined) return fileOpenerOriginal;
  return (
    <div className="p-4">
      <EmptyStatePanel className="rounded-lg p-6 text-sm">
        This panel is unavailable.
      </EmptyStatePanel>
    </div>
  );
}
