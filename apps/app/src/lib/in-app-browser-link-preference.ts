const HTTP_URL_SCHEME_PATTERN = /^https?:\/\//iu;

export function isHttpOrHttpsUrl(url: string): boolean {
  return HTTP_URL_SCHEME_PATTERN.test(url);
}

type UrlOpenTarget = "in-app-browser" | "external-browser" | "unhandled";

export function resolveUrlOpenTarget({
  url,
}: {
  desktopBrowserAvailable: boolean;
  openLinksInAppBrowser: boolean;
  url: string;
}): UrlOpenTarget {
  return isHttpOrHttpsUrl(url) ? "external-browser" : "unhandled";
}

export function openUrlByPreference({
  openExternalBrowser,
  url,
}: {
  desktopBrowserAvailable: boolean;
  openExternalBrowser: (url: string) => void;
  openInAppBrowser: (url: string) => void;
  openLinksInAppBrowser: boolean;
  url: string;
}): boolean {
  if (!isHttpOrHttpsUrl(url)) return false;
  openExternalBrowser(url);
  return true;
}

export function useOpenLinksInAppBrowserPreference() {
  return [false, (_value: boolean) => {}] as const;
}
