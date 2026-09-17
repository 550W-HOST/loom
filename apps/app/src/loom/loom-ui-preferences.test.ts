import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomListUiPreferences,
  loomResetUiPreference,
  loomSetUiPreference,
} from "@/lib/loom-ui-preferences";

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function stubFetch(body: unknown) {
  const fetchMock = vi.fn(
    async (
      _input: RequestInfo | URL,
      _init?: RequestInit,
    ): Promise<Response> => jsonResponse(body),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

function lastRequest(fetchMock: ReturnType<typeof stubFetch>) {
  const call = fetchMock.mock.calls.at(-1);
  if (!call) throw new Error("expected a fetch call");
  return {
    url: new URL(String(call[0])),
    init: call[1] ?? {},
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom UI preference adapter", () => {
  it("lists preferences through the same-origin B10 route", async () => {
    const body = { preferences: {} };
    const fetchMock = stubFetch(body);

    await expect(loomListUiPreferences()).resolves.toEqual(body);
    const request = lastRequest(fetchMock);
    expect(request.url.pathname).toBe("/api/v1/preferences/ui");
    expect(request.init.method).toBe("GET");
    expect(request.init.body).toBeUndefined();
  });

  it("updates one preference with its revision and typed value", async () => {
    const body = {
      key: "sidebar.collapsedProjects",
      revision: 2,
      value: ["project-1"],
    };
    const fetchMock = stubFetch(body);

    await expect(
      loomSetUiPreference({
        expectedRevision: 1,
        key: "sidebar.collapsedProjects",
        value: ["project-1"],
      }),
    ).resolves.toEqual(body);
    const request = lastRequest(fetchMock);
    expect(request.url.pathname).toBe(
      "/api/v1/preferences/ui/sidebar.collapsedProjects",
    );
    expect(request.init.method).toBe("PUT");
    expect(JSON.parse(String(request.init.body))).toEqual({
      expectedRevision: 1,
      value: ["project-1"],
    });
  });

  it("resets one preference without a request body", async () => {
    const body = {
      key: "sidebar.collapsedProjects",
      revision: 3,
      value: [],
    };
    const fetchMock = stubFetch(body);

    await expect(
      loomResetUiPreference({ key: "sidebar.collapsedProjects" }),
    ).resolves.toEqual(body);
    const request = lastRequest(fetchMock);
    expect(request.url.pathname).toBe(
      "/api/v1/preferences/ui/sidebar.collapsedProjects",
    );
    expect(request.init.method).toBe("DELETE");
    expect(request.init.body).toBeUndefined();
  });

  it("overrides exactly the product preference operations", () => {
    expect(sdk.system.uiPreferences.list).toBe(loomListUiPreferences);
    expect(sdk.system.uiPreferences.set).toBe(loomSetUiPreference);
    expect(sdk.system.uiPreferences.reset).toBe(loomResetUiPreference);
  });
});
