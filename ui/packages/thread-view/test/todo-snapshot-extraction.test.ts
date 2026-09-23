import { turnScope } from "@bb/domain";
import type { ThreadEventRow, ThreadEventType } from "@bb/domain";
import { describe, expect, it } from "vitest";
import { decodeThreadEventRow } from "../src/event-decode.js";
import { extractThreadTimelinePendingTodos } from "../src/todo-snapshot-extraction.js";

function eventRow(seq: number, type: ThreadEventType, data: Record<string, unknown>) {
  const row: ThreadEventRow = {
    id: `event-${seq}`,
    scope: turnScope("turn-1"),
    threadId: "thread-1",
    seq,
    type,
    data,
    createdAt: seq * 100,
  };
  return decodeThreadEventRow(row);
}

describe("thread timeline pending todos", () => {
  it("uses turn plan updates as todo snapshots", () => {
    const todos = extractThreadTimelinePendingTodos("active", [
      eventRow(12, "turn/plan/updated", {
        providerThreadId: "provider-thread-1",
        plan: [
          { step: "  Inspect the current flow  ", status: "completed" },
          { step: "Connect plan updates to this card", status: "active" },
          { step: "", status: "pending" },
          { step: "Run focused verification", status: "pending" },
          { step: "Handle the failed step", status: "failed" },
        ],
      }),
    ]);

    expect(todos).toEqual({
      sourceSeq: 12,
      updatedAt: 1200,
      items: [
        { id: "seq:12:0", text: "Inspect the current flow", status: "completed" },
        {
          id: "seq:12:1",
          text: "Connect plan updates to this card",
          status: "in_progress",
        },
        { id: "seq:12:3", text: "Run focused verification", status: "pending" },
        { id: "seq:12:4", text: "Handle the failed step", status: "completed" },
      ],
    });
  });

  it("lets a newer empty plan clear an older planSteps snapshot", () => {
    const todos = extractThreadTimelinePendingTodos("active", [
      eventRow(9, "turn/plan/updated", {
        providerThreadId: "provider-thread-1",
        plan: [],
      }),
      eventRow(4, "item/completed", {
        providerThreadId: "provider-thread-1",
        item: {
          type: "planSteps",
          id: "legacy-plan",
          steps: [{ step: "Old plan", status: "pending" }],
          status: "completed",
        },
      }),
    ]);

    expect(todos).toEqual({ sourceSeq: 9, updatedAt: 900, items: [] });
  });
});
