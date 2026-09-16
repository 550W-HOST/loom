import type { BrowserFixedPanelTab } from "@/lib/fixed-panel-tabs-state";
import type { BrowserAddressFocusRequest } from "./BrowserTabContent";
import type { UpdateBrowserTabArgs } from "./useThreadFileTabs";

interface BrowserTabDeckProps {
  browserTabs: readonly BrowserFixedPanelTab[];
  activeBrowserTabId: string | null;
  addressFocusRequest?: BrowserAddressFocusRequest | null;
  onAddressFocusRequestConsumed?: (request: BrowserAddressFocusRequest) => void;
  environmentId: string | null;
  canShowNativeBrowserView: boolean;
  canHandleBrowserCommands?: boolean;
  onNativeFocus?: () => void;
  threadId: string;
  onUpdate: (args: UpdateBrowserTabArgs) => void;
}

interface BrowserTabLifecycleObserverProps {
  browserTabs: readonly BrowserFixedPanelTab[];
  threadId: string;
}

export function buildBrowserTabIdSet({
  browserTabs,
}: {
  browserTabs: readonly BrowserFixedPanelTab[];
}): ReadonlySet<string> {
  return new Set(browserTabs.map((tab) => tab.id));
}

export function BrowserTabLifecycleObserver(
  _props: BrowserTabLifecycleObserverProps,
) {
  return null;
}

export function selectActiveBrowserTab(
  _browserTabs: readonly BrowserFixedPanelTab[],
  _activeBrowserTabId: string | null,
): BrowserFixedPanelTab | null {
  return null;
}

export function BrowserTabDeck(_props: BrowserTabDeckProps) {
  return null;
}
