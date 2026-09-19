import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import { loomDeleteProject } from "@/lib/loom-project-mutations";
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

describe("loom project deletion", () => {
  it("issues DELETE against the contract path with no body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomDeleteProject({ projectId: "proj_1" })).resolves.toEqual({
      ok: true,
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("projects.delete")).toBe("DELETE");
    expect(init.method).toBe("DELETE");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/projects/proj_1");
    expect(url.search).toBe("");
  });

  it("percent-encodes the project id exactly once", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await loomDeleteProject({ projectId: "proj/one" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/projects/proj%2Fone");
  });

  it("forwards the caller's abort signal", async () => {
    const controller = new AbortController();
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await loomDeleteProject({ projectId: "proj_1", signal: controller.signal });

    const [, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.signal).toBe(controller.signal);
  });

  it("refuses an empty project id before the request leaves", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomDeleteProject({ projectId: "" })).rejects.toBeInstanceOf(
      LoomApiPathParamError,
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("surfaces the server's refusal instead of a fake success", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          {
            code: "conflict",
            message: "project proj_1 still has 1 live thread(s)",
          },
          409,
        ),
      ),
    );

    await expect(loomDeleteProject({ projectId: "proj_1" })).rejects.toMatchObject(
      { status: 409, code: "conflict" },
    );
    await expect(
      loomDeleteProject({ projectId: "proj_1" }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the delete into the browser SDK surface", () => {
    expect(sdk.projects.delete).toBe(loomDeleteProject);
  });
});
