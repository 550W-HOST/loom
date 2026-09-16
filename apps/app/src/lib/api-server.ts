export class LoomApiUnavailableError extends Error {
  readonly code = "loom_api_unavailable";

  constructor(readonly operation: string) {
    super(`Loom product API is not connected yet: ${operation}`);
    this.name = "LoomApiUnavailableError";
  }
}

function unavailableApiPath(path: readonly string[]): any {
  const operation = path.join(".");
  const callable = () => Promise.reject(new LoomApiUnavailableError(operation));
  return new Proxy(callable, {
    apply() {
      return Promise.reject(new LoomApiUnavailableError(operation));
    },
    get(_target, property) {
      if (property === "then") return undefined;
      if (property === "$url") {
        return () => {
          throw new LoomApiUnavailableError(`${operation}.$url`);
        };
      }
      return unavailableApiPath([...path, String(property)]);
    },
  });
}

export const apiClient: any = unavailableApiPath(["api", "v1"]);

export function toRelativeUrl(url: URL): string {
  return `${url.pathname}${url.search}${url.hash}`;
}
