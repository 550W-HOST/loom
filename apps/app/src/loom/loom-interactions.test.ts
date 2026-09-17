import { afterEach, describe, expect, it, vi } from "vitest";
import type { PendingInteraction } from "@bb/domain";
import type { ResolvePendingInteractionRequest } from "@bb/server-contract";
import { BrowserSdkUnavailableError } from "@bb/sdk/browser";
import { sdk } from "@/lib/sdk";
import {
  loomCancelThreadInteraction,
  loomGetThreadInteraction,
  loomListThreadInteractions,
  loomResolveThreadInteraction,
} from "@/lib/loom-interactions";

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function interaction(): PendingInteraction {
  return {
    id: "pint_1",
    threadId: "thr_1",
    turnId: "turn_1",
    providerId: "pi",
    providerThreadId: "provider-thread-1",
    providerRequestId: "request-1",
    origin: {
      kind: "provider",
      providerId: "pi",
      providerThreadId: "provider-thread-1",
      providerRequestId: "request-1",
    },
    status: "pending",
    statusReason: null,
    createdAt: 1,
    resolvedAt: null,
    resolution: null,
    payload: {
      kind: "approval",
      reason: "Allow this command?",
      availableDecisions: ["allow_once", "allow_for_session", "deny"],
      subject: {
        kind: "permission_grant",
        itemId: "permission-1",
        toolName: "filesystem",
        permissions: {
          network: null,
          fileSystem: { read: ["/workspace"], write: [] },
        },
      },
    },
  };
}

function stubFetch(body: unknown = interaction()) {
  const fetchMock = vi.fn(
    async (
      _input: RequestInfo | URL,
      _init?: RequestInit,
    ): Promise<Response> => jsonResponse(body),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

function requestAt(
  fetchMock: ReturnType<typeof stubFetch>,
): { init: RequestInit; url: URL } {
  const call = fetchMock.mock.calls.at(-1);
  if (!call) {
    throw new Error("expected a fetch call");
  }
  return {
    url: new URL(String(call[0])),
    init: call[1] ?? {},
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom thread interaction adapter", () => {
  it("lists provider interactions through the typed same-origin route", async () => {
    const fetchMock = stubFetch([interaction()]);

    await expect(
      loomListThreadInteractions({ threadId: "thr_1" }),
    ).resolves.toEqual([interaction()]);

    const request = requestAt(fetchMock);
    expect(request.url.pathname).toBe("/api/v1/threads/thr_1/interactions");
    expect(request.init.method).toBe("GET");
    expect(request.init.body).toBeUndefined();
  });

  it("gets one interaction with its encoded target parameters", async () => {
    const fetchMock = stubFetch();

    await loomGetThreadInteraction({
      interactionId: "pint/1",
      threadId: "thr/1",
    });

    const request = requestAt(fetchMock);
    expect(request.url.pathname).toBe(
      "/api/v1/threads/thr%2F1/interactions/pint%2F1",
    );
    expect(request.init.method).toBe("GET");
    expect(request.init.body).toBeUndefined();
  });

  it("resolves with the contract decision body and POST route", async () => {
    const fetchMock = stubFetch();
    const resolution = {
      decision: "allow_once",
      grantedPermissions: null,
    } satisfies ResolvePendingInteractionRequest;

    await loomResolveThreadInteraction({
      interactionId: "pint_1",
      resolution,
      threadId: "thr_1",
    });

    const request = requestAt(fetchMock);
    expect(request.url.pathname).toBe(
      "/api/v1/threads/thr_1/interactions/pint_1/resolve",
    );
    expect(request.init.method).toBe("POST");
    expect(JSON.parse(String(request.init.body))).toEqual(resolution);
  });

  it("cancels through the cancel route without disguising it as a denial", async () => {
    const fetchMock = stubFetch();

    await loomCancelThreadInteraction({
      interactionId: "pint_1",
      threadId: "thr_1",
    });

    const request = requestAt(fetchMock);
    expect(request.url.pathname).toBe(
      "/api/v1/threads/thr_1/interactions/pint_1/cancel",
    );
    expect(request.init.method).toBe("POST");
    expect(request.init.body).toBeUndefined();
  });

  it("overrides only the four UI operations and keeps respond fail-closed", async () => {
    expect(sdk.threads.interactions.cancel).toBe(loomCancelThreadInteraction);
    expect(sdk.threads.interactions.get).toBe(loomGetThreadInteraction);
    expect(sdk.threads.interactions.list).toBe(loomListThreadInteractions);
    expect(sdk.threads.interactions.resolve).toBe(
      loomResolveThreadInteraction,
    );
    await expect(
      sdk.threads.interactions.respond({
        interactionId: "pint_1",
        threadId: "thr_1",
        value: "unused",
      }),
    ).rejects.toBeInstanceOf(BrowserSdkUnavailableError);
  });
});
