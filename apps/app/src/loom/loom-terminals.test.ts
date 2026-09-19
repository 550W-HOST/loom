import { afterEach, describe, expect, it, vi } from "vitest";
import { sdk } from "@/lib/sdk";
import {
  loomCloseTerminal,
  loomCreateTerminal,
  loomListTerminals,
  loomRenameTerminal,
} from "@/lib/loom-terminals";
import { LoomHttpError, resolveLoomApiMethod } from "@/lib/loom-http";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const SESSION = { id: "term1", title: "shell", status: "running" };

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom terminals", () => {
  it("lists a thread's terminals as a flat query, not a kind field", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ sessions: [] }));
    vi.stubGlobal("fetch", fetchMock);

    await loomListTerminals({ scope: { kind: "thread", threadId: "t1" } });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("terminals.list")).toBe("GET");
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/terminals");
    expect(url.searchParams.get("threadId")).toBe("t1");
    expect(url.searchParams.has("kind")).toBe(false);
  });

  it("lists a host path scope with its cwd", async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ sessions: [] }));
    vi.stubGlobal("fetch", fetchMock);

    await loomListTerminals({
      scope: { cwd: "/home/me", hostId: "h1", kind: "host_path" },
    });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.searchParams.get("hostId")).toBe("h1");
    expect(url.searchParams.get("cwd")).toBe("/home/me");
  });

  it("creates with POST and renames scope to target", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(SESSION, 201));
    vi.stubGlobal("fetch", fetchMock);

    await loomCreateTerminal({
      cols: 80,
      rows: 24,
      scope: { kind: "thread", threadId: "t1" },
    });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("terminals.create")).toBe("POST");
    expect(url.pathname).toBe("/api/v1/terminals");
    expect(JSON.parse(String(init.body))).toEqual({
      cols: 80,
      rows: 24,
      target: { kind: "thread", threadId: "t1" },
    });
  });

  it("renames with PATCH on the terminal's path", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(SESSION));
    vi.stubGlobal("fetch", fetchMock);

    await loomRenameTerminal({ terminalId: "term1", title: "build" });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("terminals.update")).toBe("PATCH");
    expect(url.pathname).toBe("/api/v1/terminals/term1");
    expect(JSON.parse(String(init.body))).toEqual({ title: "build" });
  });

  it("closes with POST and the literal user reason", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(SESSION));
    vi.stubGlobal("fetch", fetchMock);

    await loomCloseTerminal({ mode: "if-clean", terminalId: "term1" });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("terminals.close")).toBe("POST");
    expect(url.pathname).toBe("/api/v1/terminals/term1/close");
    expect(JSON.parse(String(init.body))).toEqual({
      mode: "if-clean",
      reason: "user",
    });
  });

  it("surfaces the server's refusal instead of a fake session", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ code: "not_found", message: "gone" }, 404),
      ),
    );

    await expect(
      loomRenameTerminal({ terminalId: "term1", title: "build" }),
    ).rejects.toBeInstanceOf(LoomHttpError);
  });

  it("wires the terminal operations into the browser SDK surface", () => {
    expect(sdk.terminals.close).toBe(loomCloseTerminal);
    expect(sdk.terminals.create).toBe(loomCreateTerminal);
    expect(sdk.terminals.list).toBe(loomListTerminals);
    expect(sdk.terminals.rename).toBe(loomRenameTerminal);
  });
});
