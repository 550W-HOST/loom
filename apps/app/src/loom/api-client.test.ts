import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  LOOM_API_MOUNT_PATH,
  LOOM_API_ROUTES,
  findLoomApiRoute,
  isPathParameter,
  pathParameterName,
  routePathSegments,
} from "@/lib/loom-api-routes";
import {
  buildLoomApiUrl,
  LoomApiRouteError,
  loomApiJson,
  throwLoomHttpError,
} from "@/lib/loom-http";
import {
  LoomApiUnknownRouteError,
  apiClient,
  toRelativeUrl,
} from "@/lib/api-server";

interface ContractRoute {
  readonly id: string;
  readonly method: string;
  readonly fullPath: string;
  readonly mountPath: string;
}

function readContractRoutes(): ContractRoute[] {
  const contractPath = path.resolve(
    import.meta.dirname,
    "../../../../contracts/bb/server-api.json",
  );
  const contract = JSON.parse(fs.readFileSync(contractPath, "utf8")) as {
    mountPath: string;
    routes: readonly {
      id: string;
      method: string;
      fullPath: string;
    }[];
  };
  return contract.routes.map((route) => ({
    ...route,
    mountPath: contract.mountPath,
  }));
}

describe("loom same-origin API route table", () => {
  const contractRoutes = readContractRoutes();

  it("declares only routes the exported contract actually contains", () => {
    for (const route of LOOM_API_ROUTES) {
      const contractRoute = contractRoutes.find(
        (candidate) => candidate.id === route.id,
      );
      expect(
        contractRoute,
        `${route.id} is not in contracts/bb/server-api.json`,
      ).toBeDefined();
      expect(route.method).toBe(contractRoute!.method);
      expect(
        `${LOOM_API_MOUNT_PATH}${route.path}`,
        `${route.id} path drifted from the contract`,
      ).toBe(contractRoute!.fullPath);
    }
  });

  it("advertises the contract's mount path", () => {
    expect(LOOM_API_MOUNT_PATH).toBe(contractRoutes[0]?.mountPath);
  });

  it("derives every URL from the same origin with no hardcoded host", () => {
    for (const route of LOOM_API_ROUTES) {
      const params: Record<string, string> = {};
      for (const segment of routePathSegments(route)) {
        if (isPathParameter(segment)) {
          params[pathParameterName(segment)] = "sample-value";
        }
      }
      const url = buildLoomApiUrl(route.id, { param: params });
      expect(url.origin).toBe(window.location.origin);
      expect(url.pathname.startsWith(`${LOOM_API_MOUNT_PATH}/`)).toBe(true);
    }
  });

  it("keeps the catch-all file path readable instead of one encoded segment", () => {
    const url = buildLoomApiUrl("threads.worktreeFile", {
      param: { id: "thread-1", filePath: "src/components/App.tsx" },
    });
    expect(url.pathname).toBe(
      "/api/v1/threads/thread-1/worktree/files/src/components/App.tsx",
    );
  });

  it("percent-encodes a plain id", () => {
    const url = buildLoomApiUrl("threads.storageContent", {
      param: { id: "a/b" },
      query: { path: "src/main.ts" },
    });
    expect(url.pathname).toBe("/api/v1/threads/a%2Fb/thread-storage/content");
    expect(url.searchParams.get("path")).toBe("src/main.ts");
  });

  it("refuses an unknown route id and a missing path parameter", () => {
    expect(() =>
      buildLoomApiUrl("threads.notARealRoute" as never),
    ).toThrow(LoomApiRouteError);
    expect(() => buildLoomApiUrl("threads.worktreeFile")).toThrow(
      LoomApiRouteError,
    );
  });

  it("omits undefined query values rather than sending 'undefined'", () => {
    const url = buildLoomApiUrl("projects.fileContent", {
      param: { id: "p1" },
      query: { path: "README.md", environmentId: undefined },
    });
    expect(url.searchParams.has("environmentId")).toBe(false);
    expect(url.searchParams.get("path")).toBe("README.md");
  });

  it("resolves the call-chain seam to the same routes", () => {
    expect(
      toRelativeUrl(
        apiClient.threads[":id"]["thread-storage"].content.$url({
          param: { id: "t1" },
          query: { path: "a/b.txt" },
        }),
      ),
    ).toBe("/api/v1/threads/t1/thread-storage/content?path=a%2Fb.txt");
    expect(
      toRelativeUrl(
        apiClient.environments[":id"].diff.file.$url({
          param: { id: "e1" },
          query: { path: "x" },
        }),
      ),
    ).toBe("/api/v1/environments/e1/diff/file?path=x");
    expect(
      toRelativeUrl(
        apiClient.hosts[":id"]["permission-ceiling"].$url({
          param: { id: "h1" },
        }),
      ),
    ).toBe("/api/v1/hosts/h1/permission-ceiling");
  });

  it("fails loudly on a call chain that is not a contract route", () => {
    expect(() => apiClient.hosts[":id"].delete.$url({ param: { id: "h1" } })).toThrow(
      LoomApiUnknownRouteError,
    );
  });

  it("keeps the route table free of entries the app never calls", () => {
    for (const route of LOOM_API_ROUTES) {
      expect(findLoomApiRoute(route.id)).toBe(route);
    }
    expect(new Set(LOOM_API_ROUTES.map((route) => route.id)).size).toBe(
      LOOM_API_ROUTES.length,
    );
  });
});

describe("loom HTTP error fidelity", () => {
  function responseWith(status: number, body: string, contentType: string) {
    return new Response(body, {
      status,
      headers: { "content-type": contentType },
    });
  }

  it("keeps 404 and 422 distinguishable with their codes", async () => {
    await expect(
      throwLoomHttpError(
        responseWith(404, JSON.stringify({ code: "not_found", message: "gone" }), "application/json"),
      ),
    ).rejects.toMatchObject({
      status: 404,
      code: "not_found",
      message: "HTTP 404: gone",
      body: { code: "not_found", message: "gone" },
    });

    await expect(
      throwLoomHttpError(
        responseWith(
          422,
          JSON.stringify({ code: "validation_failed", message: "bad input" }),
          "application/json",
        ),
      ),
    ).rejects.toMatchObject({ status: 422, code: "validation_failed" });
  });

  it("does not turn an HTML error page into a fabricated JSON body", async () => {
    await expect(
      throwLoomHttpError(
        responseWith(500, "<!doctype html><html>oops</html>", "text/html"),
      ),
    ).rejects.toMatchObject({ status: 500, body: undefined });
  });

  it("preserves an abort instead of rewriting it as an HTTP failure", async () => {
    const controller = new AbortController();
    const aborting = new Promise<never>((_resolve, reject) => {
      controller.signal.addEventListener("abort", () =>
        reject(controller.signal.reason),
      );
    });
    controller.abort(new DOMException("aborted", "AbortError"));
    await expect(aborting).rejects.toMatchObject({ name: "AbortError" });
  });

  it("returns undefined for an empty JSON body", async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async () => new Response("", { status: 200 })) as never;
    try {
      await expect(loomApiJson("system.config")).resolves.toBeUndefined();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
