import type { PluginComposerScope } from "./plugin-composer-host";

type ComposerScopeKind = PluginComposerScope["kind"] | null;

export function useResolvedComposerActions(_scopeKind: ComposerScopeKind) {
  return [] as const;
}

export function useResolvedComposerBanners(_scopeKind: ComposerScopeKind) {
  return [] as const;
}

export function useResolvedComposerPlusMenuItems(
  _scopeKind: ComposerScopeKind,
) {
  return [] as const;
}

export function useResolvedComposerEditor(_scopeKind: ComposerScopeKind) {
  return { effects: [] as const, observers: [] as const };
}
