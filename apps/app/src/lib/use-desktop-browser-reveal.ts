export function useDesktopBrowserReveal(_args: {
  threadId: string;
  isFocused: boolean;
  browserTabs: readonly { id: string }[];
  activateTab: (tabId: string) => void;
}): void {}
