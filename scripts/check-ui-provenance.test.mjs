import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  assertExactBlobEntry,
  assertExactSourceEntry,
  assertLedgerMatchesDiff,
  computePatchDiff,
  sourceRegistry,
} from "./check-ui-provenance.mjs";

const APP_PATH = "apps/app";
const ISSUE = { issue: "W-606", owner: "test", reason: "provenance checker fixture" };

function fixtureRoots() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  for (const base of [upstream, local]) fs.mkdirSync(path.join(base, APP_PATH), { recursive: true });

  writeFile(upstream, "modify.txt", "upstream\n");
  writeFile(upstream, "delete.txt", "delete\n");
  writeFile(upstream, "rename-old.txt", "rename\n");
  writeFile(upstream, "mode.sh", "mode\n");
  writeFile(upstream, "same.txt", "same\n");

  writeFile(local, "modify.txt", "local\n");
  writeFile(local, "rename-new.txt", "rename\n");
  writeFile(local, "mode.sh", "mode\n", 0o755);
  writeFile(local, "add.txt", "add\n");
  writeFile(local, "same.txt", "same\n");
  return { root, upstream, local };
}

function writeFile(root, relativePath, content, mode = 0o644) {
  const absolutePath = path.join(root, APP_PATH, relativePath);
  fs.mkdirSync(path.dirname(absolutePath), { recursive: true });
  fs.writeFileSync(absolutePath, content, { mode });
  fs.chmodSync(absolutePath, mode);
}

function ledgerPatch(actualPatch, overrides = {}) {
  return {
    ...ISSUE,
    ...actualPatch,
    ...overrides,
  };
}

function expectFailure(callback, message) {
  assert.throws(callback, new RegExp(message));
}

function registryManifest(packageOverrides = {}, sources = []) {
  return {
    registry: {
      app: {
        name: "@bb/app",
        upstreamPath: "apps/app",
        localPath: "apps/app",
        disposition: "exact-snapshot",
        snapshot: { kind: "exact-snapshot" },
      },
      packages: [{
        name: "@bb/domain",
        upstreamPath: "packages/domain",
        localPath: "ui/packages/domain",
        disposition: "retain-source",
        snapshot: { kind: "adapted-source" },
        ...packageOverrides,
      }],
      sources,
    },
  };
}

function commitFixture(root) {
  execFileSync("git", ["-C", root, "init", "--quiet"]);
  execFileSync("git", ["-C", root, "config", "user.name", "provenance-test"]);
  execFileSync("git", ["-C", root, "config", "user.email", "provenance-test@example.test"]);
  execFileSync("git", ["-C", root, "add", "."]);
  execFileSync("git", ["-C", root, "-c", "commit.gpgSign=false", "commit", "--quiet", "-m", "fixture"]);
}

test("recomputes modify, add, delete, rename, and mode-change diffs", () => {
  const { root, upstream, local } = fixtureRoots();
  const actual = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  assert.deepEqual(actual.map((patch) => patch.kind).sort(), [
    "add",
    "delete",
    "mode-change",
    "modify",
    "rename",
  ]);
  assert.equal(actual.find((patch) => patch.kind === "rename").local.path, "apps/app/rename-new.txt");
  assert.equal(actual.find((patch) => patch.kind === "mode-change").local.mode, "100755");
  fs.rmSync(root, { recursive: true, force: true });
});

test("accepts a complete one-to-one ledger", () => {
  const { root, upstream, local } = fixtureRoots();
  const actual = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  assertLedgerMatchesDiff(actual.map((patch) => ledgerPatch(patch)), actual, APP_PATH, APP_PATH);
  fs.rmSync(root, { recursive: true, force: true });
});

test("rejects unregistered content, add, delete, rename, and mode changes", () => {
  const { root, upstream, local } = fixtureRoots();
  const actual = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  for (const kind of ["modify", "add", "delete", "rename", "mode-change"]) {
    expectFailure(
      () => assertLedgerMatchesDiff([], [actual.find((patch) => patch.kind === kind)], APP_PATH, APP_PATH),
      "unregistered app",
    );
  }
  fs.rmSync(root, { recursive: true, force: true });
});

test("rejects hash mismatch and extra ledger records", () => {
  const { root, upstream, local } = fixtureRoots();
  const actual = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  const modify = actual.find((patch) => patch.kind === "modify");
  const wrongHash = ledgerPatch(modify, {
    local: { ...modify.local, sha256: "0".repeat(64) },
  });
  expectFailure(
    () => assertLedgerMatchesDiff([wrongHash], actual, APP_PATH, APP_PATH),
    "does not match the recomputed diff",
  );
  const extra = ledgerPatch({
    kind: "modify",
    upstream: { path: "apps/app/extra.txt", sha256: "1".repeat(64), mode: "100644" },
    local: { path: "apps/app/extra.txt", sha256: "2".repeat(64), mode: "100644" },
  });
  expectFailure(() => assertLedgerMatchesDiff([extra], [], APP_PATH, APP_PATH), "does not match the recomputed diff");
  fs.rmSync(root, { recursive: true, force: true });
});

test("rejects duplicate, overlapping, and glob ledger scopes", () => {
  const { root, upstream, local } = fixtureRoots();
  const actual = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  const modify = ledgerPatch(actual.find((patch) => patch.kind === "modify"));
  expectFailure(
    () => assertLedgerMatchesDiff([modify, modify], actual, APP_PATH, APP_PATH),
    "duplicate patch entry",
  );

  const overlapping = ledgerPatch({
    kind: "modify",
    upstream: { path: "apps/app/modify.txt/child.txt", sha256: "1".repeat(64), mode: "100644" },
    local: { path: "apps/app/modify.txt/child.txt", sha256: "2".repeat(64), mode: "100644" },
  });
  expectFailure(
    () => assertLedgerMatchesDiff([modify, overlapping], actual, APP_PATH, APP_PATH),
    "overlapping upstream scopes",
  );

  const glob = ledgerPatch(modify, {
    upstream: { ...modify.upstream, path: "apps/app/**" },
  });
  expectFailure(() => assertLedgerMatchesDiff([glob], actual, APP_PATH, APP_PATH), "uses a glob");
  fs.rmSync(root, { recursive: true, force: true });
});

test("treats prototype-property filenames as ordinary files", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-prototype-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  for (const base of [upstream, local]) fs.mkdirSync(path.join(base, APP_PATH), { recursive: true });

  writeFile(upstream, "toString", "upstream\n");
  writeFile(upstream, "constructor", "constructor\n");
  writeFile(upstream, "__proto__", "prototype\n");
  writeFile(local, "toString", "local\n");
  writeFile(local, "constructor", "constructor\n");
  writeFile(local, "hasOwnProperty", "added\n");

  const patches = computePatchDiff(upstream, local, APP_PATH, APP_PATH);
  assert.deepEqual(patches.map((patch) => patch.kind).sort(), ["add", "delete", "modify"]);
  assert.equal(patches.find((patch) => patch.kind === "delete").upstream.path, "apps/app/__proto__");
  assert.equal(patches.find((patch) => patch.kind === "add").local.path, "apps/app/hasOwnProperty");
  fs.rmSync(root, { recursive: true, force: true });
});

test("rejects registered roots and ancestors that are symbolic links", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-symlink-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  const external = path.join(root, "external");
  fs.mkdirSync(path.join(upstream, "apps"), { recursive: true });
  fs.mkdirSync(path.join(local, APP_PATH), { recursive: true });
  fs.mkdirSync(path.join(external, "app"), { recursive: true });
  fs.symlinkSync(external, path.join(upstream, "apps", "app"));

  expectFailure(
    () => computePatchDiff(upstream, local, APP_PATH, APP_PATH),
    "symbolic.?link",
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("keeps the source registry closed and rejects path collisions or traversal", () => {
  expectFailure(
    () => sourceRegistry(registryManifest({ snapshot: { kind: "unknown-kind" } })),
    "unsupported",
  );
  expectFailure(
    () => sourceRegistry(registryManifest({ localPath: "apps/app/generated" })),
    "overlapping local paths",
  );
  expectFailure(
    () => sourceRegistry(registryManifest({ upstreamPath: "../packages/domain" })),
    "normalized repository-relative path|parent-directory",
  );
  expectFailure(
    () => sourceRegistry(registryManifest({ localPath: "ui\\packages\\domain" })),
    "normalized repository-relative path",
  );
});

test("requires closed disposition and snapshot pairings", () => {
  expectFailure(
    () => sourceRegistry(registryManifest({ disposition: "unknown-disposition" })),
    "disposition must be one of",
  );
  expectFailure(
    () => sourceRegistry(registryManifest({ disposition: "exact-snapshot", snapshot: { kind: "adapted-source" } })),
    "requires snapshot.kind exact-snapshot",
  );
  expectFailure(
    () => sourceRegistry(registryManifest({ disposition: "retain-source", snapshot: { kind: "exact-snapshot" } })),
    "requires snapshot.kind adapted-source",
  );
  assert.doesNotThrow(() => sourceRegistry(registryManifest({
    disposition: "exact-snapshot",
    snapshot: { kind: "exact-snapshot" },
  })));
  const manifest = registryManifest();
  manifest.registry.app = {
    ...manifest.registry.app,
    disposition: "retain-source",
    snapshot: { kind: "adapted-source" },
  };
  expectFailure(() => sourceRegistry(manifest), "app.disposition must be exact-snapshot");
});

test("registers standalone exact roots and adapted-package blob overlays without overlap", () => {
  const overlay = {
    name: "@bb/domain/update-state",
    kind: "blob",
    upstreamPath: "packages/domain/src/update-state.ts",
    localPath: "ui/packages/domain/src/update-state.ts",
    disposition: "exact-snapshot",
    snapshot: { kind: "exact-snapshot" },
    materialization: "planned",
  };
  assert.doesNotThrow(() => sourceRegistry(registryManifest({}, [overlay])));

  expectFailure(
    () => sourceRegistry(registryManifest({}, [{
      ...overlay,
      kind: "source",
      upstreamPath: "packages/domain/src",
      localPath: "ui/packages/domain/src",
    }])),
    "overlapping exact root paths",
  );
  const exactPackage = registryManifest({ disposition: "exact-snapshot", snapshot: { kind: "exact-snapshot" } });
  expectFailure(
    () => sourceRegistry({
      ...exactPackage,
      registry: {
        ...exactPackage.registry,
        packages: [{ ...exactPackage.registry.packages[0], upstreamPath: "packages/exact", localPath: "ui/packages/exact" }],
        sources: [{ ...overlay, upstreamPath: "packages/exact/src/update-state.ts", localPath: "ui/packages/exact/src/update-state.ts" }],
      },
    }),
    "overlapping exact root paths",
  );
});

test("compares exact snapshots across different registered paths and modes", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-exact-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  fs.mkdirSync(path.join(upstream, "source", "app"), { recursive: true });
  fs.mkdirSync(path.join(local, "dest", "app"), { recursive: true });
  for (const base of [upstream, local]) {
    fs.writeFileSync(path.join(base, base === upstream ? "source/app/main.txt" : "dest/app/main.txt"), "same\n");
    fs.writeFileSync(path.join(base, base === upstream ? "source/app/run.sh" : "dest/app/run.sh"), "#!/bin/sh\n");
    fs.chmodSync(path.join(base, base === upstream ? "source/app/main.txt" : "dest/app/main.txt"), 0o644);
    fs.chmodSync(path.join(base, base === upstream ? "source/app/run.sh" : "dest/app/run.sh"), 0o755);
    commitFixture(base);
  }

  const entry = { name: "@bb/app", upstreamPath: "source/app", localPath: "dest/app" };
  assert.doesNotThrow(() => assertExactSourceEntry(entry, upstream, "fixture exact snapshot", local));
  execFileSync("git", ["-C", local, "config", "core.filemode", "false"]);
  fs.chmodSync(path.join(local, "dest/app/run.sh"), 0o644);
  expectFailure(() => assertExactSourceEntry(entry, upstream, "fixture exact snapshot", local), "must be clean");
  fs.rmSync(root, { recursive: true, force: true });
});

test("ignores an untracked pnpm dependency symlink but rejects tracked symlinks", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-ignored-link-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  fs.mkdirSync(path.join(upstream, "packages", "exact"), { recursive: true });
  fs.mkdirSync(path.join(local, "ui", "packages", "exact", "node_modules"), { recursive: true });
  writeRepositoryFile(upstream, "packages/exact/index.ts", "export const exact = true;\n");
  writeRepositoryFile(local, "ui/packages/exact/index.ts", "export const exact = true;\n");
  fs.mkdirSync(path.join(local, "vendor", "dependency"), { recursive: true });
  fs.symlinkSync("../../../../vendor/dependency", path.join(local, "ui/packages/exact/node_modules/dependency"), "dir");
  fs.writeFileSync(path.join(local, ".gitignore"), "**/node_modules/\n");
  commitFixture(upstream);
  commitFixture(local);

  const entry = { name: "@bb/exact", kind: "source", upstreamPath: "packages/exact", localPath: "ui/packages/exact" };
  assert.doesNotThrow(() => assertExactSourceEntry(entry, upstream, "ignored dependency exact", local));

  fs.rmSync(path.join(local, "ui/packages/exact/node_modules/dependency"), { force: true });
  fs.symlinkSync("../../../../vendor/dependency", path.join(local, "ui/packages/exact/tracked-link"), "dir");
  execFileSync("git", ["-C", local, "add", "-f", "ui/packages/exact/tracked-link"]);
  expectFailure(
    () => computePatchDiff(upstream, local, "packages/exact", "ui/packages/exact"),
    "symbolic.?link",
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("compares standalone exact blobs by blob, bytes, and mode", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "loom-provenance-blob-"));
  const upstream = path.join(root, "upstream");
  const local = path.join(root, "local");
  writeRepositoryFile(upstream, "source/blob.txt", "same blob\n");
  writeRepositoryFile(local, "dest/blob.txt", "same blob\n");
  commitFixture(upstream);
  commitFixture(local);

  const entry = { name: "bb/blob", kind: "blob", upstreamPath: "source/blob.txt", localPath: "dest/blob.txt" };
  assert.doesNotThrow(() => assertExactBlobEntry(entry, upstream, "fixture exact blob", local));
  fs.writeFileSync(path.join(local, "dest/blob.txt"), "changed blob\n");
  expectFailure(() => assertExactBlobEntry(entry, upstream, "fixture exact blob", local), "must be clean");
  fs.rmSync(root, { recursive: true, force: true });
});

function writeRepositoryFile(root, relativePath, content, mode = 0o644) {
  const absolutePath = path.join(root, relativePath);
  fs.mkdirSync(path.dirname(absolutePath), { recursive: true });
  fs.writeFileSync(absolutePath, content, { mode });
  fs.chmodSync(absolutePath, mode);
}
