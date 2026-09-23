import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import { loomEnvironmentPaths } from "@/lib/loom-environment-readers";
import {
  loomProjectCommands,
  loomProjectPaths,
} from "@/lib/loom-project-readers";
import { LoomApiPathParamError, LoomHttpError } from "@/lib/loom-http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const listing = {
  paths: [
    { kind: "file" as const, path: "Cargo.toml", name: "Cargo.toml", score: 1, positions: [0] },
  ],
  truncated: false,
};

const commandListing = {
  commands: [
    {
      name: "review",
      source: "command" as const,
      origin: "project" as const,
      description: "Review the diff",
      argumentHint: null,
    },
  ],
};

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom prompt path suggestion reads", () => {
  it("lists project commands with the provider and workspace selector", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(commandListing));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomProjectCommands({
        projectId: "proj_1",
        provider: "pi",
        environmentId: "env_1",
      }),
    ).resolves.toEqual(commandListing);

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(url.pathname).toBe("/api/v1/projects/proj_1/commands");
    expect(url.searchParams.get("provider")).toBe("pi");
    expect(url.searchParams.get("environmentId")).toBe("env_1");
    expect(url.searchParams.has("hostId")).toBe(false);
  });

  it("lists project paths with the contract query and path", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomProjectPaths({
        projectId: "proj_1",
        query: "Car",
        limit: "8",
        includeFiles: "true",
        includeDirectories: "true",
      }),
    ).resolves.toEqual(listing);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/projects/proj_1/paths");
    expect(url.searchParams.get("query")).toBe("Car");
    expect(url.searchParams.get("limit")).toBe("8");
    expect(url.searchParams.get("includeFiles")).toBe("true");
    expect(url.searchParams.get("includeDirectories")).toBe("true");
  });

  it("carries the environment or host routing selector as a query", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await loomProjectPaths({
      projectId: "proj_1",
      hostId: "host_1",
      includeFiles: "true",
      includeDirectories: "true",
    });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/projects/proj_1/paths");
    expect(url.searchParams.get("hostId")).toBe("host_1");
    expect(url.searchParams.has("environmentId")).toBe(false);
  });

  it("omits routing, query, and limit when the caller does not pass them", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await loomProjectPaths({
      projectId: "proj_1",
      includeFiles: "true",
      includeDirectories: "false",
    });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.searchParams.has("environmentId")).toBe(false);
    expect(url.searchParams.has("hostId")).toBe(false);
    expect(url.searchParams.has("query")).toBe(false);
    expect(url.searchParams.has("limit")).toBe(false);
    expect(url.searchParams.get("includeDirectories")).toBe("false");
  });

  it("refuses an empty project id before the request leaves", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomProjectPaths({
        projectId: "",
        includeFiles: "true",
        includeDirectories: "true",
      }),
    ).rejects.toBeInstanceOf(LoomApiPathParamError);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("lists environment paths with the contract query and path", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(listing));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomEnvironmentPaths({
        environmentId: "env_1",
        query: "src",
        includeFiles: "true",
        includeDirectories: "true",
      }),
    ).resolves.toEqual(listing);

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(url.pathname).toBe("/api/v1/environments/env_1/paths");
    expect(url.searchParams.get("query")).toBe("src");
  });

  it("surfaces a route failure as a loom HTTP error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ code: "project_not_found", message: "Project not found" }, 404),
      ),
    );

    await expect(
      loomProjectPaths({
        projectId: "proj_missing",
        includeFiles: "true",
        includeDirectories: "true",
      }),
    ).rejects.toMatchObject({
      status: 404,
      code: "project_not_found",
    });
    await expect(
      loomProjectPaths({
        projectId: "proj_missing",
        includeFiles: "true",
        includeDirectories: "true",
      }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the path and command reads into the browser SDK surface", () => {
    expect(sdk.projects.commands).toBe(loomProjectCommands);
    expect(sdk.projects.paths).toBe(loomProjectPaths);
    expect(sdk.environments.paths).toBe(loomEnvironmentPaths);
  });
});
