import type { PendingInteraction } from "@bb/domain";
import { describe, expect, it } from "vitest";
import { getLatestPendingInteraction } from "./thread-queries";

function interactionWithStatus(
  status: PendingInteraction["status"],
  createdAt: number,
): PendingInteraction {
  return {
    id: `${status}-${createdAt}`,
    threadId: "thread-1",
    turnId: "turn-1",
    providerId: "pi",
    providerThreadId: "provider-thread-1",
    providerRequestId: `request-${createdAt}`,
    origin: {
      kind: "provider",
      providerId: "pi",
      providerThreadId: "provider-thread-1",
      providerRequestId: `request-${createdAt}`,
    },
    status,
    statusReason: null,
    createdAt,
    resolvedAt: status === "resolved" ? createdAt + 1 : null,
    resolution: null,
    payload: {
      kind: "approval",
      reason: "Allow this command?",
      availableDecisions: ["allow_once", "deny"],
      subject: {
        kind: "command",
        itemId: `item-${createdAt}`,
        command: "ls",
        cwd: null,
        actions: [],
        sessionGrant: null,
      },
    },
  };
}

describe("pending interaction selection", () => {
  it("ignores settled history and selects the newest open interaction", () => {
    const resolved = interactionWithStatus("resolved", 3);
    const interrupted = interactionWithStatus("interrupted", 4);
    const pending = interactionWithStatus("pending", 2);
    const resolving = interactionWithStatus("resolving", 5);

    expect(
      getLatestPendingInteraction([resolved, interrupted, pending]),
    ).toBe(pending);
    expect(getLatestPendingInteraction([resolved, interrupted])).toBeNull();
    expect(getLatestPendingInteraction([pending, resolving])).toBe(resolving);
  });
});
