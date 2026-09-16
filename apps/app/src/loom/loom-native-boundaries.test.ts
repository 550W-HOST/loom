import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { LOOM_NATIVE_ROUTES } from "@/lib/loom-native-routes";
import {
  LOOM_MACHINE_INSTALL_AVAILABLE,
  resolveLoomPairingState,
} from "@/lib/loom-machine-pairing";

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

  it("reports the install step as unavailable instead of faking a command", () => {
    // loom serves no `/install.sh` (only `/install/version` and
    // `/install/loom-daemon`) and `deploy/install.sh` takes a `<server-key>`,
    // not `--join-code`. Saying so is the only honest answer this phase allows.
    expect(LOOM_MACHINE_INSTALL_AVAILABLE).toBe(false);
    const state = resolveLoomPairingState({
      joinCode: "jc",
      hostId: "h",
      expiresAt: 1,
    });
    expect(state.kind).toBe("unavailable");
    expect(JSON.stringify(state)).not.toContain("/install.sh");
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
    // The installer is mentioned only in the comment that explains why no
    // command is produced. Any *code* that builds one would need these.
    const pairingCode = pairing
      .split("\n")
      .filter((line) => !line.trimStart().startsWith("*") && !line.trimStart().startsWith("//"))
      .join("\n");
    expect(pairingCode).not.toContain("install.sh");
    expect(pairingCode).not.toContain("sh -s --");
    expect(pairingCode).not.toContain("curl ");
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
