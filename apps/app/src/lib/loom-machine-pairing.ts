import { z } from "zod";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The loom-native machine-pairing boundary.
 *
 * bb issued a machine code through the `connect` plugin's `callRpc` surface and
 * read its enablement from the generic plugin registry. loom has no plugin
 * runtime: joining a machine is the server's own join code, a first-class
 * contract route (`hosts.createJoinCode`).
 *
 * ## Why there is no install command here
 *
 * The ported dialog rendered
 * `curl ... ${serverUrl}/install.sh | sh -s -- --join-code ... --host-id ...
 * --server ...`. Nothing in loom serves that:
 *
 * * The server role exposes only `/install/version` and `/install/loom-worker`
 *   (the worker self-update artifact; `crates/server/src/http.rs`); there is no
 *   `/install.sh`, and nothing else serves an install script.
 * * The repository no longer ships an installer at all: a machine is set up by
 *   putting the `loom` binary on it and running
 *   `loom worker --server-url … --join-code … --name …`
 *   (`docs/process-model.md` § Deploying it). That is a two-step operator
 *   action, not a single command this dialog can render.
 *
 * W-593 must not add a server or release route, so a working command cannot be
 * produced from here. Presenting one anyway would be a command that looks
 * runnable and cannot work — worse than saying the capability is not ready. The
 * real, verified installer/join workflow belongs to W-588.
 *
 * So this module keeps exactly the part that is real (minting a join code) and
 * reports the install step as unavailable, with the reason.
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

/**
 * Whether a machine can currently be installed and paired from this UI.
 *
 * `false` until W-588 lands a verified installer that this UI can point at. It
 * is a named constant rather than a literal so the UI and its tests share one
 * answer, and so flipping it is a deliberate change with a reason.
 */
export const LOOM_MACHINE_INSTALL_AVAILABLE = false;

/** Why the install step cannot be offered, quoted in the UI. */
export const LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON =
  "This server does not serve an installer, and its deployment installer requires root on the target machine. Automatic machine setup is not available yet.";

/** Mint a join code through the contract route. */
export async function createLoomJoinCode(
  signal?: AbortSignal,
): Promise<LoomJoinCode> {
  const raw = await loomApiJson("hosts.createJoinCode", {
    json: {},
    signal,
  });
  return createJoinCodeResponseSchema.parse(raw);
}

/**
 * The pairing step, resolved to what can honestly be shown.
 *
 * There is deliberately no `ready`/`command` variant while
 * `LOOM_MACHINE_INSTALL_AVAILABLE` is false: an unusable command cannot be
 * rendered, because it cannot be represented.
 */
export type LoomPairingState =
  | { readonly kind: "unavailable"; readonly reason: string }
  | {
      readonly kind: "code-issued";
      readonly joinCode: LoomJoinCode;
      readonly reason: string;
    };

export function resolveLoomPairingState(
  joinCode: LoomJoinCode,
): LoomPairingState {
  if (!LOOM_MACHINE_INSTALL_AVAILABLE) {
    return {
      kind: "unavailable",
      reason: LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON,
    };
  }
  return {
    kind: "code-issued",
    joinCode,
    reason: LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON,
  };
}
