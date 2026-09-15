#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = path.join(repoRoot, "ui", "provenance.json");
const patchLedgerPath = path.join(repoRoot, "ui", "app-patch-ledger.json");
const contractManifestPath = path.join(repoRoot, "contracts", "bb", "manifest.json");

const DEFAULT_SOURCE = {
  repository: "https://github.com/get-bb/bb",
  commit: "fa1f44ebe9e5676004b669e48c99b3c7606466b6",
  commitTitle: "Cut startup JavaScript by 81 KiB and restore 5% bundle headroom (#3476)",
};

const PATCH_LEDGER_FORMAT = "loom.ui-patch-ledger/v2";
const PATCH_KINDS = new Set(["modify", "add", "delete", "rename", "mode-change"]);
const GLOB_CHARACTERS = /[*?\[\]{}]/;

function fail(message) {
  console.error(`ui provenance: ${message}`);
  process.exitCode = 1;
}

function readJson(filePath) {
  try {
    return JSON.parse(fs.readFileSync(filePath, "utf8"));
  } catch (error) {
    throw new Error(`${path.relative(repoRoot, filePath)}: ${error.message}`);
  }
}

function sha256Bytes(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function fileDigest(relativePath, root = repoRoot) {
  const absolutePath = path.join(root, relativePath);
  const bytes = fs.readFileSync(absolutePath);
  return { bytes: bytes.length, sha256: sha256Bytes(bytes) };
}

function fileMode(relativePath, root = repoRoot) {
  const stat = fs.statSync(path.join(root, relativePath));
  if (!stat.isFile()) throw new Error(`${relativePath}: expected a regular file`);
  return (stat.mode & 0o111) === 0 ? "100644" : "100755";
}

function filesUnder(relativePath, root = repoRoot, includeAllFiles = false) {
  const absolutePath = path.join(root, relativePath);
  const files = [];

  function visit(directory, prefix) {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) =>
      a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
    )) {
      const entryPath = path.join(directory, entry.name);
      const entryRelativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (!includeAllFiles && (entry.name === "node_modules" || entry.name === "dist")) continue;
      if (entry.isDirectory()) visit(entryPath, entryRelativePath);
      else if (entry.isFile()) files.push(entryRelativePath);
      else throw new Error(`${relativePath}: unsupported directory entry ${entryRelativePath}`);
    }
  }

  if (!fs.existsSync(absolutePath)) throw new Error(`${relativePath}: path does not exist`);
  visit(absolutePath, "");
  return files;
}

function treeDigest(relativePath, root = repoRoot, includeAllFiles = false) {
  const files = filesUnder(relativePath, root, includeAllFiles);
  const hash = crypto.createHash("sha256");
  for (const relativeFile of files) {
    const digest = fileDigest(path.join(relativePath, relativeFile), root);
    hash.update(`${relativeFile}\0${digest.sha256}\n`);
  }
  return { files: files.length, sha256: hash.digest("hex") };
}

function fileRecords(relativePath, root = repoRoot, includeAllFiles = false) {
  return filesUnder(relativePath, root, includeAllFiles).map((relativeFile) => ({
    path: relativeFile,
    ...fileDigest(path.join(relativePath, relativeFile), root),
  }));
}

function sourceSnapshot(relativePath, root = repoRoot) {
  return {
    path: relativePath,
    tree: treeDigest(relativePath, root, true),
    files: fileRecords(relativePath, root, true),
  };
}

function dependencySnapshot(packageJson) {
  const groups = {};
  for (const group of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
    groups[group] = Object.fromEntries(
      Object.entries(packageJson[group] ?? {}).sort(([left], [right]) =>
        left < right ? -1 : left > right ? 1 : 0,
      ),
    );
  }
  return groups;
}

function dependencyDigest(packageJson) {
  return sha256Bytes(Buffer.from(JSON.stringify(dependencySnapshot(packageJson))));
}

function packageRecord(name, relativePath, root, extra = {}) {
  const packageJsonPath = path.join(root, relativePath, "package.json");
  const packageJson = readJson(packageJsonPath);
  return {
    name,
    ...extra,
    path: relativePath,
    tree: treeDigest(relativePath, root),
    packageJson: fileDigest(path.join(relativePath, "package.json"), root),
    dependencies: dependencySnapshot(packageJson),
    dependencyDigest: dependencyDigest(packageJson),
  };
}

function assertRepositoryPath(value, label) {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${label} must be a non-empty path`);
  if (value.startsWith("/") || value.includes("\\") || path.posix.normalize(value) !== value || value === ".") {
    throw new Error(`${label} must be a normalized repository-relative path`);
  }
  if (value.split("/").includes("..")) throw new Error(`${label} may not escape the repository`);
}

function sourceRegistry(manifest) {
  const registry = manifest?.registry;
  if (!registry || typeof registry !== "object") {
    throw new Error("manifest.registry is required; migrate the provenance manifest to the registry schema");
  }
  const app = registry.app;
  if (!app || typeof app !== "object") throw new Error("manifest.registry.app is required");
  for (const field of ["name", "upstreamPath", "localPath", "disposition"]) {
    if (typeof app[field] !== "string" || app[field].length === 0) {
      throw new Error(`manifest.registry.app.${field} is required`);
    }
  }
  assertRepositoryPath(app.upstreamPath, "manifest.registry.app.upstreamPath");
  assertRepositoryPath(app.localPath, "manifest.registry.app.localPath");
  if (!app.snapshot || typeof app.snapshot.kind !== "string") {
    throw new Error("manifest.registry.app.snapshot.kind is required");
  }

  if (!Array.isArray(registry.packages) || registry.packages.length === 0) {
    throw new Error("manifest.registry.packages must be a non-empty array");
  }
  const names = new Set();
  const upstreamPaths = new Set();
  const localPaths = new Set();
  for (const [index, item] of registry.packages.entries()) {
    if (!item || typeof item !== "object") throw new Error(`manifest.registry.packages[${index}] must be an object`);
    for (const field of ["name", "upstreamPath", "localPath", "disposition"]) {
      if (typeof item[field] !== "string" || item[field].length === 0) {
        throw new Error(`manifest.registry.packages[${index}].${field} is required`);
      }
    }
    assertRepositoryPath(item.upstreamPath, `manifest.registry.packages[${index}].upstreamPath`);
    assertRepositoryPath(item.localPath, `manifest.registry.packages[${index}].localPath`);
    if (!item.snapshot || typeof item.snapshot.kind !== "string") {
      throw new Error(`manifest.registry.packages[${index}].snapshot.kind is required`);
    }
    if (names.has(item.name)) throw new Error(`manifest.registry.packages has duplicate name ${item.name}`);
    if (upstreamPaths.has(item.upstreamPath)) throw new Error(`manifest.registry.packages has duplicate upstream path ${item.upstreamPath}`);
    if (localPaths.has(item.localPath)) throw new Error(`manifest.registry.packages has duplicate local path ${item.localPath}`);
    names.add(item.name);
    upstreamPaths.add(item.upstreamPath);
    localPaths.add(item.localPath);
  }
  return registry;
}

function localAppRecord(previous, appEntry) {
  const packageJson = readJson(path.join(repoRoot, "ui", "package.json"));
  return {
    upstreamPath: appEntry.upstreamPath,
    upstreamDisposition: previous?.upstreamDisposition ?? "defer-source-port",
    localSourcePath: "ui/src",
    localSourceDisposition: previous?.localSourceDisposition ?? "loom-native-reference-client",
    sourceTree: treeDigest("ui/src"),
    packageJson: fileDigest("ui/package.json"),
    dependencies: dependencySnapshot(packageJson),
    dependencyDigest: dependencyDigest(packageJson),
    bundlePath: "ui/app.js",
    bundle: fileDigest("ui/app.js"),
  };
}

function upstreamRecord(root, previous, registry) {
  if (!root) return previous;
  const appEntry = registry.app;
  const appPackageJson = readJson(path.join(root, appEntry.upstreamPath, "package.json"));
  return {
    repository: DEFAULT_SOURCE.repository,
    commit: gitHead(root),
    app: {
      path: appEntry.upstreamPath,
      tree: treeDigest(appEntry.upstreamPath, root),
      gitTree: gitTreeId(appEntry.upstreamPath, root),
      files: fileRecords(appEntry.upstreamPath, root),
      packageJson: fileDigest(path.join(appEntry.upstreamPath, "package.json"), root),
      dependencies: dependencySnapshot(appPackageJson),
      dependencyDigest: dependencyDigest(appPackageJson),
    },
    packages: registry.packages.map((entry) => packageRecord(entry.name, entry.upstreamPath, root)),
  };
}

function contractRecord(existingContract) {
  const contractManifest = readJson(contractManifestPath);
  const artifacts = {};
  for (const [fileName, expected] of Object.entries(contractManifest.files ?? {})) {
    const digest = fileDigest(path.join("contracts", "bb", fileName));
    artifacts[fileName] = { bytes: digest.bytes, sha256: digest.sha256 };
    if (expected.bytes !== digest.bytes || expected.sha256 !== digest.sha256) {
      throw new Error(`contracts/bb/${fileName}: manifest.json has stale bytes or sha256`);
    }
  }
  return {
    manifestPath: "contracts/bb/manifest.json",
    manifestSha256: sha256Bytes(fs.readFileSync(contractManifestPath)),
    sourceRepository: contractManifest.source?.repository,
    sourceCommit: contractManifest.source?.commit,
    sourcePackages: contractManifest.source?.packages ?? [],
    artifacts,
    ...(existingContract?.disposition ? { disposition: existingContract.disposition } : {}),
  };
}

function gitHead(root) {
  try {
    return execFileSync("git", ["-C", root, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
  } catch (error) {
    throw new Error(`${root}: cannot read checkout HEAD: ${error.message}`);
  }
}

function gitTreeId(relativePath, root) {
  try {
    return execFileSync("git", ["-C", root, "rev-parse", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
  } catch (error) {
    throw new Error(`${root}: cannot read git tree ${relativePath}: ${error.message}`);
  }
}

function buildManifest(existing, upstreamRoot) {
  const registry = sourceRegistry(existing);
  const source = { ...DEFAULT_SOURCE, ...(existing?.source ?? {}) };
  const localPackages = registry.packages.map((entry) => {
    const previous = existing?.local?.packages?.find((item) => item.name === entry.name);
    return packageRecord(entry.name, entry.localPath, repoRoot, {
      upstreamPath: entry.upstreamPath,
      disposition: previous?.disposition ?? entry.disposition,
    });
  });
  const local = {
    referenceApp: localAppRecord(existing?.local?.referenceApp ?? existing?.app, registry.app),
    productApp: {
      upstreamPath: registry.app.upstreamPath,
      localPath: registry.app.localPath,
      disposition: registry.app.disposition,
      gitTree: gitTreeId(registry.app.localPath, repoRoot),
      snapshot: sourceSnapshot(registry.app.localPath),
    },
    packages: localPackages,
    imports: registry.packages.map((entry) => ({
      package: entry.name,
      upstreamPath: entry.upstreamPath,
      localPath: entry.localPath,
      disposition: entry.disposition,
    })),
  };

  return {
    format: "loom.ui-provenance/v2",
    generator: "scripts/check-ui-provenance.mjs",
    registry,
    source,
    upstream: upstreamRecord(upstreamRoot, existing?.upstream, registry),
    local,
    contracts: contractRecord(existing?.contracts),
    forbiddenReferenceAppImports: existing?.forbiddenReferenceAppImports ?? [
      "apps/app",
      "plugins/",
      "marketplace",
      "desktopBrowsers",
      "cli-skills",
    ],
  };
}

function assertEqual(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

function checkSnapshot(relativePath, expected, label, root = repoRoot) {
  const actual = sourceSnapshot(relativePath, root);
  assertEqual(actual.path, expected.path, `${label} path`);
  assertEqual(actual.tree, expected.tree, `${label} tree`);
  if (actual.files.length !== expected.files.length) {
    throw new Error(`${label}: expected ${expected.files.length} files, got ${actual.files.length}`);
  }
  for (let index = 0; index < expected.files.length; index += 1) {
    const expectedFile = expected.files[index];
    const actualFile = actual.files[index];
    if (JSON.stringify(actualFile) !== JSON.stringify(expectedFile)) {
      throw new Error(`${label}: file ${expectedFile.path} differs (expected ${JSON.stringify(expectedFile)}, got ${JSON.stringify(actualFile)})`);
    }
  }
}

function joinRepositoryPath(directory, relativeFile) {
  return path.posix.join(directory, relativeFile);
}

function changeFiles(relativePath, root, displayPath) {
  return Object.fromEntries(
    filesUnder(relativePath, root, true).map((relativeFile) => {
      const digest = fileDigest(path.join(relativePath, relativeFile), root);
      return [relativeFile, {
        path: joinRepositoryPath(displayPath, relativeFile),
        sha256: digest.sha256,
        mode: fileMode(path.join(relativePath, relativeFile), root),
      }];
    }),
  );
}

function absentPatchSide() {
  return { path: null, sha256: null, mode: null };
}

function patchRecord(kind, upstream = absentPatchSide(), local = absentPatchSide()) {
  return { kind, upstream, local };
}

function patchSortKey(patch) {
  return [patch.kind, patch.upstream.path ?? "", patch.local.path ?? ""].join("\0");
}

function computePatchDiff(upstreamRoot, localRoot, upstreamPath = "apps/app", localPath = upstreamPath) {
  const upstreamFiles = changeFiles(upstreamPath, upstreamRoot, upstreamPath);
  const localFiles = changeFiles(localPath, localRoot, localPath);
  const upstreamOnly = new Set(Object.keys(upstreamFiles).filter((relativeFile) => !(relativeFile in localFiles)));
  const localOnly = new Set(Object.keys(localFiles).filter((relativeFile) => !(relativeFile in upstreamFiles)));
  const patches = [];

  for (const relativeFile of Object.keys(upstreamFiles).sort()) {
    if (!(relativeFile in localFiles)) continue;
    const upstream = upstreamFiles[relativeFile];
    const local = localFiles[relativeFile];
    if (upstream.sha256 !== local.sha256) patches.push(patchRecord("modify", upstream, local));
    else if (upstream.mode !== local.mode) patches.push(patchRecord("mode-change", upstream, local));
  }

  const addedByFingerprint = new Map();
  for (const relativeFile of [...localOnly].sort()) {
    const local = localFiles[relativeFile];
    const fingerprint = `${local.sha256}\0${local.mode}`;
    const entries = addedByFingerprint.get(fingerprint) ?? [];
    entries.push(relativeFile);
    addedByFingerprint.set(fingerprint, entries);
  }

  for (const relativeFile of [...upstreamOnly].sort()) {
    const upstream = upstreamFiles[relativeFile];
    const fingerprint = `${upstream.sha256}\0${upstream.mode}`;
    const candidates = addedByFingerprint.get(fingerprint) ?? [];
    if (candidates.length !== 1) continue;
    const localRelativeFile = candidates[0];
    addedByFingerprint.delete(fingerprint);
    localOnly.delete(localRelativeFile);
    upstreamOnly.delete(relativeFile);
    patches.push(patchRecord("rename", upstream, localFiles[localRelativeFile]));
  }

  for (const relativeFile of [...upstreamOnly].sort()) {
    patches.push(patchRecord("delete", upstreamFiles[relativeFile], absentPatchSide()));
  }
  for (const relativeFile of [...localOnly].sort()) {
    patches.push(patchRecord("add", absentPatchSide(), localFiles[relativeFile]));
  }

  return patches.sort((left, right) => patchSortKey(left).localeCompare(patchSortKey(right)));
}

function validatePatchPath(value, label, scopePath) {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${label} path is required`);
  if (GLOB_CHARACTERS.test(value)) throw new Error(`${label} uses a glob; patch ledger entries must name one file`);
  if (value.includes("\\") || value.startsWith("/") || path.posix.normalize(value) !== value) {
    throw new Error(`${label} path must be a normalized repository-relative path`);
  }
  if (value !== scopePath && !value.startsWith(`${scopePath}/`)) {
    throw new Error(`${label} path ${value} is outside ${scopePath}`);
  }
  return value;
}

function validatePatchSide(value, label, scopePath) {
  if (value === null) return absentPatchSide();
  if (!value || typeof value !== "object") throw new Error(`${label} must be an object or null`);
  const side = {
    path: value.path ?? null,
    sha256: value.sha256 ?? null,
    mode: value.mode ?? null,
  };
  const empty = side.path === null && side.sha256 === null && side.mode === null;
  if (empty) return side;
  if (side.path === null || side.sha256 === null || side.mode === null) {
    throw new Error(`${label} must provide path, sha256, and mode together`);
  }
  validatePatchPath(side.path, label, scopePath);
  if (!/^[0-9a-f]{64}$/.test(side.sha256)) throw new Error(`${label}.sha256 must be a 64-character lowercase SHA-256`);
  if (!/^100[0-7]{3}$/.test(side.mode)) throw new Error(`${label}.mode must be a regular-file git mode`);
  return side;
}

function sidePresent(side) {
  return side.path !== null;
}

function validatePatchEntry(rawPatch, index, upstreamPath, localPath) {
  if (!rawPatch || typeof rawPatch !== "object") throw new Error(`app patch ledger patches[${index}] must be an object`);
  if (!PATCH_KINDS.has(rawPatch.kind)) throw new Error(`app patch ledger patches[${index}] has unsupported kind ${rawPatch.kind}`);
  for (const field of ["issue", "owner", "reason"]) {
    if (typeof rawPatch[field] !== "string" || rawPatch[field].length === 0) {
      throw new Error(`app patch ledger patches[${index}].${field} is required`);
    }
  }
  const upstream = validatePatchSide(rawPatch.upstream, `patches[${index}].upstream`, upstreamPath);
  const local = validatePatchSide(rawPatch.local, `patches[${index}].local`, localPath);
  if (rawPatch.kind === "add" && sidePresent(upstream)) throw new Error(`patches[${index}] add must not have an upstream file`);
  if (rawPatch.kind === "delete" && sidePresent(local)) throw new Error(`patches[${index}] delete must not have a local file`);
  if (rawPatch.kind === "rename") {
    if (!sidePresent(upstream) || !sidePresent(local) || upstream.path === local.path) {
      throw new Error(`patches[${index}] rename must change one existing path to another`);
    }
    if (upstream.sha256 !== local.sha256 || upstream.mode !== local.mode) {
      throw new Error(`patches[${index}] rename must preserve hash and mode; use modify for content changes`);
    }
  }
  if (rawPatch.kind === "mode-change") {
    if (!sidePresent(upstream) || !sidePresent(local) || upstream.path !== local.path) {
      throw new Error(`patches[${index}] mode-change must keep one path`);
    }
    if (upstream.sha256 !== local.sha256 || upstream.mode === local.mode) {
      throw new Error(`patches[${index}] mode-change must preserve content and change mode`);
    }
  }
  if (rawPatch.kind === "modify") {
    if (!sidePresent(upstream) || !sidePresent(local) || upstream.path !== local.path) {
      throw new Error(`patches[${index}] modify must keep one path`);
    }
    if (upstream.sha256 === local.sha256 && upstream.mode === local.mode) {
      throw new Error(`patches[${index}] modify must change content or mode`);
    }
  }
  return { kind: rawPatch.kind, upstream, local };
}

function patchFingerprint(patch) {
  return JSON.stringify({ kind: patch.kind, upstream: patch.upstream, local: patch.local });
}

function assertNoOverlappingScopes(patches) {
  for (const sideName of ["upstream", "local"]) {
    const paths = [];
    for (const patch of patches) {
      const value = patch[sideName].path;
      if (value !== null) paths.push(value);
    }
    for (let left = 0; left < paths.length; left += 1) {
      for (let right = left + 1; right < paths.length; right += 1) {
        const first = paths[left];
        const second = paths[right];
        if (first === second || first.startsWith(`${second}/`) || second.startsWith(`${first}/`)) {
          throw new Error(`app patch ledger has overlapping ${sideName} scopes: ${first} and ${second}`);
        }
      }
    }
  }
}

function assertLedgerMatchesDiff(rawPatches, actualPatches, upstreamPath = "apps/app", localPath = upstreamPath) {
  if (!Array.isArray(rawPatches)) throw new Error("app patch ledger patches must be an array");
  const patches = rawPatches.map((patch, index) => validatePatchEntry(patch, index, upstreamPath, localPath));

  const expected = new Map();
  for (let index = 0; index < patches.length; index += 1) {
    const fingerprint = patchFingerprint(patches[index]);
    if (expected.has(fingerprint)) throw new Error(`app patch ledger has duplicate patch entry at index ${index}`);
    expected.set(fingerprint, index);
  }
  assertNoOverlappingScopes(patches);

  const actual = new Map(actualPatches.map((patch) => [patchFingerprint(patch), patch]));
  for (const [fingerprint, index] of expected) {
    if (!actual.has(fingerprint)) {
      throw new Error(`app patch ledger patch ${index} does not match the recomputed diff (kind, path, hash, or mode mismatch)`);
    }
  }
  for (const patch of actualPatches) {
    if (!expected.has(patchFingerprint(patch))) {
      throw new Error(`unregistered app ${patch.kind} diff: ${patch.upstream.path ?? patch.local.path}`);
    }
  }
  if (expected.size !== actual.size) {
    throw new Error(`app patch ledger has ${expected.size} entries but recomputed ${actual.size} app diffs`);
  }
  return patches;
}

function checkPatchLedger(manifest, upstreamRoot) {
  const registry = sourceRegistry(manifest);
  const ledger = readJson(patchLedgerPath);
  if (ledger.format !== PATCH_LEDGER_FORMAT) throw new Error(`unsupported app patch ledger format; expected ${PATCH_LEDGER_FORMAT}`);
  assertEqual(ledger.source, {
    repository: manifest.source.repository,
    commit: manifest.source.commit,
  }, "app patch ledger source");
  assertEqual(ledger.import, {
    upstreamPath: registry.app.upstreamPath,
    localPath: registry.app.localPath,
    disposition: registry.app.disposition,
  }, "app patch ledger import");
  const baseline = ledger.baseline;
  if (!baseline || baseline.kind !== "exact-snapshot") {
    throw new Error("app patch ledger must declare an exact snapshot baseline");
  }
  for (const field of ["owner", "issue", "reason"]) {
    if (typeof baseline[field] !== "string" || baseline[field].length === 0) {
      throw new Error(`app patch ledger baseline.${field} is required`);
    }
  }
  if (!Array.isArray(baseline.affectedUpstreamFiles)) throw new Error("app patch ledger baseline.affectedUpstreamFiles must be an array");
  if (!Array.isArray(ledger.patches)) throw new Error("app patch ledger patches must be an array");
  if (!upstreamRoot && ledger.patches.length !== 0) {
    throw new Error("BB_SRC is required to recompute non-empty app patch ledger entries");
  }
  if (upstreamRoot) {
    const actualPatches = computePatchDiff(
      upstreamRoot,
      repoRoot,
      registry.app.upstreamPath,
      registry.app.localPath,
    );
    assertLedgerMatchesDiff(
      ledger.patches,
      actualPatches,
      registry.app.upstreamPath,
      registry.app.localPath,
    );
  } else {
    assertLedgerMatchesDiff(ledger.patches, [], registry.app.upstreamPath, registry.app.localPath);
  }
}

function checkLocalProductApp(manifest) {
  const registry = sourceRegistry(manifest);
  const productApp = manifest.local.productApp;
  if (!productApp) throw new Error("local.productApp is required");
  assertEqual(productApp.upstreamPath, registry.app.upstreamPath, "local product app upstream path");
  assertEqual(productApp.localPath, registry.app.localPath, "local product app path");
  assertEqual(productApp.disposition, registry.app.disposition, "local product app disposition");
  assertEqual(productApp.gitTree, gitTreeId(registry.app.localPath, repoRoot), "local product app git tree");
  checkSnapshot(registry.app.localPath, productApp.snapshot, "local product app snapshot");
  if (registry.app.disposition === "exact-snapshot") {
    assertEqual(productApp.gitTree, manifest.upstream.app.gitTree, "local/upstream product app git tree");
    checkSnapshot(registry.app.localPath, {
      path: manifest.upstream.app.path,
      tree: manifest.upstream.app.tree,
      files: manifest.upstream.app.files,
    }, "local product app vs upstream snapshot");
  }
}

function checkUpstream(manifest, root) {
  const registry = sourceRegistry(manifest);
  const head = gitHead(root);
  assertEqual(head, manifest.source.commit, "upstream checkout HEAD");
  assertEqual(manifest.upstream.repository, manifest.source.repository, "upstream repository");
  assertEqual(manifest.upstream.commit, head, "upstream manifest commit");

  const appEntry = registry.app;
  const expectedApp = manifest.upstream.app;
  assertEqual(expectedApp.path, appEntry.upstreamPath, "upstream app path");
  const appPackageJson = readJson(path.join(root, expectedApp.path, "package.json"));
  assertEqual(appPackageJson.name, appEntry.name, "upstream app package name");
  assertEqual(sourceSnapshot(expectedApp.path, root), {
    path: expectedApp.path,
    tree: expectedApp.tree,
    files: expectedApp.files,
  }, "upstream apps/app file snapshot");
  assertEqual(gitTreeId(expectedApp.path, root), expectedApp.gitTree, "upstream apps/app git tree");
  assertEqual(fileDigest(path.join(expectedApp.path, "package.json"), root), expectedApp.packageJson, "upstream app package.json");
  assertEqual(dependencySnapshot(appPackageJson), expectedApp.dependencies, "upstream app dependencies");
  assertEqual(dependencyDigest(appPackageJson), expectedApp.dependencyDigest, "upstream app dependency digest");

  assertEqual(manifest.upstream.packages.map((item) => item.name), registry.packages.map((item) => item.name), "upstream package names");
  for (const item of manifest.upstream.packages) {
    const entry = registry.packages.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: package is absent from the source registry`);
    assertEqual(item.path, entry.upstreamPath, `${item.name} upstream path`);
    const packageJson = readJson(path.join(root, item.path, "package.json"));
    assertEqual(packageJson.name, item.name, `${item.name} upstream package name`);
    assertEqual(treeDigest(item.path, root), item.tree, `${item.name} upstream tree`);
    assertEqual(fileDigest(path.join(item.path, "package.json"), root), item.packageJson, `${item.name} upstream package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} upstream dependencies`);
    assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} upstream dependency digest`);
  }
}

function checkLocal(manifest, upstreamRoot) {
  const registry = sourceRegistry(manifest);
  if (manifest.format !== "loom.ui-provenance/v2") throw new Error("unsupported manifest format");
  if (!/^[0-9a-f]{40}$/.test(manifest.source?.commit ?? "")) {
    throw new Error("source.commit must be a full 40-character git commit");
  }
  if (manifest.source.repository !== DEFAULT_SOURCE.repository) {
    throw new Error(`source.repository must be ${DEFAULT_SOURCE.repository}`);
  }
  assertEqual(manifest.source.commit, manifest.upstream.commit, "source/upstream commit");
  checkLocalProductApp(manifest);
  checkPatchLedger(manifest, upstreamRoot);

  const referenceApp = manifest.local.referenceApp;
  const localAppPackageJson = readJson(path.join(repoRoot, "ui", "package.json"));
  assertEqual(treeDigest(referenceApp.localSourcePath), referenceApp.sourceTree, "local reference app source tree");
  assertEqual(fileDigest("ui/package.json"), referenceApp.packageJson, "local reference app package.json");
  assertEqual(dependencySnapshot(localAppPackageJson), referenceApp.dependencies, "local reference app dependencies");
  assertEqual(dependencyDigest(localAppPackageJson), referenceApp.dependencyDigest, "local reference app dependency digest");
  assertEqual(fileDigest(referenceApp.bundlePath), referenceApp.bundle, "local reference app bundle");

  assertEqual(manifest.local.packages.map((item) => item.name), registry.packages.map((item) => item.name), "local package names");
  for (const item of manifest.local.packages) {
    const entry = registry.packages.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: package is absent from the source registry`);
    assertEqual(item.path, entry.localPath, `${item.name} local path`);
    const packageJson = readJson(path.join(repoRoot, item.path, "package.json"));
    assertEqual(packageJson.name, item.name, `${item.name} local package name`);
    assertEqual(treeDigest(item.path), item.tree, `${item.name} local/adapted tree`);
    assertEqual(fileDigest(path.join(item.path, "package.json")), item.packageJson, `${item.name} local package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} local dependencies`);
    assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} local dependency digest`);
  }

  assertEqual(manifest.local.imports.map((item) => item.package), registry.packages.map((item) => item.name), "import package names");
  for (let index = 0; index < manifest.local.imports.length; index += 1) {
    const imported = manifest.local.imports[index];
    const entry = registry.packages[index];
    assertEqual(imported.localPath, entry.localPath, `import ${index} local path`);
    assertEqual(imported.upstreamPath, entry.upstreamPath, `import ${index} upstream path`);
    assertEqual(imported.disposition, entry.disposition, `import ${index} disposition`);
  }

  const contracts = contractRecord(manifest.contracts);
  assertEqual(contracts, manifest.contracts, "contract artifacts");
  assertEqual(contracts.sourceRepository, manifest.source.repository, "contract source repository");
  assertEqual(contracts.sourceCommit, manifest.source.commit, "contract source commit");

  const sourceText = filesUnder(referenceApp.localSourcePath)
    .map((relativeFile) => fs.readFileSync(path.join(repoRoot, referenceApp.localSourcePath, relativeFile), "utf8"))
    .join("\n");
  for (const forbidden of manifest.forbiddenReferenceAppImports) {
    if (sourceText.includes(forbidden)) throw new Error(`forbidden reference app import/surface found: ${forbidden}`);
  }
}

function parseUpstreamArgument() {
  const argumentIndex = process.argv.indexOf("--upstream");
  if (argumentIndex < 0) return process.env.BB_SRC ? path.resolve(process.env.BB_SRC) : null;
  const value = process.argv[argumentIndex + 1];
  if (!value || value.startsWith("--")) throw new Error("--upstream requires a checkout path");
  return path.resolve(value);
}

function main() {
  try {
    const manifest = readJson(manifestPath);
    const upstreamRoot = parseUpstreamArgument();
    let checkedManifest = manifest;
    if (process.argv.includes("--write")) {
      checkedManifest = buildManifest(manifest, upstreamRoot);
      fs.writeFileSync(manifestPath, `${JSON.stringify(checkedManifest, null, 2)}\n`);
      console.log(`wrote ${path.relative(repoRoot, manifestPath)}`);
    }
    checkLocal(checkedManifest, upstreamRoot);
    if (upstreamRoot) checkUpstream(checkedManifest, upstreamRoot);
    console.log(`ui provenance OK: bb ${checkedManifest.source.commit}`);
  } catch (error) {
    fail(error.message);
  }
}

export {
  assertLedgerMatchesDiff,
  computePatchDiff,
  sourceRegistry,
};

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
