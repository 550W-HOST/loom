import { threadScope, turnScope } from "@bb/domain";
import type {
  ThreadEventRow,
  ThreadEventScope,
  ThreadEventType,
  ThreadStatus,
} from "@bb/domain";
import { describe, expect, it } from "vitest";
import { buildEventProjection } from "../src/build-event-projection.js";
import { decodeThreadEventRow } from "../src/event-decode.js";
import type {
  EventProjection,
  EventProjectionMessage,
  EventProjectionTurn,
} from "../src/event-projection-types.js";

const THREAD_ID = "thread-1";
const PROVIDER_THREAD_ID = "provider-thread-1";

function eventRow(
  seq: number,
  type: ThreadEventType,
  data: Record<string, unknown>,
  scope: ThreadEventScope = turnScope("turn-1"),
): ThreadEventRow {
  return {
    id: `event-${seq}`,
    scope,
    threadId: THREAD_ID,
    seq,
    type,
    data,
    createdAt: seq,
  };
}

function project(
  rows: readonly ThreadEventRow[],
  threadStatus: ThreadStatus = "idle",
): EventProjection {
  return buildEventProjection(rows.map(decodeThreadEventRow), {
    threadName: "Example thread",
    threadStatus,
    turnMessageDetail: "full",
  });
}

function turnEntries(projection: EventProjection): EventProjectionTurn[] {
  return projection.entries.flatMap((entry) =>
    entry.kind === "turn" ? [entry.turn] : [],
  );
}

function turnMessages(turn: EventProjectionTurn): EventProjectionMessage[] {
  return turn.messages ?? [];
}

function providerFields(turnId = "turn-1") {
  return {
    providerThreadId: PROVIDER_THREAD_ID,
    turnId,
  };
}

describe("thread-view projection baseline", () => {
  it("projects a reasoning delta as a thinking row", () => {
    const rows = [
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "item/reasoning/textDelta", {
        ...providerFields(),
        itemId: "reasoning-1",
        delta: "**Calculating distinct letter arrangements**\n\n",
      }),
      eventRow(3, "item/reasoning/textDelta", {
        ...providerFields(),
        itemId: "reasoning-1",
        delta: "There are 3 As and 2 Ns, so 60 arrangements.",
      }),
      eventRow(4, "item/agentMessage/delta", {
        ...providerFields(),
        itemId: "assistant-1",
        delta: "60\n",
      }),
      eventRow(5, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
    ];

    const turn = turnEntries(project(rows))[0];
    expect(turn).toBeDefined();
    // Thinking arrives as reasoning deltas and nothing else — the worker emits
    // no `item/started` for it — so a projection that needed one would show the
    // answer with no thinking above it, which is exactly the report this covers.
    expect(turnMessages(turn!)).toEqual([
      expect.objectContaining({
        kind: "operation",
        opType: "reasoning",
        status: "completed",
        detail:
          "**Calculating distinct letter arrangements**\n\nThere are 3 As and 2 Ns, so 60 arrangements.",
      }),
      expect.objectContaining({
        kind: "assistant-text",
        text: "60\n",
      }),
    ]);
  });

  it("merges assistant deltas in arrival order and flushes the final chunk", () => {
    const prefix = [
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "item/agentMessage/delta", {
        ...providerFields(),
        itemId: "assistant-1",
        delta: "Hello ",
      }),
      eventRow(3, "item/agentMessage/delta", {
        ...providerFields(),
        itemId: "assistant-1",
        delta: "world\n",
      }),
    ];

    const streamingTurn = turnEntries(project(prefix, "active"))[0];
    expect(streamingTurn).toBeDefined();
    expect(turnMessages(streamingTurn!)).toHaveLength(1);
    expect(turnMessages(streamingTurn!)[0]).toMatchObject({
      kind: "assistant-text",
      text: "Hello world\n",
      status: "streaming",
      sourceSeqStart: 2,
      sourceSeqEnd: 3,
    });

    const completed = project([
      ...prefix,
      eventRow(4, "item/completed", {
        ...providerFields(),
        item: {
          type: "agentMessage",
          id: "assistant-1",
          text: "Hello world\nDone.",
        },
      }),
      eventRow(5, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
    ]);
    const completedTurn = turnEntries(completed)[0];
    expect(completedTurn).toBeDefined();
    expect(turnMessages(completedTurn!)).toEqual([
      expect.objectContaining({
        kind: "assistant-text",
        text: "Hello world\nDone.",
        status: "completed",
        sourceSeqStart: 2,
        sourceSeqEnd: 4,
      }),
    ]);
  });

  it("pairs a tool call start and completion into one projected message", () => {
    const projection = project([
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "item/started", {
        ...providerFields(),
        item: {
          type: "toolCall",
          id: "tool-1",
          tool: "lookup",
          arguments: { query: "loom" },
          status: "pending",
        },
      }),
      eventRow(3, "item/toolCall/progress", {
        ...providerFields(),
        itemId: "tool-1",
        message: "Searching",
      }),
      eventRow(4, "item/completed", {
        ...providerFields(),
        item: {
          type: "toolCall",
          id: "tool-1",
          tool: "lookup",
          arguments: { query: "loom" },
          status: "completed",
          result: { answer: "found" },
        },
      }),
      eventRow(5, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
    ]);

    const turn = turnEntries(projection)[0];
    expect(turn).toBeDefined();
    const messages = turnMessages(turn!);
    expect(messages).toHaveLength(1);
    expect(messages[0]).toMatchObject({
      kind: "tool-call",
      callId: "tool-1",
      toolName: "lookup",
      toolArgs: { query: "loom" },
      output: '{"answer":"found"}',
      status: "completed",
      sourceSeqStart: 2,
      sourceSeqEnd: 4,
      completedAt: 4,
    });
  });

  it("groups independently completed turns in source order", () => {
    const rows = [
      eventRow(1, "turn/started", providerFields("turn-a"), turnScope("turn-a")),
      eventRow(
        2,
        "item/completed",
        {
          ...providerFields("turn-a"),
          item: {
            type: "agentMessage",
            id: "assistant-a",
            text: "first",
          },
        },
        turnScope("turn-a"),
      ),
      eventRow(
        3,
        "turn/completed",
        { ...providerFields("turn-a"), status: "completed" },
        turnScope("turn-a"),
      ),
      eventRow(4, "turn/started", providerFields("turn-b"), turnScope("turn-b")),
      eventRow(
        5,
        "item/completed",
        {
          ...providerFields("turn-b"),
          item: {
            type: "agentMessage",
            id: "assistant-b",
            text: "second",
          },
        },
        turnScope("turn-b"),
      ),
      eventRow(
        6,
        "turn/completed",
        { ...providerFields("turn-b"), status: "completed" },
        turnScope("turn-b"),
      ),
    ];

    const turns = turnEntries(project(rows));
    expect(turns.map((turn) => turn.turnId)).toEqual(["turn-a", "turn-b"]);
    expect(turnMessages(turns[0]!)).toEqual([
      expect.objectContaining({ text: "first" }),
    ]);
    expect(turnMessages(turns[1]!)).toEqual([
      expect.objectContaining({ text: "second" }),
    ]);
  });

  it("keeps a post-turn compaction pending until the matching boundary event", () => {
    const active = project([
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
      eventRow(3, "item/started", {
        ...providerFields(),
        item: {
          type: "contextCompaction",
          id: "compaction-1",
        },
      }),
    ]);
    expect(active.entries).toHaveLength(1);
    expect(active.entries[0]).toMatchObject({ kind: "turn" });

    const pendingCompaction = turnMessages(
      turnEntries(active)[0]!,
    ).find((message) => message.kind === "operation");
    expect(pendingCompaction).toMatchObject({
      kind: "operation",
      opType: "compaction",
      status: "pending",
      title: "Compacting context",
      completedAt: null,
    });

    const completed = project([
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
      eventRow(3, "item/started", {
        ...providerFields(),
        item: {
          type: "contextCompaction",
          id: "compaction-1",
        },
      }),
      eventRow(4, "thread/compacted", {
        ...providerFields(),
        threadId: THREAD_ID,
      }),
    ]);
    const settledCompaction = turnMessages(
      turnEntries(completed)[0]!,
    ).find((message) => message.kind === "operation");
    expect(settledCompaction).toMatchObject({
      kind: "operation",
      opType: "compaction",
      status: "completed",
      title: "Context compacted",
      completedAt: 4,
    });
  });

  it("does not render legacy extension rows as source timeline messages", () => {
    const projection = project([
      eventRow(1, "turn/started", providerFields()),
      eventRow(2, "item/completed", {
        ...providerFields(),
        item: {
          type: "extension",
          id: "extension-1",
          kind: "legacy/example",
          payload: { value: true },
          status: "completed",
          presentation: { glyph: "legacy/example" },
        },
      }),
      eventRow(3, "turn/completed", {
        ...providerFields(),
        status: "completed",
      }),
    ]);

    const messages = projection.entries.flatMap((entry) =>
      entry.kind === "turn" ? turnMessages(entry.turn) : [entry.message],
    );
    expect(messages).toEqual([]);
  });
});

describe("thread-view event scope fixture", () => {
  it("can represent thread-scoped rows alongside turn-scoped rows", () => {
    const row = eventRow(1, "system/error", { message: "offline" }, threadScope());
    expect(decodeThreadEventRow(row).event.scope).toEqual({ kind: "thread" });
  });
});
