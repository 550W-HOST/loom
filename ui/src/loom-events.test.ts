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

describe("loom event adapter", () => {
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
