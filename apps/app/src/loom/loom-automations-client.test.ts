import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createLoomAutomationsClient } from "@/lib/loom-automations-client";
import { wsManager } from "@/lib/ws";

vi.mock("@/lib/ws", () => ({
  wsManager: {
    subscribe: vi.fn(),
    unsubscribe: vi.fn(),
    onChanged: vi.fn(() => vi.fn()),
    onConnected: vi.fn(() => vi.fn()),
  },
}));

const CHANGED_HANDLERS: Array<(message: unknown) => void> = [];
const CONNECTED_HANDLERS: Array<() => void> = [];

function stubFetch(body: unknown) {
  const fetchMock = vi.fn(
    async (_input: RequestInfo | URL, _init?: RequestInit) =>
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

/** Just the part of a fetch mock these assertions read. */
interface FetchMock {
  mock: { calls: Array<[unknown, RequestInit | undefined]> };
}

function requestAt(fetchMock: FetchMock): { init: RequestInit; url: URL } {
  const call = fetchMock.mock.calls.at(-1);
  if (!call) {
    throw new Error("expected a fetch call");
  }
  return { url: new URL(String(call[0])), init: call[1] ?? {} };
}

beforeEach(() => {
  CHANGED_HANDLERS.length = 0;
  CONNECTED_HANDLERS.length = 0;
  vi.mocked(wsManager.onChanged).mockImplementation((handler) => {
    CHANGED_HANDLERS.push(handler as (message: unknown) => void);
    return () => {};
  });
  vi.mocked(wsManager.onConnected).mockImplementation((handler) => {
    CONNECTED_HANDLERS.push(handler as () => void);
    return () => {};
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("loom automations client", () => {
  it("calls each operation on its own loom route", async () => {
    const client = createLoomAutomationsClient();

    // The whole operation map, so a new operation cannot quietly fall back to
    // an "unavailable" rejection.
    const fetchMock = stubFetch({ runs: [], nextCursor: null });
    await client.call("automations_overview");
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/automations"),
      init: { method: "GET" },
    });

    await client.call("automations_list", { projectId: "proj_1" });
    expect(requestAt(fetchMock).url.pathname).toBe("/api/v1/projects/proj_1/automations");

    await client.call("automations_get", { projectId: "proj_1", automationId: "auto_1" });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/projects/proj_1/automations/auto_1"),
      init: { method: "GET" },
    });

    await client.call("automations_create", {
      projectId: "proj_1",
      name: "nightly",
      enabled: true,
      trigger: { triggerType: "schedule", cron: "0 3 * * *", timezone: "UTC" },
      execution: { mode: "script", script: "echo hi", interpreter: "bash", timeoutMs: 5_000 },
      origin: "human",
    });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/projects/proj_1/automations"),
      init: { method: "POST" },
    });
    // The project is in the path, not the body.
    expect(JSON.parse(String(requestAt(fetchMock).init.body))).toMatchObject({
      name: "nightly",
      origin: "human",
    });

    await client.call("automations_update", {
      projectId: "proj_1",
      automationId: "auto_1",
      name: "renamed",
    });
    expect(requestAt(fetchMock)).toMatchObject({
      init: { method: "PATCH" },
    });
    expect(JSON.parse(String(requestAt(fetchMock).init.body))).toEqual({ name: "renamed" });

    await client.call("automations_delete", { projectId: "proj_1", automationId: "auto_1" });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/projects/proj_1/automations/auto_1"),
      init: { method: "DELETE" },
    });

    await client.call("automations_pause", { projectId: "proj_1", automationId: "auto_1" });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/projects/proj_1/automations/auto_1/pause"),
      init: { method: "POST" },
    });

    await client.call("automations_resume", { projectId: "proj_1", automationId: "auto_1" });
    expect(requestAt(fetchMock).url.pathname).toBe(
      "/api/v1/projects/proj_1/automations/auto_1/resume",
    );

    await client.call("automations_run", {
      projectId: "proj_1",
      automationId: "auto_1",
      idempotencyKey: "once",
    });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL("http://localhost/api/v1/projects/proj_1/automations/auto_1/run"),
      init: { method: "POST" },
    });
    expect(JSON.parse(String(requestAt(fetchMock).init.body))).toEqual({
      idempotencyKey: "once",
    });

    await client.call("automations_runs", {
      projectId: "proj_1",
      automationId: "auto_1",
      limit: 25,
      cursor: "cur_1",
    });
    expect(requestAt(fetchMock)).toMatchObject({
      url: new URL(
        "http://localhost/api/v1/projects/proj_1/automations/auto_1/runs?limit=25&cursor=cur_1",
      ),
      init: { method: "GET" },
    });
  });

  it("reports a project change as both signals and unsubscribes when the last listener leaves", () => {
    const client = createLoomAutomationsClient();
    const first = vi.fn();
    const second = vi.fn();
    const unsubscribeFirst = client.subscribe(first);
    expect(wsManager.subscribe).toHaveBeenCalledWith({ kind: "project-list" });
    const unsubscribeSecond = client.subscribe(second);
    // One socket target for the client, however many listeners it has.
    expect(vi.mocked(wsManager.subscribe).mock.calls).toHaveLength(1);

    for (const handler of CHANGED_HANDLERS) {
      handler({ type: "changed", entity: "project", id: "proj_1", changes: ["project-updated"] });
    }
    // Both kinds: the list and the detail refetch on either, the run history
    // only on `automation-runs-changed`, and the server's frame does not say
    // which one changed.
    expect(first.mock.calls.map((call) => call[0])).toEqual([
      { projectId: "proj_1", kind: "automations-changed" },
      { projectId: "proj_1", kind: "automation-runs-changed" },
    ]);
    expect(second).toHaveBeenCalledTimes(2);

    // A frame about something else is not an automation change.
    for (const handler of CHANGED_HANDLERS) {
      handler({ type: "changed", entity: "thread", id: "thr_1", changes: ["title-changed"] });
      handler({ type: "changed", entity: "project", changes: ["project-updated"] });
    }
    expect(first).toHaveBeenCalledTimes(2);

    unsubscribeSecond();
    expect(wsManager.unsubscribe).not.toHaveBeenCalled();
    unsubscribeFirst();
    expect(wsManager.unsubscribe).toHaveBeenCalledWith({ kind: "project-list" });
  });

  it("re-announces the projects it served after a reconnect", async () => {
    const client = createLoomAutomationsClient();
    const listener = vi.fn();
    const unsubscribe = client.subscribe(listener);
    const fetchMock = stubFetch({ runs: [], nextCursor: null });

    await client.call("automations_runs", { projectId: "proj_9", automationId: "auto_1", limit: 50 });
    expect(requestAt(fetchMock).url.pathname).toContain("/projects/proj_9/");

    for (const onConnected of CONNECTED_HANDLERS) {
      onConnected();
    }
    // A reconnect is a gap: the view reading this project has to refetch.
    expect(listener.mock.calls.map((call) => call[0])).toEqual([
      { projectId: "proj_9", kind: "automations-changed" },
      { projectId: "proj_9", kind: "automation-runs-changed" },
    ]);
    unsubscribe();
  });
});
