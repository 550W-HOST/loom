import { afterEach, describe, expect, it, vi } from "vitest";
import type { AppSettings } from "@bb/domain";
import { sdk } from "@/lib/sdk";
import { loomUpdateGeneralSettings } from "@/lib/loom-settings-mutations";
import { LoomHttpError, resolveLoomApiMethod } from "@/lib/loom-http";

const settings: AppSettings = {
  defaultProviderId: "claude",
  managedBranchPrefix: "bb/",
  providerOrder: ["claude"],
  showDiagnosticEvents: false,
  showKeyboardHints: true,
  steerActiveThreadOnEnter: true,
  streamerMode: false,
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom general settings mutation", () => {
  it("issues PUT against the contract path with the settings as its JSON body", async () => {
    const fetchMock = vi.fn(async () => jsonResponse(settings));
    vi.stubGlobal("fetch", fetchMock);

    await expect(loomUpdateGeneralSettings(settings)).resolves.toEqual(settings);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(resolveLoomApiMethod("system.generalSettings")).toBe("PUT");
    expect(init.method).toBe("PUT");
    expect(url.pathname).toBe("/api/v1/settings/general");
    expect(url.search).toBe("");
    expect(JSON.parse(String(init.body))).toEqual(settings);
  });

  it("returns the server's stored settings rather than echoing the request", async () => {
    const stored = {
      ...settings,
      providerOrder: ["codex", "claude"],
      showUnhandledProviderEvents: true,
    };
    vi.stubGlobal("fetch", vi.fn(async () => jsonResponse(stored)));

    await expect(loomUpdateGeneralSettings(settings)).resolves.toEqual(stored);
  });

  it("surfaces the server's typed refusal instead of a fake success", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse(
          {
            code: "invalid_managed_branch_prefix",
            message: "managedBranchPrefix is not a valid git branch prefix",
          },
          422,
        ),
      ),
    );

    await expect(loomUpdateGeneralSettings(settings)).rejects.toMatchObject({
      status: 422,
      code: "invalid_managed_branch_prefix",
    });
    await expect(loomUpdateGeneralSettings(settings)).rejects.toBeInstanceOf(
      LoomHttpError,
    );
  });

  it("wires the update into the browser SDK surface", async () => {
    expect(sdk.system.updateGeneralSettings).toBe(loomUpdateGeneralSettings);

    const fetchMock = vi.fn(async () => jsonResponse(settings));
    vi.stubGlobal("fetch", fetchMock);

    await expect(sdk.system.updateGeneralSettings(settings)).resolves.toEqual(
      settings,
    );
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(url.pathname).toBe("/api/v1/settings/general");
    expect(init.method).toBe("PUT");
  });
});
