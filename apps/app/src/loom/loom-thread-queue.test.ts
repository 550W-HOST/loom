import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomCreateQueuedMessage,
  loomDeleteQueuedMessage,
  loomListQueuedMessages,
  loomReorderQueuedMessage,
  loomSendQueuedMessage,
  loomSetQueuedMessageGroupBoundary,
  loomUpdateQueuedMessage,
} from "@/lib/loom-thread-queue";
import { LoomHttpError, resolveLoomApiMethod } from "@/lib/loom-http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const INPUT = [{ mentions: [], text: "follow up", type: "text" as const }];
const QUEUED = { id: "q1", input: INPUT, updatedAt: 7 };

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function lastCall(fetchMock: ReturnType<typeof vi.fn>): [URL, RequestInit] {
  return fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
}

describe("loom queued messages", () => {
  it("lists a thread's queued rows with GET and no body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ queuedMessages: [] }));
    vi.stubGlobal("fetch", fetchMock);

    await loomListQueuedMessages({ threadId: "t1" });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.queuedMessages")).toBe("GET");
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages");
  });

  it("creates with POST and keeps threadId out of the body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(QUEUED, 201));
    vi.stubGlobal("fetch", fetchMock);

    await loomCreateQueuedMessage({ input: INPUT, threadId: "t1" });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.createQueuedMessage")).toBe("POST");
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages");
    expect(JSON.parse(String(init.body))).toEqual({ input: INPUT });
  });

  it("updates with PATCH and the optimistic-lock timestamp", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(QUEUED));
    vi.stubGlobal("fetch", fetchMock);

    await loomUpdateQueuedMessage({
      expectedUpdatedAt: 7,
      input: INPUT,
      queuedMessageId: "q1",
      threadId: "t1",
    });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.updateQueuedMessage")).toBe("PATCH");
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages/q1");
    expect(JSON.parse(String(init.body))).toEqual({
      expectedUpdatedAt: 7,
      input: INPUT,
    });
  });

  it("deletes with a bodyless DELETE on the row's own path", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomDeleteQueuedMessage({ queuedMessageId: "q1", threadId: "t1" }),
    ).resolves.toEqual({ ok: true });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.deleteQueuedMessage")).toBe("DELETE");
    expect(init.method).toBe("DELETE");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages/q1");
  });

  it("sends with POST and only the send mode in the body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true }));
    vi.stubGlobal("fetch", fetchMock);

    await loomSendQueuedMessage({
      mode: "steer",
      queuedMessageId: "q1",
      threadId: "t1",
    });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.sendQueuedMessage")).toBe("POST");
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages/q1/send");
    expect(JSON.parse(String(init.body))).toEqual({ mode: "steer" });
  });

  it("reorders with PATCH and drops an absent group boundary", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ queuedMessages: [] }));
    vi.stubGlobal("fetch", fetchMock);

    await loomReorderQueuedMessage({
      nextQueuedMessageId: "q2",
      previousQueuedMessageId: null,
      queuedMessageId: "q1",
      threadId: "t1",
    });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.reorderQueuedMessage")).toBe("PATCH");
    expect(url.pathname).toBe("/api/v1/threads/t1/queued-messages/q1/order");
    expect(JSON.parse(String(init.body))).toEqual({
      nextQueuedMessageId: "q2",
      previousQueuedMessageId: null,
    });
  });

  it("moves the group boundary with PATCH on the group-boundary route", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ queuedMessages: [] }));
    vi.stubGlobal("fetch", fetchMock);

    await loomSetQueuedMessageGroupBoundary({
      expectedGroupedPrefixQueuedMessageIds: ["q1"],
      groupBoundaryQueuedMessageId: "q1",
      threadId: "t1",
    });

    const [url, init] = lastCall(fetchMock);
    expect(resolveLoomApiMethod("threads.setQueuedMessageGroupBoundary")).toBe(
      "PATCH",
    );
    expect(url.pathname).toBe(
      "/api/v1/threads/t1/queued-messages/group-boundary",
    );
    expect(JSON.parse(String(init.body))).toEqual({
      expectedGroupedPrefixQueuedMessageIds: ["q1"],
      groupBoundaryQueuedMessageId: "q1",
    });
  });

  it("surfaces the server's refusal instead of a fake row", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ code: "conflict", message: "stale" }, 409)),
    );

    await expect(
      loomUpdateQueuedMessage({
        expectedUpdatedAt: 1,
        input: INPUT,
        queuedMessageId: "q1",
        threadId: "t1",
      }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the whole area into the browser SDK surface", () => {
    expect(sdk.threads.queuedMessages.create).toBe(loomCreateQueuedMessage);
    expect(sdk.threads.queuedMessages.delete).toBe(loomDeleteQueuedMessage);
    expect(sdk.threads.queuedMessages.list).toBe(loomListQueuedMessages);
    expect(sdk.threads.queuedMessages.reorder).toBe(loomReorderQueuedMessage);
    expect(sdk.threads.queuedMessages.send).toBe(loomSendQueuedMessage);
    expect(sdk.threads.queuedMessages.setGroupBoundary).toBe(
      loomSetQueuedMessageGroupBoundary,
    );
    expect(sdk.threads.queuedMessages.update).toBe(loomUpdateQueuedMessage);
  });
});
