import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createLoomJoinCode,
  LOOM_MACHINE_INSTALL_AVAILABLE,
  LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON,
  resolveLoomPairingState,
} from "@/lib/loom-machine-pairing";
import { useHosts } from "@/hooks/queries/host-queries";

/**
 * The pairing and host-list workflows this issue changed.
 *
 * loom serves no `/install.sh` (only `/install/version` and
 * `/install/loom-worker`), and enrolling a machine is `loom worker
 * --server-url … --join-code …` rather than a one-line install command, so this
 * phase cannot produce a working install command. The tests therefore assert
 * the *absence* of one: an unusable command must not be representable, not
 * merely discouraged.
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

describe("loom machine pairing fails closed", () => {
  it("reports the install step as unavailable", () => {
    expect(LOOM_MACHINE_INSTALL_AVAILABLE).toBe(false);
    const state = resolveLoomPairingState({
      joinCode: "jc_1",
      hostId: "h1",
      expiresAt: 1,
    });
    expect(state.kind).toBe("unavailable");
    expect(state.reason).toBe(LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON);
    // The reason names the real situation rather than a generic failure.
    expect(state.reason).toMatch(/installer|install/u);
  });

  it("has no state that can carry a command", () => {
    // If a `ready`/`command` variant ever comes back, this stops compiling and
    // forces a deliberate decision about the installer.
    const state = resolveLoomPairingState({
      joinCode: "jc_1",
      hostId: "h1",
      expiresAt: 1,
    });
    expect("command" in state).toBe(false);
    expect(JSON.stringify(state)).not.toContain("/install.sh");
    expect(JSON.stringify(state)).not.toContain("curl");
  });

  it("still mints a join code through the POST contract route", async () => {
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
    expect(String(url)).toBe(
      `${window.location.origin}/api/v1/hosts/join-codes`,
    );
    expect(init.method).toBe("POST");
  });

  it("rejects a malformed join-code response instead of trusting it", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ joinCode: "", hostId: "h1", expiresAt: 1 }, 201),
      ),
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
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ code: "boom", message: "hosts down" }, 503),
      ),
    );

    renderHosts();

    await waitFor(() =>
      expect(screen.getByText(/HTTP 503: hosts down/u)).toBeDefined(),
    );
  });
});
