import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddMachineDialog } from "@/components/dialogs/AddMachineDialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { Host } from "@bb/domain";

/**
 * Component-level coverage for the Add Machine workflow this issue changed.
 *
 * It renders the real dialog against a stubbed `fetch`, so the pairing path is
 * exercised end to end: the join-code POST, the command actually shown, the
 * unreachable-address branch, and a host-list failure. The ported test for this
 * dialog cannot run in this workspace (it mocks the removed plugin SDK and
 * imports `@bb/test-helpers`), so without this the workflow would ship with no
 * gate at all.
 */

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function renderDialog(props: { serverUrl: string | null; onOpenChange?: (open: boolean) => void }) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, retryDelay: 0 } },
  });
  return render(
    <QueryClientProvider client={client}>
      <AddMachineDialog
        open
        onOpenChange={props.onOpenChange ?? vi.fn()}
        serverUrl={props.serverUrl}
      />
    </QueryClientProvider>,
  );
}

function routeFetch(args: {
  joinCode?: unknown;
  joinStatus?: number;
  hosts?: Host[];
  hostsStatus?: number;
}) {
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/hosts/join-codes")) {
      return jsonResponse(
        args.joinCode ?? { joinCode: "jc_1", hostId: "host_new", expiresAt: Date.now() + 900_000 },
        args.joinStatus ?? 201,
      );
    }
    if (url.endsWith("/hosts")) {
      return jsonResponse(args.hosts ?? [], args.hostsStatus ?? 200);
    }
    return jsonResponse({}, 200);
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("AddMachineDialog pairing workflow", () => {
  it("mints a join code and shows a runnable command for a reachable address", async () => {
    const fetchMock = routeFetch({});

    renderDialog({ serverUrl: "https://loom.example.com" });

    const command = await screen.findByText(/--join-code jc_1/u);
    expect(command.textContent).toContain("--host-id host_new");
    expect(command.textContent).toContain("--server https://loom.example.com");
    expect(command.textContent).toContain("https://loom.example.com/install.sh");
    // The bug this issue fixed: an empty `--server` value.
    expect(command.textContent).not.toMatch(/--server\s+($|\|)/u);

    // It really went out as a POST to the contract route.
    const joinCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes("/hosts/join-codes"),
    );
    expect(joinCall).toBeDefined();
    expect((joinCall![1] as RequestInit).method).toBe("POST");

    // No plugin-registry traffic at all.
    expect(
      fetchMock.mock.calls.some(([url]) => String(url).includes("plugin")),
    ).toBe(false);
  });

  it("does not print a command when the config is empty and the origin is local-only", async () => {
    routeFetch({});

    // jsdom's origin is `http://localhost:3000`, which another machine cannot
    // address. With `serverUrl: ""` there is no reachable address, so the
    // honest answer is the unreachable notice — not a command aimed at
    // localhost. The non-local fallback (empty config + a public origin) is
    // covered directly in `machine-pairing.test.tsx`.
    renderDialog({ serverUrl: "" });

    expect(
      await screen.findByText(/Another machine cannot use this address/u),
    ).toBeDefined();
    expect(screen.queryByText(/--join-code/u)).toBeNull();
  });

  it("explains an unreachable address instead of printing a dead command", async () => {
    routeFetch({});

    renderDialog({ serverUrl: "http://127.0.0.1:38886" });

    expect(
      await screen.findByText(/Another machine cannot use this address/u),
    ).toBeDefined();
    // No command block, and no copy affordance for a command that cannot work.
    expect(screen.queryByText(/--join-code/u)).toBeNull();
    expect(screen.queryByRole("button", { name: "Copy" })).toBeNull();
  });

  it("shows a join-code error with a retry instead of a stale command", async () => {
    routeFetch({
      joinStatus: 500,
      joinCode: { code: "boom", message: "no join codes" },
    });

    renderDialog({ serverUrl: "https://loom.example.com" });

    expect(await screen.findByText(/no join codes/u)).toBeDefined();
    expect(screen.getByRole("button", { name: "Try again" })).toBeDefined();
    expect(screen.queryByText(/--join-code/u)).toBeNull();
  });

  it("reports a host-list failure instead of waiting forever", async () => {
    routeFetch({
      hostsStatus: 503,
      hosts: { code: "unavailable", message: "hosts down" },
    });

    renderDialog({ serverUrl: "https://loom.example.com" });

    // The command still shows: pairing does not depend on the host list.
    await screen.findByText(/--join-code jc_1/u);
    // And the failed check is disclosed rather than spinning indefinitely.
    expect(
      await screen.findByText(/Couldn't check whether the machine connected/u),
    ).toBeDefined();
  });

  it("still shows the command when the host list is empty", async () => {
    routeFetch({ hosts: [] });

    renderDialog({ serverUrl: "https://loom.example.com" });

    await screen.findByText(/--join-code jc_1/u);
    expect(
      screen.getByText("Waiting for the machine to connect…"),
    ).toBeDefined();
  });
});
