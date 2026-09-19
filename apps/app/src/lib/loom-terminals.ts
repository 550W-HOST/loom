import type {
  CreateTerminalRequest,
  TerminalListQuery,
  TerminalListResponse,
  TerminalSession,
  UpdateTerminalRequest,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The terminal operations the product app issues over loom.
 *
 * The `terminals` area was still the fail-closed browser SDK stub, so opening a
 * terminal, listing a thread's terminals, renaming one and closing it threw
 * `BrowserSdkUnavailableError` instead of reaching the server. The contract
 * routes and their server handlers already exist, so this is the app half only.
 *
 * It follows the loom-native reader/writer pattern (`loom-host-readers.ts`,
 * `loom-thread-storage.ts`). The one piece of real translation is the scope: a
 * terminal is addressed either by thread, by environment or by host path, and
 * the list route takes that as a query while create takes it as a `target`.
 */

export type LoomTerminalListScope =
  | { kind: "thread"; threadId: string }
  | { environmentId: string; kind: "environment" }
  | { cwd?: string; hostId: string; kind: "host_path" };

export interface LoomListTerminalsArgs {
  scope: LoomTerminalListScope;
  signal?: AbortSignal;
}

/**
 * The contract's list query is one flat, strict object that accepts exactly one
 * scope selector, so the chosen branch is what is sent rather than a `kind`
 * field the server has no schema for.
 */
function terminalListQuery(scope: LoomTerminalListScope): TerminalListQuery {
  switch (scope.kind) {
    case "thread":
      return { threadId: scope.threadId };
    case "environment":
      return { environmentId: scope.environmentId };
    case "host_path":
      return {
        hostId: scope.hostId,
        ...(scope.cwd === undefined ? {} : { cwd: scope.cwd }),
      };
  }
}

export function loomListTerminals(
  args: LoomListTerminalsArgs,
): Promise<TerminalListResponse> {
  return loomApiJson("terminals.list", {
    query: terminalListQuery(args.scope),
    signal: args.signal,
  });
}

/**
 * The SDK's create argument carries the scope under `scope`; the contract calls
 * the same object `target`, so the wrapper renames it on the way out.
 */
export interface LoomCreateTerminalArgs
  extends Omit<CreateTerminalRequest, "target"> {
  scope: CreateTerminalRequest["target"];
}

export function loomCreateTerminal(
  args: LoomCreateTerminalArgs,
): Promise<TerminalSession> {
  const { scope, ...json } = args;
  return loomApiJson("terminals.create", {
    json: { ...json, target: scope },
  });
}

export interface LoomRenameTerminalArgs extends UpdateTerminalRequest {
  terminalId: string;
}

export function loomRenameTerminal(
  args: LoomRenameTerminalArgs,
): Promise<TerminalSession> {
  return loomApiJson("terminals.update", {
    param: { terminalId: args.terminalId },
    json: { title: args.title },
  });
}

export interface LoomCloseTerminalArgs {
  mode: "force" | "if-clean";
  terminalId: string;
}

/**
 * The contract requires the close `reason` to be the literal `"user"`; a
 * product-shell close is a user action, not a server-driven teardown.
 */
export function loomCloseTerminal(
  args: LoomCloseTerminalArgs,
): Promise<TerminalSession> {
  return loomApiJson("terminals.close", {
    param: { terminalId: args.terminalId },
    json: { mode: args.mode, reason: "user" },
  });
}
