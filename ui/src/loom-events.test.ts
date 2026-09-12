import { describe, expect, it } from "vitest";
import { adaptThreadFrames, projectThreadFrames } from "./loom-events.js";
import type { RelayEventFrame } from "./types.js";

function frame(id: string, payload: unknown, createdAt = Number(id)): RelayEventFrame {
  return {
    type: "event",
    event_id: id,
    scope: { kind: "thread", id: "thread-1" },
    payload: JSON.stringify(payload),
    created_at_ms: createdAt,
  };
}

function contractRunFrame(
  id: string,
  event: Record<string, unknown>,
  createdAt = Number(id),
): RelayEventFrame {
  return frame(id, {
    type: "thread_run_event",
    thread_id: "thread-1",
    project_id: "project-1",
    run_id: "run-1",
    at_ms: createdAt,
    event,
  }, createdAt);
}

describe("loom event adapter", () => {
  it("passes contract events through with their camelCase fields", () => {
    const scope = { kind: "turn", turnId: "run-1" };
    const frames = [
      contractRunFrame("01", {
        type: "turn/started",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
      }),
      contractRunFrame("02", {
        type: "item/agentMessage/delta",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        itemId: "assistant-1",
        delta: "Hello",
      }),
      contractRunFrame("03", {
        type: "item/reasoning/textDelta",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        itemId: "reasoning-1",
        delta: "hmm",
      }),
      contractRunFrame("04", {
        type: "item/started",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        item: {
          type: "toolCall",
          id: "call-1",
          tool: "bash",
          arguments: { command: "pwd" },
          status: "pending",
        },
      }),
      contractRunFrame("05", {
        type: "item/completed",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        item: {
          type: "toolCall",
          id: "call-1",
          tool: "bash",
          arguments: { command: "pwd" },
          status: "completed",
          result: "/workspace",
        },
      }),
      contractRunFrame("06", {
        type: "item/completed",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        item: {
          type: "agentMessage",
          id: "assistant-1",
          text: "Hello",
        },
      }),
      contractRunFrame("07", {
        type: "provider/error",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        message: "host lost",
      }),
      contractRunFrame("08", {
        type: "turn/completed",
        threadId: "thread-1",
        scope,
        providerThreadId: "provider-1",
        status: "failed",
        error: { message: "host lost" },
      }),
    ];

    const adapted = adaptThreadFrames(frames);
    expect(adapted.rows.map((row) => row.type)).toEqual([
      "turn/started",
      "item/agentMessage/delta",
      "item/reasoning/textDelta",
      "item/started",
      "item/completed",
      "item/completed",
      "provider/error",
      "turn/completed",
    ]);
    expect(adapted.rows[1]).toMatchObject({
      threadId: "thread-1",
      scope,
      type: "item/agentMessage/delta",
      data: { itemId: "assistant-1", delta: "Hello" },
    });

    const projected = projectThreadFrames({ frames, threadName: "Demo", threadStatus: "error" });
    const text = JSON.stringify(projected.rows);
    expect(text).toContain("Hello");
    expect(text).toContain("hmm");
    expect(text).toContain("bash");
    expect(text).toContain("host lost");
  });

  it("keeps malformed contract events visible", () => {
    const projected = projectThreadFrames({
      frames: [
        contractRunFrame("01", {
          type: "future/event",
          threadId: "thread-1",
          scope: { kind: "turn", turnId: "run-1" },
          unsupportedValue: 42,
        }),
      ],
      threadName: "Demo",
      threadStatus: "idle",
    });
    expect(JSON.stringify(projected.rows)).toContain("future/event");
  });

  it("maps a user message, streaming thinking/text, a tool pair, and a failed run", () => {
    const frames = [
      frame("01", { type: "thread_message_added", thread_id: "thread-1", message: { id: "m1", role: "user", content: "Inspect" } }),
      frame("02", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "started", provider: "pi" } }),
      frame("03", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "output", stream: "thinking", text: "hmm" } }),
      frame("04", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "output", stream: "assistant", text: "Hello" } }),
      frame("05", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "tool_call", tool_call_id: "call-1", name: "bash", args: { command: "pwd" } } }),
      frame("06", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "tool_result", tool_call_id: "call-1", name: "bash", ok: true, output: "/workspace" } }),
      frame("07", { type: "thread_run_event", thread_id: "thread-1", run_id: "run-1", event: { type: "finished", outcome: "failed", error: "host lost" } }),
    ];
    const adapted = adaptThreadFrames(frames);
    expect(adapted.rows.map((row) => row.type)).toEqual([
      "client/turn/requested",
      "turn/started",
      "turn/input/accepted",
      "item/reasoning/textDelta",
      "item/agentMessage/delta",
      "item/started",
      "item/completed",
      "provider/error",
      "turn/completed",
    ]);
    const projected = projectThreadFrames({ frames, threadName: "Demo", threadStatus: "error" });
    const text = JSON.stringify(projected.rows);
    expect(text).toContain("Inspect");
    expect(text).toContain("Hello");
    expect(text).toContain("bash");
    expect(text).toContain("host lost");
  });

  it("keeps unknown payloads visible as a system timeline row", () => {
    const projected = projectThreadFrames({
      frames: [frame("01", { type: "future_event", thread_id: "thread-1", value: 42 })],
      threadName: "Demo",
      threadStatus: "idle",
    });
    expect(JSON.stringify(projected.rows)).toContain("future_event");
  });
});
