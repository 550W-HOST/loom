import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomCreateProject,
  loomDeleteProject,
} from "@/lib/loom-project-mutations";
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

const NEW_PROJECT = {
  id: "proj_new",
  kind: "standard",
  name: "bb",
  gitRemoteUrl: null,
  createdAt: 1,
  updatedAt: 2,
  sources: [],
};

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom project creation", () => {
  it("issues POST against the contract path with the JSON body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(NEW_PROJECT, 201));
    vi.stubGlobal("fetch", fetchMock);

    const request = {
      name: "bb",
      source: { type: "local_path", hostId: "host_1", path: "/home/me/bb" },
    } as const;

    await expect(loomCreateProject(request)).resolves.toEqual(NEW_PROJECT);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("projects.create")).toBe("POST");
    expect(init.method).toBe("POST");
    expect(url.pathname).toBe("/api/v1/projects");
    expect(url.search).toBe("");
    expect(JSON.parse(String(init.body))).toEqual(request);
  });

  it("puts no signal into the request body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(NEW_PROJECT, 201));
    vi.stubGlobal("fetch", fetchMock);
    const controller = new AbortController();

    await loomCreateProject({
      name: "bb",
      source: { type: "local_path", hostId: "host_1", path: "/home/me/bb" },
      signal: controller.signal,
    });

    const [, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.signal).toBe(controller.signal);
    expect(Object.keys(JSON.parse(String(init.body))).sort()).toEqual([
      "name",
      "source",
    ]);
  });

  it("surfaces the server's refusal instead of a fake project", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          { code: "not_found", message: "host host_1 is not enrolled" },
          404,
        ),
      ),
    );

    await expect(
      loomCreateProject({
        name: "bb",
        source: { type: "local_path", hostId: "host_1", path: "/home/me/bb" },
      }),
    ).rejects.toMatchObject({ status: 404, code: "not_found" });
    await expect(
      loomCreateProject({
        name: "bb",
        source: { type: "local_path", hostId: "host_1", path: "/home/me/bb" },
      }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the create into the browser SDK surface", () => {
    expect(sdk.projects.create).toBe(loomCreateProject);
  });
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
