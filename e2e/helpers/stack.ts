import { spawn, type ChildProcess } from "node:child_process";
import {
  chmodSync,
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

/**
 * The stack the suite runs against: one `loom server` process, one `loom
 * worker` process, one ACP stub.
 *
 * `loom` carries the UI, so "serve the product app" is not a configuration
 * step: the binary built from this checkout serves the app built from this
 * checkout. The worker is the same file started under its `worker` subcommand
 * and configured entirely through flags, with its provider pointed at a stub so
 * a turn is deterministic and needs no credentials; the harness's own
 * `LOOM_E2E_PROVIDER_CMD` swaps in a real agent when a human wants one.
 *
 * The state file is how the Playwright worker processes learn the URL: setup
 * runs in its own process, and a test file cannot import a value that only
 * exists there.
 */

const here = dirname(fileURLToPath(import.meta.url));
export const repoRoot = resolve(here, "..", "..");
const statePath = process.env.LOOM_E2E_STATE ?? join(tmpdir(), "loom-e2e-stack.json");

/**
 * Where the stack will listen.
 *
 * Derived rather than discovered because Playwright reads its config *before*
 * `globalSetup` runs: the tests' `baseURL` has to be knowable at load time,
 * while the stack is still being started. Setup and configuration therefore
 * agree by construction, not by a file that may not exist yet.
 */
export function stackBaseURL(): string {
  return `http://127.0.0.1:${Number(process.env.LOOM_E2E_PORT ?? 38990)}`;
}

export interface StackState {
  baseURL: string;
  dataDir: string;
  serverDataDir: string;
  daemonDataDir: string;
  provider: string;
  /** Where the daemon's pid lives, so a test can take it away and put it back. */
  daemonPidPath: string;
  serverPidPath: string;
  logs: { server: string; daemon: string };
}

const children: ChildProcess[] = [];

export function stackState(): StackState {
  return JSON.parse(readFileSync(statePath, "utf8")) as StackState;
}

/**
 * The ACP stub: the handshake, a prompt, and a permission question when the
 * prompt asks for one.
 *
 * The permission branch is what makes the blocked-turn contract observable in
 * a browser: the agent stops mid-turn until a human answers, which is a state
 * no unit test can put on screen. The decision it received is written next to
 * the script, so a test can assert what the agent was *told* and not merely
 * that a banner disappeared.
 *
 * The session and tool-call ids carry this process's pid. ACP ids are unique
 * within a session, and the daemon's pending-request registry is keyed on the
 * pair — a stub that reused one pair for every run would let a resolution
 * replayed after a daemon restart answer a *later* run's question before a
 * client ever saw it. That is a property of the stub, not of the product, and
 * the suite must not manufacture it.
 */
function writeStub(path: string): void {
  // `String.raw`: the script's own escapes (`\(`, `\1`, `\\n` for printf) are
  // the shell's, not JavaScript's.
  writeFileSync(
    path,
    String.raw`#!/bin/sh
session_id="stub-session-$$"
tool_call_id="call-$$"
request_id="perm-$$"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id"
      ;;
    session/new)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$session_id"
      ;;
    session/load)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    session/prompt)
      case "$line" in
        *permission*)
          printf '{"jsonrpc":"2.0","id":"%s","method":"session/request_permission","params":{"sessionId":"%s","toolCall":{"toolCallId":"%s","title":"Run rm -rf /","kind":"execute","status":"pending"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}\n' "$request_id" "$session_id" "$tool_call_id"
          while IFS= read -r answer; do
            case "$answer" in
              *"$request_id"*)
                printf '%s' "$answer" > "$0.decision"
                break
                ;;
            esac
          done
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"decision received"}}}}\n' "$session_id"
          ;;
        *)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"stub reply"}}}}\n' "$session_id"
          ;;
      esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
`,
  );
  chmodSync(path, 0o755);
}

/**
 * Starts one role, detached, with its output in a log file.
 *
 * Detached because the lifecycle of these processes is the *run's*, not the
 * process that happened to spawn them: Playwright's workers come and go, and a
 * daemon that a test restarted must still be there for the next test file. The
 * logs go to a file for the same reason — a pipe would be closed when its
 * spawner exits, and a daemon writing to a closed pipe is a daemon that dies
 * in the middle of the suite.
 */
function start(
  name: string,
  command: string,
  args: string[],
  env: Record<string, string>,
  logPath: string,
): ChildProcess {
  const log = openSync(logPath, "a");
  const child = spawn(command, args, {
    cwd: repoRoot,
    env: { ...process.env, ...env },
    stdio: ["ignore", log, log],
    detached: true,
  });
  closeSync(log);
  child.unref();
  children.push(child);
  return child;
}

/**
 * A port that is already taken belongs to a run that was killed before its
 * teardown, and silently adopting its stack would make this run's setup lie
 * about what it started.
 */
async function refuseBusyPort(port: number): Promise<void> {
  try {
    await fetch(`http://127.0.0.1:${port}/health`);
  } catch {
    return;
  }
  throw new Error(
    `something is already listening on ${port}: stop the leftover stack or set LOOM_E2E_PORT`,
  );
}

async function waitFor(check: () => Promise<boolean>, what: string, seconds = 60): Promise<void> {
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    try {
      if (await check()) return;
    } catch {
      // Not up yet.
    }
    await sleep(200);
  }
  throw new Error(`the stack did not become ready: ${what}`);
}

/**
 * Starts everything the suite drives.
 *
 * The binary and the bundle are preconditions, not build steps: the app is
 * compiled into the server, so a missing one would silently test a stale
 * client, and a suite that builds on demand is a suite whose first run is a
 * different run.
 */
export async function startStack(): Promise<StackState> {
  const loom = join(repoRoot, "target", "debug", "loom");
  if (!existsSync(loom)) {
    throw new Error(`missing ${loom}: run \`cargo build -p loom\` first`);
  }
  if (!existsSync(join(repoRoot, "apps", "app", "dist", "index.html"))) {
    throw new Error(
      "missing apps/app/dist: the app is compiled into the binary, so run " +
        "`pnpm --filter @bb/app run build` and then `cargo build -p loom`",
    );
  }

  const root = mkdtempSync(join(tmpdir(), "loom-e2e-"));
  const serverDataDir = join(root, "server");
  const daemonDataDir = join(root, "worker");
  mkdirSync(join(root, "workspace"), { recursive: true });

  const stub = join(root, "acp-stub.sh");
  writeStub(stub);
  const provider = process.env.LOOM_E2E_PROVIDER_CMD ?? stub;

  const baseURL = stackBaseURL();
  const port = Number(new URL(baseURL).port);

  await refuseBusyPort(port);
  const server = start(
    "server",
    loom,
    ["server", "--bind", `127.0.0.1:${port}`, "--data-dir", serverDataDir],
    {},
    join(root, "server.log"),
  );
  const serverPidPath = join(root, "server.pid");
  if (server.pid !== undefined) {
    writeFileSync(serverPidPath, String(server.pid));
  }
  const state: StackState = {
    baseURL,
    dataDir: root,
    serverDataDir,
    daemonDataDir,
    provider,
    daemonPidPath: join(root, "daemon.pid"),
    serverPidPath,
    logs: { server: join(root, "server.log"), daemon: join(root, "daemon.log") },
  };
  startDaemon(state, loom);

  await waitFor(async () => (await fetch(`${baseURL}/health`)).ok, "the server answered /health");
  await waitFor(async () => {
    const hosts = (await (await fetch(`${baseURL}/api/v1/hosts`)).json()) as { status: string }[];
    return hosts.some((host) => host.status === "connected");
  }, "the daemon enrolled");

  writeFileSync(statePath, JSON.stringify(state, null, 2));
  return state;
}

/**
 * Starts (or restarts) the worker process.
 *
 * A test that removes the worker to watch the product notice the machine is
 * gone has to be able to put it back; the pid file is how the teardown then
 * reaps whichever worker is alive at the end, not the one setup happened to
 * spawn.
 */
export function startDaemon(
  state: StackState,
  binary = join(repoRoot, "target", "debug", "loom"),
): ChildProcess {
  const child = start(
    "worker",
    binary,
    [
      "worker",
      "--server-url",
      state.baseURL,
      "--name",
      "e2e",
      // The same choice every deployment makes: without a state file the worker
      // enrolls as a new machine on every restart, and the machine list is
      // where that accumulates.
      "--state",
      join(state.dataDir, "daemon-host-id"),
      // A question nobody answers must not hold the machine hostage for the
      // rest of the run: the broker blocks the agent's dispatch loop until the
      // permission timeout passes.
      "--permission-timeout-ms",
      "20000",
      "--data-dir",
      state.daemonDataDir,
      // A managed environment's workspace is otherwise created under the
      // developer's home directory, and a test run has no business leaving
      // directories there.
      "--workspace-root",
      join(state.dataDir, "workspace"),
      // Only point at a provider when the harness configured one; otherwise the
      // worker runs whatever the control plane dispatched.
      ...(state.provider
        ? ["--provider-cmd", state.provider, "--provider-args", ""]
        : []),
    ],
    {},
    state.logs.daemon,
  );
  if (child.pid !== undefined) {
    writeFileSync(state.daemonPidPath, String(child.pid));
  }
  return child;
}

/** Puts the daemon back if a test took it away and did not manage to return it. */
export function ensureDaemon(state: StackState): void {
  try {
    process.kill(Number(readFileSync(state.daemonPidPath, "utf8")), 0);
    return;
  } catch {
    // Not running: start one.
  }
  startDaemon(state);
}

export function stopStack(): void {
  children.length = 0;
  try {
    const { daemonPidPath, serverPidPath } = stackState();
    for (const path of [daemonPidPath, serverPidPath]) {
      try {
        process.kill(Number(readFileSync(path, "utf8")), "SIGTERM");
      } catch {
        // A test already stopped it, or setup never got this far.
      }
    }
  } catch {
    // Setup never got as far as writing the state file.
  }
  if (process.env.LOOM_E2E_KEEP === "1") return;
  try {
    rmSync(stackState().dataDir, { recursive: true, force: true });
  } catch {
    // The teardown runs even when setup failed; nothing to remove then.
  }
}
