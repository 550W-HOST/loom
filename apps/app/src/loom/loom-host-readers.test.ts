import { afterEach, describe, expect, it, vi } from "vitest";
import type { HostDirectoryListing } from "@bb/server-contract";
import { sdk } from "@/lib/sdk";
import { loomHostDirectory } from "@/lib/loom-host-readers";
import {
  LoomApiPathParamError,
  LoomHttpError,
  resolveLoomApiMethod,
} from "@/lib/loom-http";

const listing: HostDirectoryListing = {
  directory: "/home/me",
  parent: "/home",
  entries: [
    { kind: "directory", name: "repo", path: "/home/me/repo" },
    { kind: "file", name: "notes.txt", path: "/home/me/notes.txt" },
  ],
};

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

describe("loom host directory read", () => {
  it("issues GET against the contract path with the directory query", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomHostDirectory({ hostId: "host_1", path: "/home/me" }),
    ).resolves.toEqual(listing);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("hosts.directory")).toBe("GET");
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/hosts/host_1/directory");
    expect(url.searchParams.get("path")).toBe("/home/me");
  });

  it("omits an absent path so the server picks the host's default directory", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await loomHostDirectory({ hostId: "host_1" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/hosts/host_1/directory");
    expect(url.search).toBe("");
    expect(url.searchParams.has("path")).toBe(false);
  });

  it("percent-encodes the host id exactly once", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await loomHostDirectory({ hostId: "host/one", path: "/" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/hosts/host%2Fone/directory");
    expect(url.searchParams.get("path")).toBe("/");
  });

  it("refuses an empty host id before the request leaves", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomHostDirectory({ hostId: "" })).rejects.toBeInstanceOf(
      LoomApiPathParamError,
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("surfaces the server's typed refusal instead of a fake listing", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          { code: "not_configured", message: "host has not reported a default directory" },
          501,
        ),
      ),
    );

    await expect(loomHostDirectory({ hostId: "host_1" })).rejects.toMatchObject({
      status: 501,
      code: "not_configured",
    });
    await expect(loomHostDirectory({ hostId: "host_1" })).rejects.toBeInstanceOf(
      LoomHttpError,
    );
  });

  it("wires the read into the browser SDK surface", async () => {
    expect(sdk.hosts.directory).toBe(loomHostDirectory);

    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await expect(sdk.hosts.directory({ hostId: "host_1" })).resolves.toEqual(
      listing,
    );
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(url.pathname).toBe("/api/v1/hosts/host_1/directory");
    expect(init.method).toBe("GET");
  });
});
