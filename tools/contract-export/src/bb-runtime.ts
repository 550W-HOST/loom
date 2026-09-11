import {
  cpSync,
  existsSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * The packages that make up the runtime closure of bb's contract surface.
 *
 * Only these are copied into the scratch tree; nothing under `apps/` is
 * touched, and the bb checkout itself stays read-only.
 */
export const CONTRACT_PACKAGES = [
  "domain",
  "hono-typed-routes",
  "host-daemon-contract",
  "process-utils",
  "provider-bridge-protocol",
  "server-contract",
] as const;

/** Runtime dependencies the contract packages import at module load. */
const RUNTIME_DEPS = ["zod", "hono", "cross-spawn", "cross-spawn-async"] as const;

const TOOL_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export interface BbRuntime {
  /** Directory that holds `node_modules/@bb/*` plus the bundled deps. */
  readonly root: string;
  /** Absolute path of the bb checkout whose contract was exported. */
  readonly bbSrc: string;
}

function writePackageJson(dir: string, name: string): void {
  const manifest = {
    name,
    version: "0.0.1",
    type: "module",
    exports: {
      ".": { types: "./src/index.ts", default: "./src/index.ts" },
      "./*": { types: "./src/*.ts", default: "./src/*.ts" },
    },
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(manifest, null, 2));
}

/**
 * Copies the contract packages into a scratch module tree so that
 * `@bb/*` specifiers resolve without an install step inside the bb checkout.
 *
 * bb's packages rely on pnpm workspace symlinks that do not exist in a plain
 * `git clone`; copying them here keeps the export reproducible from a checkout
 * with no dependencies installed, and never writes into bb.
 */
export function materializeBbRuntime(bbSrc: string, workDir: string): BbRuntime {
  const root = resolve(workDir);
  const modules = join(root, "node_modules");
  rmSync(root, { recursive: true, force: true });
  mkdirSync(join(modules, "@bb"), { recursive: true });

  for (const pkg of CONTRACT_PACKAGES) {
    const from = join(bbSrc, "packages", pkg, "src");
    if (!existsSync(from)) {
      throw new Error(`bb package source not found: ${from}`);
    }
    const to = join(modules, "@bb", pkg);
    mkdirSync(to, { recursive: true });
    cpSync(from, join(to, "src"), { recursive: true });
    writePackageJson(to, `@bb/${pkg}`);
  }

  for (const dep of RUNTIME_DEPS) {
    const from = join(TOOL_ROOT, "node_modules", dep);
    if (!existsSync(from)) {
      // cross-spawn-async is optional; skip only that one.
      if (dep === "cross-spawn-async") continue;
      throw new Error(
        `missing runtime dependency ${dep}; run \`bun install\` in ${TOOL_ROOT}`,
      );
    }
    cpSync(from, join(modules, dep), { recursive: true });
  }

  for (const bin of ["cross-spawn"]) {
    const from = join(TOOL_ROOT, "node_modules", ".bin", bin);
    if (existsSync(from)) {
      mkdirSync(join(modules, ".bin"), { recursive: true });
      cpSync(from, join(modules, ".bin", bin));
    }
  }

  return { root, bbSrc: resolve(bbSrc) };
}

/** Best-effort source revision stamp for the manifest. */
export function bbSourceCommit(bbSrc: string): string | null {
  try {
    const proc = Bun.spawnSync(["git", "-C", bbSrc, "rev-parse", "HEAD"]);
    if (proc.exitCode !== 0) return null;
    const sha = proc.stdout.toString().trim();
    return /^[0-9a-f]{40}$/.test(sha) ? sha : null;
  } catch {
    return null;
  }
}
