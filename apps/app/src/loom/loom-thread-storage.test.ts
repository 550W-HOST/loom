import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomThreadStorageFiles,
  loomThreadStorageLocation,
  loomThreadStoragePaths,
} from "@/lib/loom-thread-storage";

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

describe("loom thread storage reads", () => {
  it("lists storage files with the contract query and path", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({
        files: [],
        truncated: false,
        storageRootPath: "/data/thread-storage/thr_1",
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomThreadStorageFiles({
        threadId: "thr_1",
        limit: "10",
        query: "notes",
      }),
    ).resolves.toEqual({
      files: [],
      truncated: false,
      storageRootPath: "/data/thread-storage/thr_1",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/threads/thr_1/thread-storage/files");
    expect(url.searchParams.get("limit")).toBe("10");
    expect(url.searchParams.get("query")).toBe("notes");
  });

  it("omits an absent file query instead of sending 'undefined'", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({ files: [], truncated: false, storageRootPath: "/r" }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await loomThreadStorageFiles({ threadId: "thr_1", limit: "5" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.searchParams.has("query")).toBe(false);
    expect(url.searchParams.get("limit")).toBe("5");
  });

  it("reads the storage location from the layout route", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({
        hostId: "host_1",
        storageRootPath: "/data/thread-storage/thr_1",
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomThreadStorageLocation({ threadId: "thr_1" }),
    ).resolves.toEqual({
      hostId: "host_1",
      storageRootPath: "/data/thread-storage/thr_1",
    });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/threads/thr_1/thread-storage/location");
    expect(url.search).toBe("");
  });

  it("lists storage paths with both path-kind flags", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({
        paths: [],
        truncated: true,
        storageRootPath: "/data/thread-storage/thr_1",
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomThreadStoragePaths({
        threadId: "thr_1",
        limit: "20",
        includeFiles: "true",
        includeDirectories: "false",
      }),
    ).resolves.toEqual({
      paths: [],
      truncated: true,
      storageRootPath: "/data/thread-storage/thr_1",
    });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(url.pathname).toBe("/api/v1/threads/thr_1/thread-storage/paths");
    expect(url.searchParams.get("limit")).toBe("20");
    expect(url.searchParams.get("includeFiles")).toBe("true");
    expect(url.searchParams.get("includeDirectories")).toBe("false");
  });

  it("surfaces a route failure as a loom HTTP error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          { code: "thread_environment_unavailable", message: "no environment" },
          409,
        ),
      ),
    );

    await expect(
      loomThreadStorageFiles({ threadId: "thr_1", limit: "5" }),
    ).rejects.toMatchObject({
      status: 409,
      code: "thread_environment_unavailable",
    });
  });

  it("wires the storage reads into the browser SDK surface", () => {
    expect(sdk.threads.storageFiles).toBe(loomThreadStorageFiles);
    expect(sdk.threads.storageLocation).toBe(loomThreadStorageLocation);
    expect(sdk.threads.storagePaths).toBe(loomThreadStoragePaths);
  });
});
