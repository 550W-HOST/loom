// @vitest-environment jsdom

import { act, cleanup, renderHook } from "@testing-library/react";
import type { PendingInteraction } from "@bb/domain";
import type { ResolvePendingInteractionRequest } from "@bb/server-contract";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createQueryClientTestHarness } from "@/test/queryClientTestHarness";
import { threadPendingInteractionsQueryKey } from "../queries/query-keys";
import { useResolveThreadPendingInteraction } from "./thread-interaction-mutations";

const mocks = vi.hoisted(() => ({
  resolve: vi.fn(),
}));

vi.mock("@/lib/sdk", () => ({
  sdk: { threads: { interactions: { resolve: mocks.resolve } } },
}));

function interaction(status: PendingInteraction["status"]): PendingInteraction {
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
    status,
    statusReason: null,
    createdAt: 1,
    resolvedAt: status === "resolved" ? 2 : null,
    resolution: status === "resolved" ? { decision: "deny" } : null,
    payload: {
      kind: "approval",
      reason: "Allow this command?",
      availableDecisions: ["allow_once", "deny"],
      subject: {
        kind: "command",
        itemId: "command-1",
        command: "ls",
        cwd: null,
        actions: [],
        sessionGrant: null,
      },
    },
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("thread interaction mutations", () => {
  it("forwards a typed resolution and invalidates the interaction-owned views", async () => {
    const resolved = interaction("resolved");
    mocks.resolve.mockResolvedValue(resolved);
    const resolution = {
      decision: "deny",
    } satisfies ResolvePendingInteractionRequest;
    const { queryClient, wrapper } = createQueryClientTestHarness();
    const invalidateQueries = vi.spyOn(queryClient, "invalidateQueries");
    const { result } = renderHook(() => useResolveThreadPendingInteraction(), {
      wrapper,
    });

    await act(async () => {
      await result.current.mutateAsync({
        interactionId: "pint_1",
        resolution,
        threadId: "thr_1",
      });
    });

    expect(mocks.resolve).toHaveBeenCalledWith({
      interactionId: "pint_1",
      resolution,
      threadId: "thr_1",
    });
    expect(invalidateQueries).toHaveBeenCalledWith(
      expect.objectContaining({
        queryKey: threadPendingInteractionsQueryKey("thr_1"),
      }),
    );
  });
});
