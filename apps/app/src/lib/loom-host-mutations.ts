import { loomApiJson } from "@/lib/loom-http";

/**
 * The host mutations the product app issues over loom.
 *
 * `hosts.delete` was still the fail-closed browser SDK stub, so removing a
 * machine from Settings threw `BrowserSdkUnavailableError` instead of reaching
 * the server. The contract route and its server handler already exist (the
 * handler answers `{ "ok": true }` and refuses the primary host or a referenced
 * one with a typed 409), so this is the app half only.
 *
 * It follows the loom-native writer pattern (`loom-ui-preferences.ts`,
 * `loom-thread-storage.ts`): a contract DELETE with no body, wired into
 * `src/lib/sdk.ts`.
 */

export interface LoomDeleteHostArgs {
  hostId: string;
  signal?: AbortSignal;
}

/**
 * Delete a host through the contract route.
 *
 * The contract response body is `null`, but the server answers `{ ok: true }`
 * on success; returning the parsed body keeps the SDK's `{ ok: true }` result
 * honest rather than inventing it.
 */
export function loomDeleteHost(args: LoomDeleteHostArgs): Promise<{ ok: true }> {
  return loomApiJson("hosts.delete", {
    param: { id: args.hostId },
    signal: args.signal,
  });
}
