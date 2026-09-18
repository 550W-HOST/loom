#!/usr/bin/env node
/**
 * Boots the stack the browser acceptance run drives: a real server role, a
 * real daemon role with an ACP stub provider, and the product app compiled into
 * the binary.
 *
 * The browser talks to **the server's origin**, serving the same bundle a
 * release ships (`LOOM_UI_DIR`) — not a dev server, so the acceptance run
 * exercises the production shape. Build the bundle first:
 * `pnpm --filter @bb/app run build`. Nothing is mocked.
 *
 * Why a script and not a test: the repo has no browser runner, and this is the
 * acceptance evidence W-599.5 asks for — a human (or an agent driving a
 * browser) walks the Automations view against real processes. What is checked
 * automatically is the same flow at the API level (`crates/server/tests/`,
 * `crates/daemon/tests/`); what this adds is the view.
 *
 *   node scripts/e2e/automations-browser-setup.mjs [--keep] [--reuse]
 *
 * It prints one JSON line with the app URL, the API URL and the data
 * directories, then stays in the foreground until SIGINT/SIGTERM, at which
 * point every child process is stopped and (unless `--keep`) the data
 * directories are removed. `--keep` is what a reviewer wants when a run fails
 * and the server's state has to be read afterwards.
 *
 * `--reuse` boots onto the previous run's data directories
 * (`LOOM_E2E_DATA_DIR`, set by `--keep` runs through the printed `dataDir`), so
 * the restart half of the acceptance run — automations and their history
 * surviving a server restart — is the same command twice.
 */

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const keep = process.argv.includes("--keep");
const reuseRoot = process.env.LOOM_E2E_DATA_DIR;

const serverPort = Number(process.env.LOOM_E2E_SERVER_PORT ?? 38941);
const apiOrigin = `http://127.0.0.1:${serverPort}`;
// Same origin for the view and the API: the server serves the bundle.
const appUrl = `${apiOrigin}/automations`;
const uiDir = process.env.LOOM_E2E_UI_DIR ?? join(repoRoot, "apps", "app", "dist");

const root = reuseRoot ?? mkdtempSync(join(tmpdir(), "loom-e2e-"));
const serverDataDir = join(root, "server");
const daemonDataDir = join(root, "daemon");

/**
 * The ACP stub: a JSON-RPC agent over stdio that answers the handshake and one
 * prompt, the same shell the Rust end-to-end tests write, so the daemon's real
 * ACP client is what the browser run exercises.
 */
const stubPath = join(root, "acp-stub.sh");
writeFileSync(
  stubPath,
  `#!/bin/sh
session_id=stub-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\\([^,]*\\),"method":.*/\\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\\([^"]*\\)".*/\\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\\n' "$id"
      ;;
    session/new)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\\n' "$id" "$session_id"
      ;;
    session/load)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\\n' "$id"
      ;;
    session/prompt)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"automation stub reply"}}}}\\n' "$session_id"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\\n' "$id"
      ;;
  esac
done
`,
);
chmodSync(stubPath, 0o755);

const children = [];

function start(name, command, args, env) {
  const child = spawn(command, args, {
    cwd: repoRoot,
    env: { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => process.stderr.write(`[${name}] ${chunk}`));
  child.stderr.on("data", (chunk) => process.stderr.write(`[${name}] ${chunk}`));
  child.on("exit", (code, signal) => {
    if (!shuttingDown) {
      process.stderr.write(`[${name}] exited (code=${code} signal=${signal})\n`);
    }
  });
  children.push(child);
  return child;
}

let shuttingDown = false;

function shutdown() {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const child of children) {
    child.kill("SIGTERM");
  }
  if (!keep && reuseRoot === undefined) {
    rmSync(root, { recursive: true, force: true });
  } else {
    process.stderr.write(`[e2e] keeping data directories under ${root}\n`);
  }
  process.exit(0);
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

for (const binary of ["loom"]) {
  const path = join(repoRoot, "target", "debug", binary);
  if (!existsSync(path)) {
    process.stderr.write(`[e2e] ${path} is missing: run \`cargo build -p ${binary}\` first\n`);
    process.exit(1);
  }
}
if (!existsSync(join(uiDir, "index.html"))) {
  process.stderr.write(
    `[e2e] no UI bundle at ${uiDir}: run \`pnpm --filter @bb/app run build\` first\n`,
  );
  process.exit(1);
}

start("server", join(repoRoot, "target", "debug", "loom"), ["server"], {
  LOOM_BIND: `127.0.0.1:${serverPort}`,
  LOOM_DATA_DIR: serverDataDir,
  LOOM_UI_DIR: uiDir,
});

start(
  "daemon",
  join(repoRoot, "target", "debug", "loom"),
  ["daemon", "--server-url", apiOrigin, "--name", "e2e"],
  {
    LOOM_DATA_DIR: daemonDataDir,
    LOOM_PROVIDER_CMD: stubPath,
    LOOM_PROVIDER_ARGS: "",
  },
);

// Readiness: the app answers, the API answers, and a host is enrolled (the
// last is what makes a script or agent run actually execute rather than fail
// with "no connected machine").
const deadline = Date.now() + 120_000;
let ready = false;
while (Date.now() < deadline) {
  try {
    const [app, hosts] = await Promise.all([
      fetch(appUrl),
      fetch(`${apiOrigin}/api/v1/hosts`),
    ]);
    if (app.ok && hosts.ok) {
      const enrolled = await hosts.json();
      if (Array.isArray(enrolled) && enrolled.some((host) => host.status === "connected")) {
        ready = true;
        break;
      }
    }
  } catch {
    // Not up yet.
  }
  await new Promise((done) => setTimeout(done, 500));
}

if (!ready) {
  process.stderr.write("[e2e] the stack did not become ready in time\n");
  shutdown();
}

process.stdout.write(
  `${JSON.stringify({
    appUrl,
    apiOrigin,
    dataDir: root,
    serverDataDir,
    daemonDataDir,
    acpStub: stubPath,
  })}\n`,
);
