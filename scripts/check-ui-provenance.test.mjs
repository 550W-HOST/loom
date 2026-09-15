import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { assertLedgerMatchesDiff, computePatchDiff } from "./check-ui-provenance.mjs";

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
