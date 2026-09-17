#!/usr/bin/env node
/**
 * Writes the Brotli precompressed siblings the bundle budget measures.
 *
 * The budget check reads `<chunk>.br` next to every emitted chunk and treats a
 * missing one as an error rather than guessing a size: a build that skipped
 * this step would otherwise weigh zero against the compressed budget and hide
 * real growth. The server serves the raw files, and a reverse proxy (or the
 * container's host) can serve these with `Content-Encoding: br`; what they are
 * required for here is the measurement.
 *
 * Files under 1 KiB are skipped, matching `check-bundle-budget.mjs`: below that
 * the framing overhead of a Brotli stream loses to raw bytes, and the checker
 * counts those raw bytes instead.
 *
 *   node scripts/precompress-app-dist.mjs [--check]
 *
 * `--check` writes nothing and exits non-zero when a `.br` sibling is missing,
 * which is what a packaging step wants to assert.
 */

import { brotliCompressSync, constants } from "node:zlib";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const distDir = path.join(appDir, "dist");
const MIN_PRECOMPRESS_BYTES = 1024;
const checkOnly = process.argv.includes("--check");

if (!fs.existsSync(distDir)) {
  console.error(`missing ${path.relative(appDir, distDir)} — run the app build first`);
  process.exit(1);
}

/** Every emitted file that deserves a Brotli sibling, deepest first. */
function candidates(dir) {
  const found = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      found.push(...candidates(full));
      continue;
    }
    // A `.br`/`.gz` file is already compressed, and so is any binary format
    // whose payload does not shrink; the budget only ever measures JS/CSS
    // chunks, and the rest of the bundle is served as it is.
    if (/\.(br|gz)$/u.test(entry.name)) continue;
    if (fs.statSync(full).size < MIN_PRECOMPRESS_BYTES) continue;
    found.push(full);
  }
  return found;
}

const missing = [];
let written = 0;
for (const file of candidates(distDir)) {
  const target = `${file}.br`;
  if (fs.existsSync(target)) continue;
  if (checkOnly) {
    missing.push(path.relative(distDir, file));
    continue;
  }
  fs.writeFileSync(
    target,
    brotliCompressSync(fs.readFileSync(file), {
      params: {
        [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
        [constants.BROTLI_PARAM_SIZE_HINT]: fs.statSync(file).size,
      },
    }),
  );
  written += 1;
}

if (checkOnly && missing.length > 0) {
  console.error(`missing Brotli siblings for: ${missing.join(", ")}`);
  process.exit(1);
}

console.log(
  checkOnly
    ? `precompressed assets OK (${candidates(distDir).length} files)`
    : `precompressed ${written} asset(s) under ${path.relative(appDir, distDir)}`,
);
