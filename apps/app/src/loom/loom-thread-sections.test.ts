import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomCreateThreadSection,
  loomDeleteThreadSection,
  loomUpdateThreadSection,
} from "@/lib/loom-thread-sections";
import { LoomHttpError, resolveLoomApiMethod } from "@/lib/loom-http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const SECTION = { id: "sec1", name: "Work", createdAt: 1, updatedAt: 2 };
const MUTATION = { id: "sec1", name: "Work", updatedThreadCount: 0 };

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom thread sections", () => {
  it("creates with POST and the name body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(SECTION, 201));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomCreateThreadSection({ name: "Work" }),
    ).resolves.toEqual(SECTION);

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("threadSections.create")).toBe("POST");
    expect(init.method).toBe("POST");
    expect(url.pathname).toBe("/api/v1/thread-sections");
    expect(url.search).toBe("");
    expect(JSON.parse(String(init.body))).toEqual({ name: "Work" });
  });

  it("renames with PATCH and the id in the body rather than the path", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(MUTATION));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomUpdateThreadSection({ id: "sec1", name: "Work" }),
    ).resolves.toEqual(MUTATION);

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("threadSections.update")).toBe("PATCH");
    expect(init.method).toBe("PATCH");
    expect(url.pathname).toBe("/api/v1/thread-sections");
    expect(JSON.parse(String(init.body))).toEqual({ id: "sec1", name: "Work" });
  });

  it("deletes with a JSON body on DELETE", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(MUTATION));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomDeleteThreadSection({ id: "sec1" }),
    ).resolves.toEqual(MUTATION);

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("threadSections.delete")).toBe("DELETE");
    expect(init.method).toBe("DELETE");
    expect(url.pathname).toBe("/api/v1/thread-sections");
    expect(JSON.parse(String(init.body))).toEqual({ id: "sec1" });
  });

  it("surfaces the server's refusal instead of a fake section", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ code: "conflict", message: "section exists" }, 409),
      ),
    );

    await expect(
      loomCreateThreadSection({ name: "Work" }),
    ).rejects.toMatchObject({ status: 409, code: "conflict" });
    await expect(
      loomCreateThreadSection({ name: "Work" }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires every section write into the browser SDK surface", () => {
    expect(sdk.threadSections.create).toBe(loomCreateThreadSection);
    expect(sdk.threadSections.update).toBe(loomUpdateThreadSection);
    expect(sdk.threadSections.delete).toBe(loomDeleteThreadSection);
  });
});
