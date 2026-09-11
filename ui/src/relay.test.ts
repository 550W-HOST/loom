import { describe, expect, it } from "vitest";
import {
  mergeRelayFrames,
  replayAllPages,
  ThreadRelaySubscription,
} from "./relay.js";
import type { RelayEventFrame } from "./types.js";

function frame(id: string): RelayEventFrame {
  return {
    type: "event",
    event_id: id,
    scope: { kind: "thread", id: "thread-1" },
    payload: JSON.stringify({ id }),
    created_at_ms: Number(id),
  };
}

describe("relay resume", () => {
  it("merges live and replay frames by event id in event order", () => {
    expect(mergeRelayFrames([frame("03"), frame("01"), frame("02"), frame("02")]).map((item) => item.event_id)).toEqual(["01", "02", "03"]);
  });

  it("pages until has_more is false and advances from the last page frame", async () => {
    const calls: (string | null)[] = [];
    const pages = [
      { frames: [frame("02"), frame("03")], has_more: true },
      { frames: [frame("04")], has_more: true },
      { frames: [frame("05")], has_more: false },
    ];
    const result = await replayAllPages(async ({ since }) => {
      calls.push(since);
      return pages[calls.length - 1]!;
    }, "thread-1", "01");
    expect(calls).toEqual(["01", "03", "04"]);
    expect(result.map((item) => item.event_id)).toEqual(["02", "03", "04", "05"]);
  });

  it("stops if a malformed page cannot provide a cursor", async () => {
    const result = await replayAllPages(async () => ({ frames: [{ type: "welcome" }], has_more: true }), "thread-1", null);
    expect(result).toEqual([]);
  });

  it("waits for the matching subscription ack before replaying", async () => {
    const socket = {
      onopen: null as (() => void) | null,
      onmessage: null as ((event: { data: unknown }) => void) | null,
      onclose: null as (() => void) | null,
      onerror: null as (() => void) | null,
      send() {},
      close() {},
    };
    const cursors: string[] = [];
    const received: string[][] = [];
    const subscription = new ThreadRelaySubscription({
      threadId: "thread-1",
      wsUrl: "ws://loom.test/ws",
      createSocket: () => socket,
      cursorStore: {
        get() {
          return "03";
        },
        set() {},
      },
      loadReplay: async ({ since }) => {
        cursors.push(since ?? "<none>");
        return { frames: [frame("01")], has_more: false };
      },
      onFrames(frames) {
        received.push(frames.map((item) => item.event_id));
      },
      onNotice() {},
      onState() {},
    });
    subscription.start();
    socket.onopen?.();
    socket.onmessage?.({
      data: JSON.stringify({
        type: "subscribed",
        scope: { kind: "thread", id: "other-thread" },
        first_subscriber: false,
      }),
    });
    socket.onmessage?.({ data: JSON.stringify(frame("02")) });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(cursors).toEqual([]);
    expect(received).toEqual([]);

    socket.onmessage?.({
      data: JSON.stringify({
        type: "subscribed",
        scope: { kind: "thread", id: "thread-1" },
        first_subscriber: false,
      }),
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(cursors).toEqual(["<none>"]);
    expect(received).toEqual([["01", "02"]]);
    subscription.stop();
  });
});
