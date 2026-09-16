import { z } from "zod";
import { loomApiJson } from "@/lib/loom-http";
import { isLocalOnlyUrl } from "@/lib/loopback-hostname";

/**
 * The loom-native machine-pairing boundary.
 *
 * bb issued a machine code through the `connect` plugin's `callRpc` surface and
 * read its enablement from the generic plugin registry. loom has no plugin
 * runtime: a machine joins by presenting the server's own join code, which is a
 * first-class contract route (`hosts.createJoinCode`).
 *
 * Two things this module refuses to do, both of which the previous version got
 * wrong:
 *
 * 1. It never emits a pairing command with an empty or unusable `--server`.
 *    loom's `system.config` currently answers `serverUrl: ""`, so taking that
 *    value at face value produced `... --server ` — a command that looks
 *    runnable and cannot work. The reachable address is derived from the origin
 *    the app is actually served from, and an address that only resolves on this
 *    machine is reported as such instead of being printed.
 * 2. It does not pretend a machine code exists. loom has no `connect` plugin,
 *    so that capability is reported as `unavailable` rather than faked.
 */

export interface LoomJoinCode {
  readonly joinCode: string;
  readonly hostId: string;
  readonly expiresAt: number;
}

const createJoinCodeResponseSchema = z.object({
  joinCode: z.string().min(1),
  hostId: z.string().min(1),
  expiresAt: z.number(),
});

export interface LoomMachineCode {
  readonly code: string;
  readonly expiresAt: number;
  readonly serverUrl: string;
}

export type LoomMachineCodeResult =
  | { readonly kind: "unavailable" }
  | { readonly kind: "issued"; readonly code: LoomMachineCode };

/**
 * How a pairing command resolved.
 *
 * `unreachable` is not an error: it is the honest answer for a server the other
 * machine cannot address, and the caller renders it as an explanation rather
 * than a command.
 */
export type LoomPairingCommand =
  | { readonly kind: "ready"; readonly command: string; readonly serverUrl: string }
  | { readonly kind: "unreachable"; readonly serverUrl: string }
  | { readonly kind: "no-server-address" };

/** Mint a join code through the contract route. */
export async function createLoomJoinCode(
  signal?: AbortSignal,
): Promise<LoomJoinCode> {
  const raw = await loomApiJson<unknown>("hosts.createJoinCode", {
    json: {},
    signal,
  });
  return createJoinCodeResponseSchema.parse(raw);
}

/**
 * loom has no `connect` plugin, so there is no machine code to issue. Returning
 * `unavailable` (rather than throwing, or returning a fake code) lets the
 * dialog show the join-code command and say plainly that remote access is not
 * configured.
 */
export function createLoomMachineCode(): Promise<LoomMachineCodeResult> {
  return Promise.resolve({ kind: "unavailable" });
}

function isAbsoluteHttpUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

/**
 * The address another machine should use to reach this server.
 *
 * `configuredServerUrl` (loom's `system.config.serverUrl`) is used only when it
 * is a real absolute URL. Otherwise the app's own origin wins, because the app
 * is served by the same server it talks to: that origin is known to be
 * reachable and known not to be a placeholder.
 */
export function resolvePairingServerUrl(
  configuredServerUrl: string | null | undefined,
  origin: string | null | undefined = typeof window === "undefined"
    ? null
    : window.location?.origin,
): string | null {
  if (
    typeof configuredServerUrl === "string" &&
    isAbsoluteHttpUrl(configuredServerUrl)
  ) {
    return configuredServerUrl.replace(/\/+$/u, "");
  }
  if (typeof origin === "string" && isAbsoluteHttpUrl(origin)) {
    return origin.replace(/\/+$/u, "");
  }
  return null;
}

/** True when this address cannot be used from a different machine. */
export function isPairingServerUrlUnreachable(serverUrl: string): boolean {
  return isLocalOnlyUrl(serverUrl);
}

/**
 * Build the command that joins a machine to this server.
 *
 * Returns a state rather than a string so a caller cannot accidentally render
 * an unusable command: there is no `serverUrl` value that yields a command with
 * an empty `--server`.
 */
export function buildLoomPairingCommand(args: {
  joinCode: string;
  hostId: string;
  configuredServerUrl?: string | null;
  origin?: string | null;
}): LoomPairingCommand {
  const serverUrl = resolvePairingServerUrl(
    args.configuredServerUrl,
    args.origin,
  );
  if (serverUrl === null) {
    return { kind: "no-server-address" };
  }
  if (isPairingServerUrlUnreachable(serverUrl)) {
    return { kind: "unreachable", serverUrl };
  }
  return {
    kind: "ready",
    serverUrl,
    command:
      `curl -fL --progress-meter --connect-timeout 10 --max-time 60 --retry 2 ` +
      `${serverUrl}/install.sh | sh -s -- --join-code ${args.joinCode} ` +
      `--host-id ${args.hostId} --server ${serverUrl}`,
  };
}
