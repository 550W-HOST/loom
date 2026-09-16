import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { LOOM_NATIVE_ROUTES } from "@/lib/loom-native-routes";
import { createLoomMachineCode } from "@/lib/loom-machine-pairing";

const appRoot = path.resolve(import.meta.dirname, "../..");

function read(relativePath: string): string {
  return fs.readFileSync(path.join(appRoot, relativePath), "utf8");
}

function sourceFiles(directory: string): string[] {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isDirectory()) return sourceFiles(absolutePath);
    return /\.[cm]?[jt]sx?$/u.test(entry.name) &&
      !/\.(?:test|stories)\.[cm]?[jt]sx?$/u.test(entry.name)
      ? [absolutePath]
      : [];
  });
}

describe("loom-native boundaries", () => {
  it("reads liveness from the server's own route, not the bb contract", () => {
    // `/health` and `/api/v1/version` are loom-native: they are deliberately
    // absent from contracts/bb/server-api.json, so the shell must not expect
    // them there.
    const contract = JSON.parse(
      fs.readFileSync(
        path.resolve(appRoot, "../../contracts/bb/server-api.json"),
        "utf8",
      ),
    ) as { routes: { fullPath: string }[] };
    const contractPaths = new Set(contract.routes.map((r) => r.fullPath));

    expect(LOOM_NATIVE_ROUTES.map((route) => route.path)).toEqual([
      "/health",
      "/api/v1/version",
    ]);
    for (const route of LOOM_NATIVE_ROUTES) {
      expect(contractPaths.has(route.path)).toBe(false);
    }
  });

  it("reports machine pairing as unavailable instead of faking a code", async () => {
    // loom has no `connect` plugin. Reporting `unavailable` keeps the dialog
    // honest: it shows the join code and says remote access is not configured,
    // rather than linking to a plugin page that no longer exists.
    await expect(createLoomMachineCode()).resolves.toEqual({
      kind: "unavailable",
    });
  });

  it("does not reach the generic plugin registry from product source", () => {
    const files = sourceFiles(path.join(appRoot, "src"));
    const offenders = files.filter((file) => {
      const source = fs.readFileSync(file, "utf8");
      return (
        source.includes("sdk.plugins.") ||
        source.includes("sdk.skills.") ||
        source.includes("sdk.experimental_desktopBrowsers.") ||
        source.includes("@get-bb/plugin-sdk")
      );
    });
    expect(offenders).toEqual([]);
  });

  it("keeps the pairing dialog off the removed plugin routes", () => {
    const dialog = read("src/components/dialogs/AddMachineDialog.tsx");
    expect(dialog).not.toContain("sdk.plugins");
    expect(dialog).not.toContain("getPluginConfigurationRoutePath");
    expect(dialog).not.toContain("getPluginDetailRoutePath");
    // It mints the join code through the loom-native boundary instead.
    expect(dialog).toContain("createLoomJoinCode");

    const pairing = read("src/lib/loom-machine-pairing.ts");
    expect(pairing).toContain('"hosts.createJoinCode"');
  });

  it("never invents realtime to stand in for HTTP", () => {
    // The shell must load over HTTP. Opening a WebSocket would make
    // loading/empty/error unreachable and belongs to the realtime issue.
    const boundary = read("src/loom/LoomShellBoundary.tsx");
    expect(boundary).not.toContain("wsManager");
    expect(boundary).not.toContain("useWebSocket");
    expect(boundary).not.toContain("setInterval");
  });
});
