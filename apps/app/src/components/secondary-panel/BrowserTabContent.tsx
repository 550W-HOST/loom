export interface BrowserAddressFocusRequest {
  requestId: number;
  tabId: string;
}

interface BrowserTabContentProps {
  tabId: string;
  existingOnly?: true;
  initialUrl: string;
  addressFocusRequest: BrowserAddressFocusRequest | null;
  onAddressFocusRequestConsumed?: (request: BrowserAddressFocusRequest) => void;
  canShowNativeBrowserView: boolean;
  canHandleBrowserCommands?: boolean;
  onNativeFocus?: () => void;
  visibilityCoordinator: unknown;
  environmentId: string | null;
  threadId: string;
  onUpdate: (args: unknown) => void;
}

export function BrowserTabContent(_props: BrowserTabContentProps) {
  return null;
}
