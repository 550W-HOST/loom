import { z } from "zod";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The loom-native machine-pairing boundary.
 *
 * bb issued a machine code through the `connect` plugin's `callRpc` surface
 * and read its enablement from the generic plugin registry. loom has no plugin
 * runtime: a machine joins by presenting the server's own join code, which is
 * a first-class contract route (`hosts.createJoinCode`).
 *
 * The capability is therefore reported honestly as *not present* rather than
 * faked. A caller that used to render a plugin link now renders the join code
 * alone, and the UI must not claim a remote-access setup exists when it does
 * not.
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

export type LoomMachineCodeResult =
  | { readonly kind: "unavailable" }
  | { readonly kind: "issued"; readonly code: LoomMachineCode };

export interface LoomMachineCode {
  readonly code: string;
  readonly expiresAt: number;
  readonly serverUrl: string;
}

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
