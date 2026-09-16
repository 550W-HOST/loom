import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  buildLoomPairingCommand,
  createLoomJoinCode,
  createLoomMachineCode,
  isPairingServerUrlUnreachable,
  resolvePairingServerUrl,
} from "@/lib/loom-machine-pairing";
import { useHosts } from "@/hooks/queries/host-queries";

/**
 * The pairing and host-list workflows this issue changed, exercised against the
 * same-origin transport.
 *
 * The ported `AddMachineDialog.test.tsx` cannot run here: it mocks the removed
 * `@/lib/sdk` plugin surface and imports `@bb/test-helpers`, which this
 * workspace does not have. These tests are in the collected set, so a
 * regression in the paths this issue touched fails CI rather than sitting in an
 * uncollected file.
 */

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom machine pairing", () => {
  it("reports the machine code as unavailable rather than faking one", async () => {
    await expect(createLoomMachineCode()).resolves.toEqual({
      kind: "unavailable",
    });
  });

  it("derives the address from the app origin when the server sends an empty one", () => {
    // loom's `system.config` answers `serverUrl: ""`. Taking that at face value
    // produced `... --server ` — a command that looks runnable and cannot work.
    expect(resolvePairingServerUrl("", "https://loom.example.com")).toBe(
      "https://loom.example.com",
    );
    expect(resolvePairingServerUrl(null, "https://loom.example.com")).toBe(
      "https://loom.example.com",
    );
    // A real configured URL still wins.
    expect(
      resolvePairingServerUrl("https://public.example.com", "http://localhost"),
    ).toBe("https://public.example.com");
    // A trailing slash is trimmed so the path does not double up.
    expect(
      resolvePairingServerUrl("https://public.example.com/", "http://localhost"),
    ).toBe("https://public.example.com");
  });

  it("returns no address when neither source is a usable URL", () => {
    expect(resolvePairingServerUrl("", null)).toBeNull();
    expect(resolvePairingServerUrl("not-a-url", "")).toBeNull();
  });

  it("builds a runnable command for a reachable address", () => {
    const result = buildLoomPairingCommand({
      joinCode: "join-123",
      hostId: "host-1",
      configuredServerUrl: "",
      origin: "https://loom.example.com",
    });
    expect(result.kind).toBe("ready");
    if (result.kind !== "ready") throw new Error("expected ready");
    expect(result.serverUrl).toBe("https://loom.example.com");
    expect(result.command).toContain("--join-code join-123");
    expect(result.command).toContain("--host-id host-1");
    expect(result.command).toContain("--server https://loom.example.com");
    // The exact bug: no empty `--server` value anywhere in the command.
    expect(result.command).not.toMatch(/--server\s+($|\|)/u);
    expect(result.command).toContain(
      "https://loom.example.com/install.sh",
    );
  });

  it("reports a local-only address as unreachable instead of printing a command", () => {
    for (const local of [
      "http://localhost:38886",
      "http://127.0.0.1:38886",
      "http://[::1]:38886",
      "http://0.0.0.0:38886",
    ]) {
      expect(isPairingServerUrlUnreachable(local)).toBe(true);
      const result = buildLoomPairingCommand({
        joinCode: "join-123",
        hostId: "host-1",
        configuredServerUrl: local,
        origin: local,
      });
      expect(result.kind).toBe("unreachable");
      expect("command" in result).toBe(false);
    }
  });

  it("mints a join code through the POST contract route", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({ joinCode: "abc", hostId: "h1", expiresAt: 999 }, 201),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(createLoomJoinCode()).resolves.toEqual({
      joinCode: "abc",
      hostId: "h1",
      expiresAt: 999,
    });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(String(url)).toBe(`${window.location.origin}/api/v1/hosts/join-codes`);
    expect(init.method).toBe("POST");
  });

  it("rejects a malformed join-code response instead of trusting it", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ joinCode: "", hostId: "h1", expiresAt: 1 }, 201)),
    );
    await expect(createLoomJoinCode()).rejects.toThrow();
  });
});

describe("loom host list", () => {
  function Probe() {
    const hosts = useHosts();
    if (hosts.isPending) return <p>loading</p>;
    if (hosts.isError) return <p>error: {(hosts.error as Error).message}</p>;
    return <p>hosts: {hosts.data?.length ?? 0}</p>;
  }

  function renderHosts() {
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, retryDelay: 0 } },
    });
    return render(
      <QueryClientProvider client={client}>
        <Probe />
      </QueryClientProvider>,
    );
  }

  it("reads the host list from the contract route", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse([
        {
          id: "host_1",
          name: "Machine",
          status: "connected",
          createdAt: 1,
          updatedAt: 1,
        },
      ]),
    );
    vi.stubGlobal("fetch", fetchMock);

    renderHosts();

    await waitFor(() => expect(screen.getByText("hosts: 1")).toBeDefined());
    const [url] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(String(url)).toBe(`${window.location.origin}/api/v1/hosts`);
  });

  it("surfaces a host-list error instead of waiting forever", async () => {
    // The previous implementation went through a fail-closed SDK stub, so the
    // Add Machine dialog's connection check never resolved either way.
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ code: "boom", message: "hosts down" }, 503)),
    );

    renderHosts();

    await waitFor(() =>
      expect(screen.getByText(/HTTP 503: hosts down/u)).toBeDefined(),
    );
  });
});
