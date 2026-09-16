import type { ComponentType, ReactNode } from "react";
import type { ResolvedReplacement } from "@/lib/plugin-slot-resolvers";

interface PluginReplacementRegistration {
  id: string;
  pluginId: string;
  generation: number;
}

export function PluginReplacementSlot<
  Registration extends PluginReplacementRegistration,
>({
  original,
}: {
  children: (registration: Registration, Original: ComponentType) => ReactNode;
  onCrash?: (pluginId: string) => void;
  original: ReactNode;
  replacement: ResolvedReplacement<Registration>;
  slotKind: string;
}) {
  return original;
}
