import type { BbDesktopBrowserApi } from "@bb/desktop-contract";

export interface BrowserViewVisibilityCoordinator {
  show(
    tabId: string,
    syncBounds: () => void,
    options?: { focus?: boolean },
  ): void;
  hide(tabId: string): void;
  release(tabId: string): void;
}

export function createBrowserViewVisibilityCoordinator(
  _desktopBrowser: BbDesktopBrowserApi,
): BrowserViewVisibilityCoordinator {
  return {
    show(_tabId, _syncBounds, _options) {},
    hide(_tabId) {},
    release(_tabId) {},
  };
}

export function registerBrowserView(_args: {
  environmentId: string | null;
  tabId: string;
  threadId: string;
}): void {}

export function destroyPersistedBrowserView(_args: {
  desktopBrowser: BbDesktopBrowserApi;
  tabId: string;
}): void {}

export function destroyPersistedBrowserViewsForThread(_args: {
  desktopBrowser: BbDesktopBrowserApi | null;
  threadId: string;
}): void {}

export function destroyPersistedBrowserViewsForEnvironment(_args: {
  desktopBrowser: BbDesktopBrowserApi | null;
  environmentId: string;
}): void {}

export function resetBrowserViewPersistence(): void {}
