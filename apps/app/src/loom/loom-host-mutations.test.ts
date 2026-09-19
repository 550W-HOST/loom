import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import { loomDeleteHost } from "@/lib/loom-host-mutations";
import {
  LoomApiPathParamError,
  LoomHttpError,
  resolveLoomApiMethod,
} from "@/lib/loom-http";

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

describe("loom host deletion", () => {
  it("issues DELETE against the contract path with no body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomDeleteHost({ hostId: "host_1" })).resolves.toEqual({
      ok: true,
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("hosts.delete")).toBe("DELETE");
    expect(init.method).toBe("DELETE");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/hosts/host_1");
    expect(url.search).toBe("");
  });

  it("percent-encodes the host id exactly once", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await loomDeleteHost({ hostId: "host/one" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/hosts/host%2Fone");
  });

  it("refuses an empty host id before the request leaves", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomDeleteHost({ hostId: "" })).rejects.toBeInstanceOf(
      LoomApiPathParamError,
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("surfaces the server's typed refusal instead of a fake success", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          { code: "host_in_use", message: "host is still referenced by a project" },
          409,
        ),
      ),
    );

    await expect(loomDeleteHost({ hostId: "host_1" })).rejects.toMatchObject({
      status: 409,
      code: "host_in_use",
    });
    await expect(
      loomDeleteHost({ hostId: "host_1" }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the delete into the browser SDK surface", () => {
    expect(sdk.hosts.delete).toBe(loomDeleteHost);
  });
});
