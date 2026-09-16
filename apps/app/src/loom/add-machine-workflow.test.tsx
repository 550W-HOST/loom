import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddMachineDialog } from "@/components/dialogs/AddMachineDialog";
import type { Host } from "@bb/domain";

/**
 * Component-level coverage for the Add Machine workflow.
 *
 * The ported test for this dialog cannot run in this workspace (it mocks the
 * removed plugin SDK and imports `@bb/test-helpers`), so this is the gate. It
 * asserts what the dialog must *not* do as much as what it does: no install
 * command, no indefinite spinner, no implied live connection.
 */

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function renderDialog(props: {
  serverUrl: string | null;
  onOpenChange?: (open: boolean) => void;
}) {
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
        args.joinCode ?? {
          joinCode: "jc_1",
          hostId: "host_new",
          expiresAt: Date.now() + 900_000,
        },
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

describe("AddMachineDialog fails closed on install", () => {
  it("shows that automatic setup is unavailable, with the reason", async () => {
    routeFetch({});
    renderDialog({ serverUrl: "https://loom.example.com" });

    const notice = await screen.findByTestId("loom-machine-install-unavailable");
    expect(notice.textContent).toContain("isn’t available yet");
    expect(notice.textContent).toMatch(/installer|install/u);
  });

  it("never renders or copies an install command", async () => {
    routeFetch({});
    const { container } = renderDialog({ serverUrl: "https://loom.example.com" });

    await screen.findByTestId("loom-machine-install-unavailable");
    expect(container.textContent).not.toContain("/install.sh");
    expect(container.textContent).not.toContain("curl");
    expect(container.textContent).not.toContain("--join-code");
    expect(container.textContent).not.toContain("--server ");
    // The only copy affordance is for the join code itself, never a command.
    const copyButtons = screen.queryAllByRole("button", { name: /copy/iu });
    for (const button of copyButtons) {
      expect(button.textContent).toMatch(/copy code/iu);
    }
  });

  it("shows the real join code for an operator who already has binaries", async () => {
    routeFetch({});
    renderDialog({ serverUrl: "https://loom.example.com" });

    const block = await screen.findByTestId("loom-machine-join-code");
    expect(block.textContent).toContain("jc_1");
  });

  it("mints the join code with POST and touches no plugin route", async () => {
    const fetchMock = routeFetch({});
    renderDialog({ serverUrl: "https://loom.example.com" });
    await screen.findByTestId("loom-machine-join-code");

    const joinCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes("/hosts/join-codes"),
    );
    expect(joinCall).toBeDefined();
    expect((joinCall![1] as RequestInit).method).toBe("POST");
    expect(
      fetchMock.mock.calls.some(([url]) => String(url).includes("plugin")),
    ).toBe(false);
  });

  it("offers only a manual Refresh, with no indefinite spinner", async () => {
    routeFetch({ hosts: [] });
    renderDialog({ serverUrl: "https://loom.example.com" });

    await screen.findByTestId("loom-machine-join-code");
    // The old copy implied live observation, which cannot be delivered before
    // the realtime work lands.
    expect(document.body.textContent).not.toContain(
      "Waiting for the machine to connect",
    );
    expect(await screen.findByText("Check whether a machine has joined.")).toBeDefined();
    expect(screen.getByRole("button", { name: "Refresh" })).toBeDefined();
  });

  it("reports a host-list failure instead of spinning", async () => {
    routeFetch({
      hostsStatus: 503,
      hosts: { code: "unavailable", message: "hosts down" },
    });
    renderDialog({ serverUrl: "https://loom.example.com" });

    await screen.findByTestId("loom-machine-join-code");
    expect(
      await screen.findByText(/Couldn’t check connected machines/u),
    ).toBeDefined();
  });

  it("shows a retry when the join code cannot be minted", async () => {
    routeFetch({
      joinStatus: 500,
      joinCode: { code: "boom", message: "no join codes" },
    });
    renderDialog({ serverUrl: "https://loom.example.com" });

    expect(await screen.findByText(/no join codes/u)).toBeDefined();
    expect(screen.getByRole("button", { name: "Try again" })).toBeDefined();
    expect(screen.queryByTestId("loom-machine-join-code")).toBeNull();
  });
});
