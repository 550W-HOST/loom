export function getBrowserUrlHost(url: string): string {
  if (url.length === 0) return "";
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
