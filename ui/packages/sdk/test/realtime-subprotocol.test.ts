import { describe, expect, it, vi } from "vitest";
import { BB_REALTIME_SUBPROTOCOL } from "@bb/domain";
import { createBbRealtimeClient } from "../src/realtime-client.js";
import type {
  BbRealtimeSocket,
  BbRealtimeSocketFactory,
  BbSdkTransport,
} from "../src/transport.js";

function socket(): BbRealtimeSocket {
  return {
    close: vi.fn(),
    onclose: null,
    onerror: null,
    onmessage: null,
    onopen: null,
    readyState: 0,
    send: vi.fn(),
  };
}

describe("BbRealtimeClient transport negotiation", () => {
  it("passes the explicit public protocol to a Node-style socket factory", () => {
    const calls: Array<{
      protocols: string | string[] | undefined;
      url: string;
    }> = [];
    const websocket: BbRealtimeSocketFactory = (url, protocols) => {
      calls.push({ protocols, url });
      return socket();
    };
    const transport = {
      baseUrl: "https://loom.example.test",
      realtimeUrl: "wss://loom.example.test/ws",
      runtime: "node",
      websocket,
    } as BbSdkTransport;
    const realtime = createBbRealtimeClient({ transport });

    const unsubscribe = realtime.subscribe({
      callback: vi.fn(),
      event: "thread:changed",
      threadId: "thr_test",
    });

    expect(calls).toEqual([
      {
        protocols: BB_REALTIME_SUBPROTOCOL,
        url: "wss://loom.example.test/ws",
      },
    ]);
    unsubscribe();
  });
});
