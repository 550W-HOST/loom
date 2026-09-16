import type { PluginMessageDirectiveSlot } from "./plugin-slots";

export interface ResolvedComposerAction {
  key: string;
  pluginId: string;
  customizationId: string;
  generation: number;
  action: any;
}

export interface ResolvedComposerPlusMenuItem {
  key: string;
  pluginId: string;
  customizationId: string;
  generation: number;
  item: any;
}

export type ResolvedReplacement<T> =
  | { kind: "owner" }
  | { kind: "plugin"; registration: T };

export type FileOpenerOverride =
  | "builtin"
  | { pluginId: string; openerId: string };
export type FileOpenerPreferenceMap = Record<string, string>;
export const BUILT_IN_FILE_OPENER_PREFERENCE = "__builtin__";

export function buildFileOpenerRef(opener: {
  pluginId: string;
  id: string;
}): string {
  return `${opener.pluginId}/${opener.id}`;
}

export function resolveComposerActions(
  ..._args: readonly unknown[]
): readonly ResolvedComposerAction[] {
  return [];
}

export function resolveComposerBanners(
  ..._args: readonly unknown[]
): readonly any[] {
  return [];
}

export function resolveComposerPlusMenuItems(
  ..._args: readonly unknown[]
): readonly ResolvedComposerPlusMenuItem[] {
  return [];
}

export function resolveComposerEditorEffects(
  ..._args: readonly unknown[]
): readonly any[] {
  return [];
}

export function resolveComposerDraftObservers(
  ..._args: readonly unknown[]
): readonly any[] {
  return [];
}

export function resolvePendingInteraction(..._args: readonly unknown[]): null {
  return null;
}

export function resolveTimelineRenderer(..._args: readonly unknown[]): null {
  return null;
}

export function resolveReplacement<T>(
  ..._args: readonly unknown[]
): ResolvedReplacement<T> {
  return { kind: "owner" };
}

export function resolveFileOpenerReplacement<T>(_args: {
  registrations: readonly T[];
  preference?: FileOpenerPreferenceMap;
  path: string;
  override?: FileOpenerOverride;
}): ResolvedReplacement<T> {
  return { kind: "owner" };
}

export type ResolvedMessageDirective =
  | { status: "resolved"; slot: PluginMessageDirectiveSlot }
  | { status: "collision"; pluginIds: readonly string[] };

export function resolveMessageDirectiveRegistry(
  _slots: readonly PluginMessageDirectiveSlot[],
): ReadonlyMap<string, ResolvedMessageDirective> {
  return new Map();
}

export function getFileExtension(path: string): string | null {
  const name = path.split("/").at(-1) ?? "";
  const index = name.lastIndexOf(".");
  return index <= 0 || index === name.length - 1
    ? null
    : name.slice(index + 1).toLowerCase();
}
