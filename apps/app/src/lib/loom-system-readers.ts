import type { ProviderInfo } from "@bb/domain";
import type {
  SystemExecutionOptionsQuery,
  SystemExecutionOptionsResponse,
  SystemProvidersQuery,
  SystemProviderStatesResponse,
  SystemVersionResponse,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The ported readers for the system surfaces the shell and composer need, on
 * the same-origin typed transport.
 *
 * The generic `@bb/sdk/browser` area methods are still compile-only stubs (every
 * call rejects with `BrowserSdkUnavailableError`), so a query that used one
 * appeared as a permanent per-surface failure — the composer's model picker
 * showed "Failed to load models" against a server that answered 200. These
 * helpers call the contract route directly and keep the transport honest: a
 * failure here is a real, reportable failure.
 */

/** The `system.executionOptions` query, minus the `signal` argument. */
export interface LoomExecutionOptionsArgs extends SystemExecutionOptionsQuery {
  signal?: AbortSignal;
}

export function readSystemExecutionOptions(
  args: LoomExecutionOptionsArgs,
): Promise<SystemExecutionOptionsResponse> {
  const { signal, ...query } = args;
  return loomApiJson(
    "system.executionOptions",
    { query, signal },
  );
}

export interface LoomSystemProvidersArgs extends SystemProvidersQuery {
  signal?: AbortSignal;
}

/**
 * `system.providers` answers a bare array of `SystemProviderInfo`; the SDK area
 * types it as `ProviderInfo[]`, and `@bb/domain` owns that schema.
 */
export function readSystemProviders(
  args: LoomSystemProvidersArgs = {},
): Promise<ReadonlyArray<ProviderInfo>> {
  const { signal, ...query } = args;
  return loomApiJson("system.providers", {
    query,
    signal,
  });
}

export function readSystemProviderStates(
  args: LoomSystemProvidersArgs,
): Promise<SystemProviderStatesResponse> {
  const { signal, ...query } = args;
  return loomApiJson(
    "system.providerStates",
    { query, signal },
  );
}

export interface LoomSystemVersionArgs {
  force?: boolean;
  signal?: AbortSignal;
}

export function readSystemVersion(
  args: LoomSystemVersionArgs = {},
): Promise<SystemVersionResponse> {
  // The contract's query is the string literal `"true" | "false"`, not a
  // boolean; omitting it entirely is how "no force" is expressed.
  return loomApiJson("system.version", {
    query: args.force === undefined ? {} : { force: String(args.force) as "true" | "false" },
    signal: args.signal,
  });
}
