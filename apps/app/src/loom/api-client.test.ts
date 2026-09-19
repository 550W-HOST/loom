import fs from "node:fs";
import path from "node:path";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import {
  LOOM_API_MOUNT_PATH,
  LOOM_API_ROUTES,
  findLoomApiRoute,
  isCatchAllParameter,
  isPathParameter,
  pathParameterName,
  routePathSegments,
} from "@/lib/loom-api-routes";
import {
  buildLoomApiUrl,
  LoomApiBodyNotAllowedError,
  LoomApiBodyRequiredError,
  LoomApiMethodError,
  LoomApiPathParamError,
  LoomApiRouteError,
  LoomHttpError,
  loomApiFetch,
  loomApiJson,
  loomNativeJson,
  resolveLoomApiMethod,
  throwLoomHttpError,
} from "@/lib/loom-http";
import { apiClient, toRelativeUrl } from "@/lib/api-server";

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
    routes: readonly { id: string; method: string; fullPath: string }[];
  };
  return contract.routes.map((route) => ({
    ...route,
    mountPath: contract.mountPath,
  }));
}

function paramsFor(routeId: string): Record<string, string> {
  const route = findLoomApiRoute(routeId);
  if (!route) throw new Error(`no route ${routeId}`);
  const params: Record<string, string> = {};
  for (const segment of routePathSegments(route)) {
    if (isPathParameter(segment)) {
      params[pathParameterName(segment)] = `sample-${pathParameterName(segment)}`;
    }
  }
  return params;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

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
      // The method must match the contract exactly: item 1 of the review was a
      // POST route being issued as GET.
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

  it("has no duplicate route ids", () => {
    expect(new Set(LOOM_API_ROUTES.map((route) => route.id)).size).toBe(
      LOOM_API_ROUTES.length,
    );
  });

  it("derives every URL from the same origin with no hardcoded host", () => {
    for (const route of LOOM_API_ROUTES) {
      const url = buildLoomApiUrl(route.id, { param: paramsFor(route.id) });
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

  it("omits undefined query values rather than sending 'undefined'", () => {
    const url = buildLoomApiUrl("projects.fileContent", {
      param: { id: "p1" },
      query: { path: "README.md", environmentId: undefined },
    });
    expect(url.searchParams.has("environmentId")).toBe(false);
    expect(url.searchParams.get("path")).toBe("README.md");
  });

  it("refuses an unknown route id", () => {
    expect(() => buildLoomApiUrl("threads.notARealRoute" as never)).toThrow(
      LoomApiRouteError,
    );
  });

  it("refuses a missing path parameter", () => {
    expect(() => buildLoomApiUrl("threads.worktreeFile")).toThrow(
      LoomApiPathParamError,
    );
  });
});

describe("loom transport encodes a path parameter exactly once", () => {
  it("encodes a space, Unicode, and a literal percent once", () => {
    const cases: Array<[string, string]> = [
      // raw input → what must appear in the URL path
      ["my file.ts", "my%20file.ts"],
      ["проект.ts", "%D0%BF%D1%80%D0%BE%D0%B5%D0%BA%D1%82.ts"],
      ["100%.ts", "100%25.ts"],
      ["a+b & c.ts", "a%2Bb%20%26%20c.ts"],
      ["file(1).ts", "file(1).ts"],
    ];
    for (const [raw, expected] of cases) {
      const url = buildLoomApiUrl("threads.worktreeFile", {
        param: { id: "t1", filePath: raw },
      });
      expect(url.pathname).toBe(
        `/api/v1/threads/t1/worktree/files/${expected}`,
      );
    }
  });

  it("does not double-encode a pre-encoded-looking value", () => {
    // A file literally named "a%20b.ts" must become a%2520b.ts, and a file
    // named "a b.ts" must become a%20b.ts. The two must not collide.
    const literalPercent = buildLoomApiUrl("threads.worktreeFile", {
      param: { id: "t1", filePath: "a%20b.ts" },
    });
    const realSpace = buildLoomApiUrl("threads.worktreeFile", {
      param: { id: "t1", filePath: "a b.ts" },
    });
    expect(literalPercent.pathname).toBe(
      "/api/v1/threads/t1/worktree/files/a%2520b.ts",
    );
    expect(realSpace.pathname).toBe(
      "/api/v1/threads/t1/worktree/files/a%20b.ts",
    );
    expect(literalPercent.pathname).not.toBe(realSpace.pathname);
  });

  it("keeps a nested catch-all path inside the declared route", () => {
    const url = buildLoomApiUrl("threads.worktreeFile", {
      param: { id: "t1", filePath: "src/deep/nested/文件.ts" },
    });
    expect(url.pathname).toBe(
      "/api/v1/threads/t1/worktree/files/src/deep/nested/%E6%96%87%E4%BB%B6.ts",
    );
  });

  it("refuses dot-segment traversal instead of letting URL normalise it away", () => {
    const traversals = [
      "..",
      ".",
      "../../../../etc/passwd",
      "a/../../b",
      "a/./b",
    ];
    for (const filePath of traversals) {
      expect(
        () =>
          buildLoomApiUrl("threads.worktreeFile", {
            param: { id: "t1", filePath },
          }),
        `expected ${filePath} to be refused`,
      ).toThrow(LoomApiPathParamError);
    }
  });

  it("treats an encoded-slash spelling as one literal segment, not a traversal", () => {
    // `..%2F..` is a single segment: `%2F` is not a separator, so this is a
    // (weird) file name rather than a path escape. It must be encoded once and
    // stay inside the declared route.
    const url = buildLoomApiUrl("threads.worktreeFile", {
      param: { id: "t1", filePath: "..%2F..%2Fetc" },
    });
    expect(url.pathname).toBe(
      "/api/v1/threads/t1/worktree/files/..%252F..%252Fetc",
    );
  });

  it("refuses an empty segment and control characters", () => {
    for (const filePath of ["", "a//b", "a\u0000b", "a\\b", "a\nb"]) {
      expect(() =>
        buildLoomApiUrl("threads.worktreeFile", {
          param: { id: "t1", filePath },
        }),
      ).toThrow(LoomApiPathParamError);
    }
    expect(() =>
      buildLoomApiUrl("threads.worktreeFile", { param: { id: "", filePath: "a" } }),
    ).toThrow(LoomApiPathParamError);
  });

  it("never resolves a traversal to a different route", () => {
    // The failure this guards against: `url.pathname = "/a/../b"` silently
    // becomes `/b`, so the request would leave the declared route.
    expect(() =>
      buildLoomApiUrl("threads.worktreeFile", {
        param: { id: "t1", filePath: "../storageContent" },
      }),
    ).toThrow(LoomApiPathParamError);
  });
});

describe("loom transport executes the contract method", () => {
  it("issues POST for hosts.createJoinCode even though a JSON body is sent", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ joinCode: "c", hostId: "h", expiresAt: 1 }, 201));
    vi.stubGlobal("fetch", fetchMock);

    await loomApiJson("hosts.createJoinCode", { json: {} });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [
      URL,
      RequestInit,
    ];
    expect(String(url)).toBe(`${window.location.origin}/api/v1/hosts/join-codes`);
    expect(init.method).toBe("POST");
    expect(init.body).toBe("{}");
  });

  it("issues GET and never attaches a body for a read route", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ status: "ok" }));
    vi.stubGlobal("fetch", fetchMock);

    await loomApiJson("system.config");

    const [url, init] = fetchMock.mock.calls[0] as unknown as [
      URL,
      RequestInit,
    ];
    expect(String(url)).toBe(`${window.location.origin}/api/v1/system/config`);
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
  });

  it("refuses a requested method that disagrees with the contract", () => {
    expect(() =>
      // `loomApiFetch` resolves the method from the table; a mismatch must be
      // refused rather than silently overriding the contract.
      resolveLoomApiMethod("hosts.createJoinCode", "GET"),
    ).toThrow(LoomApiMethodError);
  });

  it("resolves the method from the route table", () => {
    expect(resolveLoomApiMethod("hosts.createJoinCode")).toBe("POST");
    expect(resolveLoomApiMethod("system.config")).toBe("GET");
    expect(resolveLoomApiMethod("hosts.updatePermissionCeiling")).toBe("PATCH");
    expect(resolveLoomApiMethod("system.voiceTranscription")).toBe("POST");
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
        responseWith(
          404,
          JSON.stringify({ code: "not_found", message: "gone" }),
          "application/json",
        ),
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

  it("re-throws an abort raised while reading the error body", async () => {
    // `response.text()` is itself abortable. Swallowing that abort turned a
    // cancelled request into a bogus HTTP failure.
    const abort = new DOMException("aborted", "AbortError");
    const response = {
      status: 500,
      statusText: "Server Error",
      headers: new Headers(),
      text: () => Promise.reject(abort),
    } as unknown as Response;

    await expect(throwLoomHttpError(response)).rejects.toBe(abort);
  });

  it("returns undefined for an empty JSON body", async () => {
    vi.stubGlobal("fetch", (async () => new Response("", { status: 200 })) as never);
    await expect(loomApiJson("system.config")).resolves.toBeUndefined();
  });

  it("raises LoomHttpError with status for a failed request", async () => {
    vi.stubGlobal(
      "fetch",
      (async () =>
        jsonResponse({ code: "boom", message: "nope" }, 503)) as never,
    );
    await expect(loomApiJson("system.config")).rejects.toBeInstanceOf(
      LoomHttpError,
    );
  });

  it("reads a loom-native path through the same error mapping", async () => {
    vi.stubGlobal(
      "fetch",
      (async () => new Response("nope", { status: 503 })) as never,
    );
    await expect(loomNativeJson("/health")).rejects.toMatchObject({
      status: 503,
    });
  });
});

describe("loom typed apiClient seam", () => {
  it("resolves the call chains the app actually uses", () => {
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
    expect(
      toRelativeUrl(apiClient.hosts[":id"].delete.$url({ param: { id: "h1" } })),
    ).toBe("/api/v1/hosts/h1");
    expect(toRelativeUrl(apiClient.hosts["join-codes"].$url({}))).toBe(
      "/api/v1/hosts/join-codes",
    );
  });

  it("hides the wrong verb from the type surface", () => {
    // At runtime every verb exists so a conflict can be *refused* with a clear
    // error; the type surface is what prevents the call from being written. The
    // runtime refusal is covered in the transport describe block.
    const joinCodeRoute = apiClient.hosts["join-codes"] as Record<
      string,
      unknown
    >;
    expect(typeof joinCodeRoute.$post).toBe("function");
  });

  it("refuses the wrong verb at runtime instead of issuing it", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    const joinCodeRoute = apiClient.hosts["join-codes"] as unknown as {
      $get(args: unknown): Promise<unknown>;
    };

    await expect(joinCodeRoute.$get({ json: {} })).rejects.toBeInstanceOf(
      LoomApiMethodError,
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("issues the POST with its body through the seam", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ joinCode: "c", hostId: "h", expiresAt: 1 }, 201));
    vi.stubGlobal("fetch", fetchMock);

    await apiClient.hosts["join-codes"].$post({ json: {} });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(String(url)).toBe(`${window.location.origin}/api/v1/hosts/join-codes`);
    expect(init.method).toBe("POST");
  });
});

describe("loom route table catch-all detection", () => {
  it("flags only the {.+} segments as catch-all", () => {
    for (const route of LOOM_API_ROUTES) {
      for (const segment of routePathSegments(route)) {
        if (!isPathParameter(segment)) continue;
        const isCatchAll = isCatchAllParameter(segment);
        expect(isCatchAll).toBe(segment.includes("{.+}"));
      }
    }
    const filePreviews = findLoomApiRoute("filePreviews.content");
    expect(
      routePathSegments(filePreviews!).some((segment) =>
        isCatchAllParameter(segment),
      ),
    ).toBe(true);
  });
});

describe("the lowest transport enforces the contract request before fetch", () => {
  // The higher-level seam had a guard, but `loomApiFetch`/`loomApiJson` are
  // exported and took a generic bag, so an untyped caller could still put a
  // body on a route-table GET and let the browser throw. These call the
  // transport directly and assert fetch is never reached.

  function stubFetch() {
    const fetchMock = vi.fn(async () => jsonResponse({}));
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("refuses a JSON body on a GET route without calling fetch", async () => {
    const fetchMock = stubFetch();

    await expect(
      loomApiFetch("threads.worktreeFile", {
        param: { id: "t1", filePath: "a.ts" },
        // Cast past the types, as a JS caller or `as any` would.
        json: { x: 1 },
      } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses a multipart body on a GET route without calling fetch", async () => {
    const fetchMock = stubFetch();

    await expect(
      loomApiFetch("threads.worktreeFile", {
        param: { id: "t1", filePath: "a.ts" },
        formData: new FormData(),
      } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses a body on loomApiJson for a bodyless route", async () => {
    const fetchMock = stubFetch();

    await expect(
      loomApiJson("system.config", { json: {} } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses json and formData together, whichever the route", async () => {
    const fetchMock = stubFetch();

    await expect(
      loomApiFetch("hosts.createJoinCode", {
        json: {},
        formData: new FormData(),
      } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);
    expect(fetchMock).not.toHaveBeenCalled();

    await expect(
      loomApiFetch("threads.worktreeFile", {
        param: { id: "t1", filePath: "a.ts" },
        json: {},
        formData: new FormData(),
      } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses a missing JSON body before fetch", async () => {
    const fetchMock = stubFetch();
    const untypedFetch = loomApiFetch as unknown as (
      routeId: "hosts.createJoinCode",
    ) => Promise<Response>;
    const untypedJson = loomApiJson as unknown as (
      routeId: "hosts.createJoinCode",
    ) => Promise<unknown>;

    await expect(untypedFetch("hosts.createJoinCode")).rejects.toBeInstanceOf(
      LoomApiBodyRequiredError,
    );
    await expect(untypedJson("hosts.createJoinCode")).rejects.toMatchObject({
      code: "loom_api_body_required",
      bodyKind: "JSON",
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses a missing multipart body before fetch", async () => {
    const fetchMock = stubFetch();
    const untypedFetch = loomApiFetch as unknown as (
      routeId: "system.voiceTranscription",
    ) => Promise<Response>;

    await expect(
      untypedFetch("system.voiceTranscription"),
    ).rejects.toMatchObject({
      code: "loom_api_body_required",
      bodyKind: "multipart",
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("refuses a query on a route whose contract declares none", async () => {
    const fetchMock = stubFetch();

    await expect(
      loomApiJson("system.config", { query: { nope: "x" } } as never),
    ).rejects.toBeInstanceOf(LoomApiBodyNotAllowedError);

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("still sends the body a json route declares", async () => {
    const fetchMock = stubFetch();

    await loomApiJson("hosts.createJoinCode", { json: {} });

    const [, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("POST");
    expect(init.body).toBe("{}");
  });

  it("still sends the multipart body a form route declares", async () => {
    const fetchMock = stubFetch();
    const formData = new FormData();
    formData.set("file", new Blob(["x"]), "x.txt");

    await loomApiFetch("system.voiceTranscription", { formData });

    const [, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("POST");
    expect(init.body).toBe(formData);
  });

  it("carries the typed error code so a caller can branch on it", async () => {
    stubFetch();
    await expect(
      loomApiFetch("system.config", { json: {} } as never),
    ).rejects.toMatchObject({ code: "loom_api_body_not_allowed" });
  });
});

describe("the seam cannot send a body through the wrong verb", () => {
  it("refuses $get with a body on a POST route", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({}));
    vi.stubGlobal("fetch", fetchMock);
    const joinCodeRoute = apiClient.hosts["join-codes"] as unknown as {
      $get(args: unknown): Promise<unknown>;
    };

    await expect(joinCodeRoute.$get({ json: {} })).rejects.toBeInstanceOf(
      LoomApiMethodError,
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
