import {
  useCallback,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";

interface UrlOpenRoutingProviderProps {
  children: ReactNode;
  openInAppBrowser: ((url: string) => void) | null;
}

type UrlAnchorClickHandler = (
  event: ReactMouseEvent<HTMLAnchorElement>,
) => void;

export function openUrlInExternalBrowser(url: string): void {
  if (typeof window !== "undefined") {
    window.open(url, "_blank", "noopener,noreferrer");
  }
}

export function UrlOpenRoutingProvider({
  children,
}: UrlOpenRoutingProviderProps) {
  return children;
}

export function AppNavigationUrlHost({ children }: { children: ReactNode }) {
  return children;
}

export function useUrlAnchorClickHandler(
  url: string | undefined,
): UrlAnchorClickHandler {
  return useCallback(
    (event) => {
      if (event.defaultPrevented || event.button !== 0 || url === undefined) {
        return;
      }
      if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) {
        return;
      }
      event.preventDefault();
      openUrlInExternalBrowser(url);
    },
    [url],
  );
}
