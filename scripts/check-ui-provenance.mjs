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

const PROVENANCE_FORMAT = "loom.ui-provenance/v3";
const PATCH_LEDGER_FORMAT = "loom.ui-patch-ledger/v2";
const PATCH_KINDS = new Set(["modify", "add", "delete", "rename", "mode-change"]);
const SNAPSHOT_KINDS = new Set(["exact-snapshot", "adapted-source"]);
const DISPOSITION_SNAPSHOT_KINDS = new Map([
  ["exact-snapshot", "exact-snapshot"],
  ["retain-source", "adapted-source"],
  ["source-port", "adapted-source"],
]);
const APP_DISPOSITIONS = new Set(["exact-snapshot", "source-port"]);
const PACKAGE_DISPOSITIONS = new Set(["exact-snapshot", "retain-source"]);
const STANDALONE_SOURCE_KINDS = new Set(["blob", "source"]);
const MATERIALIZATION_STATES = new Set(["planned", "materialized"]);
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

function readJsonAt(relativePath, root, label = relativePath) {
  return readJson(repositoryPath(relativePath, root, label));
}

function sha256Bytes(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function assertRepositoryPath(value, label) {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${label} must be a non-empty path`);
  if (
    value.startsWith("/") ||
    value.includes("\\") ||
    value.endsWith("/") ||
    path.posix.normalize(value) !== value
  ) {
    throw new Error(`${label} must be a normalized repository-relative path`);
  }
  if (value.split("/").some((segment) => segment.length === 0 || segment === "." || segment === "..")) {
    throw new Error(`${label} may not contain empty, current-directory, or parent-directory segments`);
  }
}

function repositoryRoot(root, label = "repository root") {
  const absoluteRoot = path.resolve(root);
  let rootStat;
  try {
    rootStat = fs.lstatSync(absoluteRoot);
  } catch (error) {
    throw new Error(`${label}: ${error.message}`);
  }
  if (rootStat.isSymbolicLink()) throw new Error(`${label} may not be a symbolic link`);
  if (!rootStat.isDirectory()) throw new Error(`${label} must be a directory`);
  try {
    fs.realpathSync(absoluteRoot);
  } catch (error) {
    throw new Error(`${label}: ${error.message}`);
  }
  return absoluteRoot;
}

function repositoryPath(relativePath, root, label = relativePath) {
  assertRepositoryPath(relativePath, label);
  const absoluteRoot = repositoryRoot(root);
  const rootReal = fs.realpathSync(absoluteRoot);
  const absolutePath = path.resolve(absoluteRoot, ...relativePath.split("/"));
  const lexicalRelative = path.relative(absoluteRoot, absolutePath);
  if (lexicalRelative.startsWith("..") || path.isAbsolute(lexicalRelative)) {
    throw new Error(`${label} escapes the repository root`);
  }

  let current = absoluteRoot;
  for (const segment of relativePath.split("/")) {
    current = path.join(current, segment);
    let stat;
    try {
      stat = fs.lstatSync(current);
    } catch (error) {
      throw new Error(`${label}: ${error.message}`);
    }
    if (stat.isSymbolicLink()) throw new Error(`${label} contains a symbolic-link component`);
  }

  let realPath;
  try {
    realPath = fs.realpathSync(absolutePath);
  } catch (error) {
    throw new Error(`${label}: ${error.message}`);
  }
  const realRelative = path.relative(rootReal, realPath);
  if (realRelative.startsWith("..") || path.isAbsolute(realRelative)) {
    throw new Error(`${label} resolves outside the repository root`);
  }
  return absolutePath;
}

function fileDigest(relativePath, root = repoRoot) {
  const absolutePath = repositoryPath(relativePath, root);
  const stat = fs.lstatSync(absolutePath);
  if (!stat.isFile()) throw new Error(`${relativePath}: expected a regular file`);
  const bytes = fs.readFileSync(absolutePath);
  return { bytes: bytes.length, sha256: sha256Bytes(bytes) };
}

function fileMode(relativePath, root = repoRoot) {
  const stat = fs.lstatSync(repositoryPath(relativePath, root));
  if (!stat.isFile()) throw new Error(`${relativePath}: expected a regular file`);
  return (stat.mode & 0o111) === 0 ? "100644" : "100755";
}

function gitCommand(root, args, options = {}) {
  return execFileSync("git", ["-c", "core.filemode=true", "-C", repositoryRoot(root), ...args], options);
}

function gitWorktreeRoot(root) {
  try {
    return gitCommand(root, ["rev-parse", "--show-toplevel"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch (error) {
    if (error.status === 128) return null;
    throw error;
  }
}

function gitPathFromAbsolute(worktreeRoot, absolutePath) {
  const relativePath = path.relative(worktreeRoot, absolutePath);
  if (relativePath.startsWith("..") || path.isAbsolute(relativePath)) {
    throw new Error(`${absolutePath} escapes the git worktree`);
  }
  return relativePath.split(path.sep).join("/");
}

function trackedGitPaths(relativePath, root, absolutePath) {
  const worktreeRoot = gitWorktreeRoot(root);
  if (!worktreeRoot) return null;
  const prefix = gitPathFromAbsolute(worktreeRoot, absolutePath);
  let output;
  try {
    output = gitCommand(root, ["ls-files", "-z", "--stage", "--full-name", "--", prefix || "."]);
  } catch (error) {
    throw new Error(`${root}: cannot inspect tracked files under ${relativePath}: ${error.message}`);
  }
  const tracked = new Set();
  for (const record of output.toString("utf8").split("\0").filter(Boolean)) {
    const separator = record.indexOf("\t");
    if (separator < 0) throw new Error(`${root}: malformed tracked-file record for ${relativePath}`);
    const [mode] = record.slice(0, separator).split(" ");
    const trackedPath = record.slice(separator + 1);
    if (mode === "120000") tracked.add(trackedPath);
    else if (!/^100[0-7]{3}$/.test(mode)) {
      throw new Error(`${root}: unsupported tracked mode ${mode} for ${trackedPath}`);
    } else {
      tracked.add(trackedPath);
    }
  }
  return { worktreeRoot, tracked };
}

function isIgnoredGitPath(gitRoot, relativePath) {
  try {
    gitCommand(gitRoot, ["check-ignore", "--no-index", "-q", "--", relativePath]);
    return true;
  } catch (error) {
    if (error.status === 1) return false;
    throw new Error(`${gitRoot}: cannot inspect ignore state for ${relativePath}: ${error.message}`);
  }
}

function hasTrackedDescendant(trackedPaths, relativePath) {
  for (const trackedPath of trackedPaths) {
    if (trackedPath.startsWith(`${relativePath}/`)) return true;
  }
  return false;
}

function filesUnder(relativePath, root = repoRoot, includeAllFiles = false) {
  const absolutePath = repositoryPath(relativePath, root);
  const files = [];
  const tracked = trackedGitPaths(relativePath, root, absolutePath);

  function visit(directory, prefix) {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) =>
      a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
    )) {
      const entryPath = path.join(directory, entry.name);
      const entryRelativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
      const gitRelativePath = tracked
        ? gitPathFromAbsolute(tracked.worktreeRoot, entryPath)
        : null;
      const isTracked = tracked?.tracked.has(gitRelativePath) ?? false;
      const isIgnored = !isTracked && tracked && isIgnoredGitPath(tracked.worktreeRoot, gitRelativePath);
      if (isIgnored && !hasTrackedDescendant(tracked.tracked, gitRelativePath)) continue;
      if (entry.isSymbolicLink()) {
        throw new Error(`${relativePath}: symbolic links are not allowed (${entryRelativePath})`);
      }
      if (!includeAllFiles && (entry.name === "node_modules" || entry.name === "dist")) continue;
      if (entry.isDirectory()) visit(entryPath, entryRelativePath);
      else if (entry.isFile()) files.push(entryRelativePath);
      else throw new Error(`${relativePath}: unsupported directory entry ${entryRelativePath}`);
    }
  }

  const rootStat = fs.lstatSync(absolutePath);
  if (!rootStat.isDirectory()) throw new Error(`${relativePath}: expected a directory`);
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

function fileRecordsWithMode(relativePath, root = repoRoot, includeAllFiles = false) {
  return filesUnder(relativePath, root, includeAllFiles).map((relativeFile) => ({
    path: relativeFile,
    ...fileDigest(path.posix.join(relativePath, relativeFile), root),
    mode: fileMode(path.posix.join(relativePath, relativeFile), root),
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

function assertDispositionSnapshotPair(entry, label) {
  const expectedKind = DISPOSITION_SNAPSHOT_KINDS.get(entry.disposition);
  if (!expectedKind) {
    throw new Error(`${label}.disposition must be one of ${[...DISPOSITION_SNAPSHOT_KINDS.keys()].join(", ")}`);
  }
  if (!entry.snapshot || !SNAPSHOT_KINDS.has(entry.snapshot.kind)) {
    throw new Error(`${label}.snapshot.kind is unsupported; must be one of ${[...SNAPSHOT_KINDS].join(", ")}`);
  }
  if (entry.snapshot.kind !== expectedKind) {
    throw new Error(`${label} disposition ${entry.disposition} requires snapshot.kind ${expectedKind}`);
  }
}

function isStrictChild(child, parent) {
  return child !== parent && child.startsWith(`${parent}/`);
}

function isAllowedAdaptedFileOverlay(left, right) {
  const source = left.category === "source" ? left : right.category === "source" ? right : null;
  const packageEntry = left.category === "package" ? left : right.category === "package" ? right : null;
  return Boolean(
    source &&
    packageEntry &&
    source.kind === "blob" &&
    packageEntry.disposition === "retain-source" &&
    isStrictChild(source.path, packageEntry.path),
  );
}

function registerPath(entries, candidate, label, sideName) {
  for (const previous of entries) {
    if (!pathsOverlap(previous.path, candidate.path) || isAllowedAdaptedFileOverlay(previous, candidate)) continue;
    const overlapLabel = previous.category === "source" || candidate.category === "source"
      ? "exact root paths"
      : `${sideName} paths`;
    throw new Error(`${label} has overlapping ${overlapLabel}: ${previous.path} and ${candidate.path}`);
  }
  entries.push(candidate);
}

function packageRecord(name, relativePath, root, extra = {}) {
  const packageJson = readJsonAt(path.posix.join(relativePath, "package.json"), root, `${name} package.json`);
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
  assertDispositionSnapshotPair(app, "manifest.registry.app");
  if (!APP_DISPOSITIONS.has(app.disposition)) {
    throw new Error(`manifest.registry.app.disposition must be one of ${[...APP_DISPOSITIONS].join(", ")}`);
  }

  if (!Array.isArray(registry.packages) || registry.packages.length === 0) {
    throw new Error("manifest.registry.packages must be a non-empty array");
  }
  const names = new Set([app.name]);
  const upstreamEntries = [{ name: app.name, path: app.upstreamPath, category: "app", kind: "source", disposition: app.disposition }];
  const localEntries = [{ name: app.name, path: app.localPath, category: "app", kind: "source", disposition: app.disposition }];
  for (const [index, item] of registry.packages.entries()) {
    if (!item || typeof item !== "object") throw new Error(`manifest.registry.packages[${index}] must be an object`);
    for (const field of ["name", "upstreamPath", "localPath", "disposition"]) {
      if (typeof item[field] !== "string" || item[field].length === 0) {
        throw new Error(`manifest.registry.packages[${index}].${field} is required`);
      }
    }
    assertRepositoryPath(item.upstreamPath, `manifest.registry.packages[${index}].upstreamPath`);
    assertRepositoryPath(item.localPath, `manifest.registry.packages[${index}].localPath`);
    assertDispositionSnapshotPair(item, `manifest.registry.packages[${index}]`);
    if (!PACKAGE_DISPOSITIONS.has(item.disposition)) {
      throw new Error(`manifest.registry.packages[${index}].disposition must be one of ${[...PACKAGE_DISPOSITIONS].join(", ")}`);
    }
    if (names.has(item.name)) throw new Error(`manifest.registry has duplicate package name ${item.name}`);
    registerPath(upstreamEntries, {
      name: item.name,
      path: item.upstreamPath,
      category: "package",
      kind: "source",
      disposition: item.disposition,
    }, "manifest.registry", "upstream");
    registerPath(localEntries, {
      name: item.name,
      path: item.localPath,
      category: "package",
      kind: "source",
      disposition: item.disposition,
    }, "manifest.registry", "local");
    names.add(item.name);
  }

  if (registry.sources !== undefined && !Array.isArray(registry.sources)) {
    throw new Error("manifest.registry.sources must be an array");
  }
  for (const [index, item] of (registry.sources ?? []).entries()) {
    const label = `manifest.registry.sources[${index}]`;
    if (!item || typeof item !== "object") throw new Error(`${label} must be an object`);
    for (const field of ["name", "kind", "upstreamPath", "localPath", "disposition"]) {
      if (typeof item[field] !== "string" || item[field].length === 0) {
        throw new Error(`${label}.${field} is required`);
      }
    }
    if (!STANDALONE_SOURCE_KINDS.has(item.kind)) {
      throw new Error(`${label}.kind must be one of ${[...STANDALONE_SOURCE_KINDS].join(", ")}`);
    }
    if (item.disposition !== "exact-snapshot") {
      throw new Error(`${label}.disposition must be exact-snapshot for a standalone exact source`);
    }
    assertDispositionSnapshotPair(item, label);
    const materialization = item.materialization ?? "planned";
    if (!MATERIALIZATION_STATES.has(materialization)) {
      throw new Error(`${label}.materialization must be one of ${[...MATERIALIZATION_STATES].join(", ")}`);
    }
    assertRepositoryPath(item.upstreamPath, `${label}.upstreamPath`);
    assertRepositoryPath(item.localPath, `${label}.localPath`);
    if (names.has(item.name)) throw new Error(`manifest.registry has duplicate source name ${item.name}`);
    registerPath(upstreamEntries, {
      name: item.name,
      path: item.upstreamPath,
      category: "source",
      kind: item.kind,
      disposition: item.disposition,
    }, "manifest.registry", "upstream");
    registerPath(localEntries, {
      name: item.name,
      path: item.localPath,
      category: "source",
      kind: item.kind,
      disposition: item.disposition,
    }, "manifest.registry", "local");
    names.add(item.name);
  }
  return registry;
}

function pathsOverlap(left, right) {
  return left === right || left.startsWith(`${right}/`) || right.startsWith(`${left}/`);
}

function upstreamRecord(root, previous, registry) {
  if (!root) return previous;
  const appEntry = registry.app;
  const appPackageJson = readJsonAt(path.posix.join(appEntry.upstreamPath, "package.json"), root, "upstream app package.json");
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
    packages: registry.packages.map((entry) => packageRecord(entry.name, entry.upstreamPath, root, {
      gitTree: gitTreeId(entry.upstreamPath, root),
    })),
    sources: (registry.sources ?? []).map((entry) => standaloneRecord(entry, root, "upstream")),
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
    return gitCommand(root, ["rev-parse", "HEAD"], { encoding: "utf8" }).trim();
  } catch (error) {
    throw new Error(`${root}: cannot read checkout HEAD: ${error.message}`);
  }
}

function gitTreeId(relativePath, root) {
  const checkoutRoot = repositoryRoot(root);
  repositoryPath(relativePath, checkoutRoot);
  try {
    const type = gitCommand(checkoutRoot, ["cat-file", "-t", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
    if (type !== "tree") throw new Error(`expected tree, got ${type || "missing object"}`);
    return gitCommand(checkoutRoot, ["rev-parse", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
  } catch (error) {
    throw new Error(`${root}: cannot read git tree ${relativePath}: ${error.message}`);
  }
}

function gitBlobId(relativePath, root) {
  const checkoutRoot = repositoryRoot(root);
  repositoryPath(relativePath, checkoutRoot);
  try {
    const type = gitCommand(checkoutRoot, ["cat-file", "-t", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
    if (type !== "blob") throw new Error(`expected blob, got ${type || "missing object"}`);
    return gitCommand(checkoutRoot, ["rev-parse", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
  } catch (error) {
    throw new Error(`${root}: cannot read git blob ${relativePath}: ${error.message}`);
  }
}

function gitFileRecords(relativePath, root) {
  const checkoutRoot = repositoryRoot(root);
  repositoryPath(relativePath, checkoutRoot);
  let output;
  try {
    output = gitCommand(checkoutRoot, ["ls-tree", "-r", "-z", "HEAD", "--", relativePath]);
  } catch (error) {
    throw new Error(`${root}: cannot read git files ${relativePath}: ${error.message}`);
  }
  const prefix = `${relativePath}/`;
  return output.toString("utf8").split("\0").filter(Boolean).map((record) => {
    const separator = record.indexOf("\t");
    if (separator < 0) throw new Error(`${root}: malformed git tree record for ${relativePath}`);
    const [mode, type] = record.slice(0, separator).split(" ");
    const fullPath = record.slice(separator + 1);
    if (type !== "blob" || !/^100[0-7]{3}$/.test(mode) || !fullPath.startsWith(prefix)) {
      throw new Error(`${root}: unexpected git tree entry ${record}`);
    }
    return { path: fullPath.slice(prefix.length), mode };
  });
}

function gitFileRecord(relativePath, root) {
  const checkoutRoot = repositoryRoot(root);
  repositoryPath(relativePath, checkoutRoot);
  let output;
  try {
    output = gitCommand(checkoutRoot, ["ls-tree", "-z", "HEAD", "--", relativePath]);
  } catch (error) {
    throw new Error(`${root}: cannot read git file ${relativePath}: ${error.message}`);
  }
  const records = output.toString("utf8").split("\0").filter(Boolean);
  if (records.length !== 1) throw new Error(`${root}: expected one git file record for ${relativePath}`);
  const separator = records[0].indexOf("\t");
  if (separator < 0) throw new Error(`${root}: malformed git file record for ${relativePath}`);
  const [mode, type] = records[0].slice(0, separator).split(" ");
  const fullPath = records[0].slice(separator + 1);
  if (type !== "blob" || !/^100[0-7]{3}$/.test(mode) || fullPath !== relativePath) {
    throw new Error(`${root}: unexpected git file entry ${records[0]}`);
  }
  return { path: fullPath, mode };
}

function assertCleanGitWorktree(root, label) {
  const checkoutRoot = repositoryRoot(root, label);
  let output;
  try {
    output = gitCommand(checkoutRoot, ["status", "--porcelain=v1", "--untracked-files=all"], {
      encoding: "utf8",
    });
  } catch (error) {
    throw new Error(`${label}: cannot inspect worktree: ${error.message}`);
  }
  if (output.trim() !== "") throw new Error(`${label} must be a clean git worktree`);
}

function assertCleanGitPath(root, relativePath, label) {
  const checkoutRoot = repositoryRoot(root, label);
  repositoryPath(relativePath, checkoutRoot, label);
  let output;
  try {
    output = gitCommand(checkoutRoot, [
      "status", "--porcelain=v1", "--untracked-files=all", "--", relativePath,
    ], { encoding: "utf8" });
  } catch (error) {
    throw new Error(`${label}: cannot inspect worktree: ${error.message}`);
  }
  if (output.trim() !== "") throw new Error(`${label} must be clean; found ${output.trim()}`);
}

function buildManifest(existing, upstreamRoot) {
  if (existing?.format !== PROVENANCE_FORMAT) {
    throw new Error(`unsupported manifest format; expected ${PROVENANCE_FORMAT}`);
  }
  const registry = sourceRegistry(existing);
  const source = { ...DEFAULT_SOURCE, ...(existing?.source ?? {}) };
  const localPackages = registry.packages.map((entry) =>
    packageRecord(entry.name, entry.localPath, repoRoot, {
      upstreamPath: entry.upstreamPath,
      disposition: entry.disposition,
      gitTree: gitTreeId(entry.localPath, repoRoot),
    }),
  );
  const local = {
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
    sources: (registry.sources ?? []).map((entry) => {
      const materialization = entry.materialization ?? "planned";
      const record = {
        name: entry.name,
        kind: entry.kind,
        path: entry.localPath,
        disposition: entry.disposition,
        materialization,
      };
      const exists = repositoryPathExists(entry.localPath, repoRoot);
      assertMaterializationState(materialization, exists, entry.name);
      if (exists) return standaloneRecord(entry, repoRoot, "local");
      return record;
    }),
  };

  return {
    format: PROVENANCE_FORMAT,
    generator: "scripts/check-ui-provenance.mjs",
    registry,
    source,
    upstream: upstreamRecord(upstreamRoot, existing?.upstream, registry),
    local,
    contracts: contractRecord(existing?.contracts),
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

function snapshotContents(snapshot, label) {
  if (!snapshot || !snapshot.tree || !Array.isArray(snapshot.files)) {
    throw new Error(`${label} must contain a tree and file records`);
  }
  return {
    tree: snapshot.tree,
    files: snapshot.files,
  };
}

function assertSnapshotContentsEqual(actual, expected, label) {
  assertEqual(snapshotContents(actual, label), snapshotContents(expected, label), label);
}

function repositoryPathExists(relativePath, root = repoRoot) {
  assertRepositoryPath(relativePath, relativePath);
  const absoluteRoot = repositoryRoot(root);
  const absolutePath = path.resolve(absoluteRoot, ...relativePath.split("/"));
  try {
    fs.lstatSync(absolutePath);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}

function assertMaterializationState(materialization, exists, label) {
  if (materialization === "planned" && exists) {
    throw new Error(`${label}: planned standalone source already exists locally`);
  }
  if (materialization === "materialized" && !exists) {
    throw new Error(`${label}: materialized standalone source is missing locally`);
  }
}

function assertExactSourceEntry(entry, upstreamRoot, label, localRoot = repoRoot) {
  const upstreamTree = gitTreeId(entry.upstreamPath, upstreamRoot);
  const localTree = gitTreeId(entry.localPath, localRoot);
  assertEqual(localTree, upstreamTree, `${label} git tree`);
  assertCleanGitPath(upstreamRoot, entry.upstreamPath, `${label} upstream`);
  assertCleanGitPath(localRoot, entry.localPath, `${label} local`);

  const upstreamFiles = fileRecordsWithMode(entry.upstreamPath, upstreamRoot, true);
  const localFiles = fileRecordsWithMode(entry.localPath, localRoot, true);
  assertEqual(localFiles, upstreamFiles, `${label} files, bytes, hashes, and modes`);

  const upstreamGitFiles = gitFileRecords(entry.upstreamPath, upstreamRoot);
  const localGitFiles = gitFileRecords(entry.localPath, localRoot);
  assertEqual(localGitFiles, upstreamGitFiles, `${label} git file modes`);
}

function assertExactBlobEntry(entry, upstreamRoot, label, localRoot = repoRoot) {
  const upstreamBlob = gitBlobId(entry.upstreamPath, upstreamRoot);
  const localBlob = gitBlobId(entry.localPath, localRoot);
  assertEqual(localBlob, upstreamBlob, `${label} git blob`);
  assertCleanGitPath(upstreamRoot, entry.upstreamPath, `${label} upstream`);
  assertCleanGitPath(localRoot, entry.localPath, `${label} local`);
  assertEqual(
    { ...fileDigest(entry.localPath, localRoot), mode: fileMode(entry.localPath, localRoot) },
    { ...fileDigest(entry.upstreamPath, upstreamRoot), mode: fileMode(entry.upstreamPath, upstreamRoot) },
    `${label} bytes, hash, and mode`,
  );
  const upstreamGitFile = gitFileRecord(entry.upstreamPath, upstreamRoot);
  const localGitFile = gitFileRecord(entry.localPath, localRoot);
  assertEqual(localGitFile.mode, upstreamGitFile.mode, `${label} git file mode`);
}

function assertExactStandaloneEntry(entry, upstreamRoot, label, localRoot = repoRoot) {
  if (entry.kind === "source") return assertExactSourceEntry(entry, upstreamRoot, label, localRoot);
  if (entry.kind === "blob") return assertExactBlobEntry(entry, upstreamRoot, label, localRoot);
  throw new Error(`${label}: unsupported standalone source kind ${entry.kind}`);
}

function standaloneRecord(entry, root, side) {
  const relativePath = side === "upstream" ? entry.upstreamPath : entry.localPath;
  const record = {
    name: entry.name,
    kind: entry.kind,
    path: relativePath,
    disposition: entry.disposition,
    materialization: entry.materialization ?? "planned",
  };
  if (entry.kind === "blob") {
    record.gitBlob = gitBlobId(relativePath, root);
    record.file = {
      ...fileDigest(relativePath, root),
      mode: fileMode(relativePath, root),
    };
  } else {
    record.gitTree = gitTreeId(relativePath, root);
    record.snapshot = sourceSnapshot(relativePath, root);
  }
  return record;
}

function joinRepositoryPath(directory, relativeFile) {
  return path.posix.join(directory, relativeFile);
}

function changeFiles(relativePath, root, displayPath) {
  const files = new Map();
  for (const relativeFile of filesUnder(relativePath, root, true)) {
    const digest = fileDigest(path.posix.join(relativePath, relativeFile), root);
    files.set(relativeFile, {
      path: joinRepositoryPath(displayPath, relativeFile),
      sha256: digest.sha256,
      mode: fileMode(path.posix.join(relativePath, relativeFile), root),
    });
  }
  return files;
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
  const upstreamOnly = new Set([...upstreamFiles.keys()].filter((relativeFile) => !localFiles.has(relativeFile)));
  const localOnly = new Set([...localFiles.keys()].filter((relativeFile) => !upstreamFiles.has(relativeFile)));
  const patches = [];

  for (const relativeFile of [...upstreamFiles.keys()].sort()) {
    if (!localFiles.has(relativeFile)) continue;
    const upstream = upstreamFiles.get(relativeFile);
    const local = localFiles.get(relativeFile);
    if (upstream.sha256 !== local.sha256) patches.push(patchRecord("modify", upstream, local));
    else if (upstream.mode !== local.mode) patches.push(patchRecord("mode-change", upstream, local));
  }

  const addedByFingerprint = new Map();
  for (const relativeFile of [...localOnly].sort()) {
    const local = localFiles.get(relativeFile);
    const fingerprint = `${local.sha256}\0${local.mode}`;
    const entries = addedByFingerprint.get(fingerprint) ?? [];
    entries.push(relativeFile);
    addedByFingerprint.set(fingerprint, entries);
  }

  for (const relativeFile of [...upstreamOnly].sort()) {
    const upstream = upstreamFiles.get(relativeFile);
    const fingerprint = `${upstream.sha256}\0${upstream.mode}`;
    const candidates = addedByFingerprint.get(fingerprint) ?? [];
    if (candidates.length !== 1) continue;
    const localRelativeFile = candidates[0];
    addedByFingerprint.delete(fingerprint);
    localOnly.delete(localRelativeFile);
    upstreamOnly.delete(relativeFile);
    patches.push(patchRecord("rename", upstream, localFiles.get(localRelativeFile)));
  }

  for (const relativeFile of [...upstreamOnly].sort()) {
    patches.push(patchRecord("delete", upstreamFiles.get(relativeFile), absentPatchSide()));
  }
  for (const relativeFile of [...localOnly].sort()) {
    patches.push(patchRecord("add", absentPatchSide(), localFiles.get(relativeFile)));
  }

  return patches.sort((left, right) => patchSortKey(left).localeCompare(patchSortKey(right)));
}

function validatePatchPath(value, label, scopePath) {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${label} path is required`);
  if (GLOB_CHARACTERS.test(value)) throw new Error(`${label} uses a glob; patch ledger entries must name one file`);
  assertRepositoryPath(value, `${label} path`);
  assertRepositoryPath(scopePath, `${label} scope`);
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

function validatePatchLedger(manifest, ledger, upstreamRoot, localRoot = repoRoot) {
  const registry = sourceRegistry(manifest);
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
  const affectedFiles = new Set();
  for (const [index, affectedFile] of baseline.affectedUpstreamFiles.entries()) {
    validatePatchPath(affectedFile, `baseline.affectedUpstreamFiles[${index}]`, registry.app.upstreamPath);
    if (affectedFiles.has(affectedFile)) throw new Error(`app patch ledger has duplicate baseline affected file ${affectedFile}`);
    affectedFiles.add(affectedFile);
  }
  if (!Array.isArray(ledger.patches)) throw new Error("app patch ledger patches must be an array");
  if (registry.app.disposition === "source-port" && !upstreamRoot) {
    throw new Error("BB_SRC is required to verify a source-port app");
  }
  if (!upstreamRoot && ledger.patches.length !== 0) {
    throw new Error("BB_SRC is required to recompute non-empty app patch ledger entries");
  }
  if (upstreamRoot) {
    const actualPatches = computePatchDiff(
      upstreamRoot,
      localRoot,
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

function assertAttributionPreserved(previousPatches, nextPatches) {
  if (!Array.isArray(previousPatches) || !Array.isArray(nextPatches)) return;
  const previousByPath = new Map();
  for (const patch of previousPatches) {
    previousByPath.set(
      patch?.local?.path ?? patch?.upstream?.path,
      patch,
    );
  }
  for (const patch of nextPatches) {
    const filePath = patch?.local?.path ?? patch?.upstream?.path;
    const previous = previousByPath.get(filePath);
    if (!previous) continue;

    // A path whose recorded change is byte-identical to the previous revision
    // was not touched by this change, so its issue/owner/reason must not have
    // moved either. Regenerating the ledger (the documented --write flow)
    // rewrites every entry, and doing so silently relabels unrelated history —
    // which is how W-604's audit trail was once erased wholesale.
    const unchangedDiff =
      patch.kind === previous.kind &&
      JSON.stringify(patch.upstream) === JSON.stringify(previous.upstream) &&
      JSON.stringify(patch.local) === JSON.stringify(previous.local);
    if (!unchangedDiff) continue;

    const attribution = (entry) =>
      JSON.stringify({
        issue: entry.issue,
        owner: entry.owner,
        reason: entry.reason,
      });
    if (attribution(patch) !== attribution(previous)) {
      throw new Error(
        `app patch ledger re-attributed an unchanged diff: ${filePath} ` +
          `had ${attribution(previous)}, now ${attribution(patch)}. ` +
          `Preserve the original issue/owner/reason, or record a real change.`,
      );
    }
  }
}

/**
 * Compare the on-disk ledger with the ledger in `HEAD`.
 *
 * The working tree is the only place the previous revision is available without
 * a git call, and this check runs inside CI where `HEAD` is the commit under
 * test. A shallow clone or a detached checkout with no parent simply skips the
 * comparison rather than failing the build.
 */
function assertLedgerAttributionMatchesHead(ledger) {
  let previousRaw;
  try {
    previousRaw = execFileSync(
      "git",
      ["show", "HEAD:ui/app-patch-ledger.json"],
      { cwd: repoRoot, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
    );
  } catch {
    return;
  }
  let previous;
  try {
    previous = JSON.parse(previousRaw);
  } catch {
    return;
  }
  // The working tree is what is being validated; if it equals HEAD there is
  // nothing to compare.
  assertAttributionPreserved(previous.patches, ledger.patches);
}

/**
 * The recorded stage history of every `apps/app` path.
 *
 * `ui/app-patch-ledger.stages.json` names which issue introduced each path and
 * at which baseline. It exists because the ledger's per-entry attribution is
 * regenerated wholesale by tooling, and the hash-only validators downstream
 * cannot tell "this file is new" from "this file changed" — so a later stage
 * silently relabelled W-604's entire audit trail. Checking against a committed
 * record makes that visible in CI, where comparing to `HEAD` cannot (the tree
 * under test *is* `HEAD`).
 */
const patchStagesPath = path.join(repoRoot, "ui", "app-patch-ledger.stages.json");

function checkPatchStages(ledger) {
  let stages;
  try {
    stages = readJson(patchStagesPath);
  } catch {
    return;
  }
  const stageByPath = new Map();
  for (const stage of stages.stages ?? []) {
    for (const stagePath of stage.paths ?? []) {
      stageByPath.set(stagePath, stage.issue);
    }
  }

  for (const patch of ledger.patches ?? []) {
    const filePath = patch?.local?.path ?? patch?.upstream?.path;
    const introducingIssue = stageByPath.get(filePath);
    if (introducingIssue === undefined) continue;
    if (patch.issue === introducingIssue) continue;
    // A path whose introducing stage is still part of this entry's recorded
    // attribution is fine: `W-604+W-593` means both stages touched it.
    if (typeof patch.issue === "string" && patch.issue.includes(introducingIssue)) {
      continue;
    }
    throw new Error(
      `app patch ledger lost the stage that introduced ${filePath}: it was ` +
        `${introducingIssue}, but this entry credits ${JSON.stringify(patch.issue)}. ` +
        `Credit the introducing stage (e.g. "${introducingIssue}+<this issue>") ` +
        `rather than replacing it.`,
    );
  }
}

function checkPatchLedger(manifest, upstreamRoot) {
  const ledger = readJson(patchLedgerPath);
  assertLedgerAttributionMatchesHead(ledger);
  checkPatchStages(ledger);
  validatePatchLedger(manifest, ledger, upstreamRoot);
}

function checkLocalProductApp(manifest, upstreamRoot) {
  const registry = sourceRegistry(manifest);
  const productApp = manifest.local.productApp;
  if (!productApp) throw new Error("local.productApp is required");
  assertEqual(productApp.upstreamPath, registry.app.upstreamPath, "local product app upstream path");
  assertEqual(productApp.localPath, registry.app.localPath, "local product app path");
  assertEqual(productApp.disposition, registry.app.disposition, "local product app disposition");
  assertEqual(productApp.gitTree, gitTreeId(registry.app.localPath, repoRoot), "local product app git tree");
  checkSnapshot(registry.app.localPath, productApp.snapshot, "local product app snapshot");
  if (registry.app.snapshot.kind === "exact-snapshot") {
    assertEqual(productApp.gitTree, manifest.upstream.app.gitTree, "local/upstream product app git tree");
    assertSnapshotContentsEqual(productApp.snapshot, {
      tree: manifest.upstream.app.tree,
      files: manifest.upstream.app.files,
    }, "local product app vs upstream snapshot");
    assertCleanGitPath(repoRoot, registry.app.localPath, "local product app");
    if (upstreamRoot) assertExactSourceEntry(registry.app, upstreamRoot, "product app exact snapshot");
  } else {
    if (!upstreamRoot) throw new Error("BB_SRC is required to verify a source-port app");
    assertCleanGitPath(repoRoot, registry.app.localPath, "local source-port app");
  }
}

function checkLocalStandaloneSources(manifest, upstreamRoot, registry) {
  const entries = registry.sources ?? [];
  if (!Array.isArray(manifest.local.sources)) throw new Error("local sources must be an array");
  assertEqual(manifest.local.sources.map((item) => item.name), entries.map((item) => item.name), "local source names");
  for (const item of manifest.local.sources) {
    const entry = entries.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: standalone source is absent from the source registry`);
    assertEqual(item.kind, entry.kind, `${item.name} local source kind`);
    assertEqual(item.path, entry.localPath, `${item.name} local source path`);
    assertEqual(item.disposition, entry.disposition, `${item.name} local source disposition`);
    const materialization = entry.materialization ?? "planned";
    assertEqual(item.materialization, materialization, `${item.name} local source materialization`);
    const exists = repositoryPathExists(entry.localPath, repoRoot);
    assertMaterializationState(materialization, exists, item.name);
    if (!exists) continue;
    assertEqual(standaloneRecord(entry, repoRoot, "local"), item, `${item.name} local standalone source`);
    if (upstreamRoot) assertExactStandaloneEntry(entry, upstreamRoot, `${item.name} exact standalone source`);
  }
}

function checkUpstream(manifest, root) {
  const registry = sourceRegistry(manifest);
  assertCleanGitWorktree(root, "upstream checkout");
  const head = gitHead(root);
  assertEqual(head, manifest.source.commit, "upstream checkout HEAD");
  assertEqual(manifest.upstream.repository, manifest.source.repository, "upstream repository");
  assertEqual(manifest.upstream.commit, head, "upstream manifest commit");

  const appEntry = registry.app;
  const expectedApp = manifest.upstream.app;
  assertEqual(expectedApp.path, appEntry.upstreamPath, "upstream app path");
  const appPackageJson = readJsonAt(path.posix.join(expectedApp.path, "package.json"), root, "upstream app package.json");
  assertEqual(appPackageJson.name, appEntry.name, "upstream app package name");
  assertEqual(sourceSnapshot(expectedApp.path, root), {
    path: expectedApp.path,
    tree: expectedApp.tree,
    files: expectedApp.files,
  }, "upstream apps/app file snapshot");
  assertEqual(gitTreeId(expectedApp.path, root), expectedApp.gitTree, "upstream apps/app git tree");
  assertEqual(fileDigest(path.posix.join(expectedApp.path, "package.json"), root), expectedApp.packageJson, "upstream app package.json");
  assertEqual(dependencySnapshot(appPackageJson), expectedApp.dependencies, "upstream app dependencies");
  assertEqual(dependencyDigest(appPackageJson), expectedApp.dependencyDigest, "upstream app dependency digest");

  if (!Array.isArray(manifest.upstream.packages)) throw new Error("upstream packages must be an array");
  assertEqual(manifest.upstream.packages.map((item) => item.name), registry.packages.map((item) => item.name), "upstream package names");
  for (const item of manifest.upstream.packages) {
    const entry = registry.packages.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: package is absent from the source registry`);
    assertEqual(item.path, entry.upstreamPath, `${item.name} upstream path`);
    const packageJson = readJsonAt(path.posix.join(item.path, "package.json"), root, `${item.name} upstream package.json`);
    assertEqual(packageJson.name, item.name, `${item.name} upstream package name`);
    const actualGitTree = gitTreeId(item.path, root);
    if (item.gitTree !== undefined) assertEqual(actualGitTree, item.gitTree, `${item.name} upstream git tree`);
    assertEqual(treeDigest(item.path, root), item.tree, `${item.name} upstream tree`);
    assertEqual(fileDigest(path.posix.join(item.path, "package.json"), root), item.packageJson, `${item.name} upstream package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} upstream dependencies`);
      assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} upstream dependency digest`);
  }
  const sourceEntries = registry.sources ?? [];
  if (!Array.isArray(manifest.upstream.sources)) throw new Error("upstream sources must be an array");
  assertEqual(manifest.upstream.sources.map((item) => item.name), sourceEntries.map((item) => item.name), "upstream source names");
  for (const item of manifest.upstream.sources) {
    const entry = sourceEntries.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: standalone source is absent from the source registry`);
    assertEqual(standaloneRecord(entry, root, "upstream"), item, `${item.name} upstream standalone source`);
  }
  for (const entry of [registry.app, ...registry.packages]) {
    if (entry.snapshot.kind === "exact-snapshot") {
      assertExactSourceEntry(entry, root, `${entry.name} exact snapshot`);
    }
  }
}

function checkLocal(manifest, upstreamRoot) {
  if (manifest.format !== PROVENANCE_FORMAT) {
    throw new Error(`unsupported manifest format; expected ${PROVENANCE_FORMAT}`);
  }
  const registry = sourceRegistry(manifest);
  if (!/^[0-9a-f]{40}$/.test(manifest.source?.commit ?? "")) {
    throw new Error("source.commit must be a full 40-character git commit");
  }
  if (manifest.source.repository !== DEFAULT_SOURCE.repository) {
    throw new Error(`source.repository must be ${DEFAULT_SOURCE.repository}`);
  }
  assertEqual(manifest.source.commit, manifest.upstream.commit, "source/upstream commit");
  checkLocalProductApp(manifest, upstreamRoot);
  checkPatchLedger(manifest, upstreamRoot);

  if (!Array.isArray(manifest.local.packages)) throw new Error("local packages must be an array");
  assertEqual(manifest.local.packages.map((item) => item.name), registry.packages.map((item) => item.name), "local package names");
  for (const item of manifest.local.packages) {
    const entry = registry.packages.find((candidate) => candidate.name === item.name);
    if (!entry) throw new Error(`${item.name}: package is absent from the source registry`);
    assertEqual(item.path, entry.localPath, `${item.name} local path`);
    assertEqual(item.upstreamPath, entry.upstreamPath, `${item.name} local upstream path`);
    assertEqual(item.disposition, entry.disposition, `${item.name} local disposition`);
    const actualGitTree = gitTreeId(entry.localPath, repoRoot);
    if (typeof item.gitTree !== "string") throw new Error(`${item.name} local package gitTree is required`);
    assertEqual(actualGitTree, item.gitTree, `${item.name} local git tree`);
    const packageJson = readJsonAt(path.posix.join(item.path, "package.json"), repoRoot, `${item.name} local package.json`);
    assertEqual(packageJson.name, item.name, `${item.name} local package name`);
    assertEqual(treeDigest(item.path), item.tree, `${item.name} local/adapted tree`);
    assertEqual(fileDigest(path.posix.join(item.path, "package.json")), item.packageJson, `${item.name} local package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} local dependencies`);
    assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} local dependency digest`);
    if (entry.snapshot.kind === "exact-snapshot") {
      const upstream = manifest.upstream?.packages?.find((candidate) => candidate.name === entry.name);
      if (!upstream || typeof upstream.gitTree !== "string") {
        throw new Error(`${entry.name} exact snapshot requires an upstream gitTree and BB_SRC verification`);
      }
      assertEqual(gitTreeId(entry.localPath, repoRoot), upstream.gitTree, `${entry.name} exact git tree`);
      assertEqual(item.tree, upstream.tree, `${entry.name} exact tree`);
      assertCleanGitPath(repoRoot, entry.localPath, `${entry.name} local exact snapshot`);
    }
  }
  checkLocalStandaloneSources(manifest, upstreamRoot, registry);

  if (!Array.isArray(manifest.local.imports)) throw new Error("local imports must be an array");
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
  assertAttributionPreserved,
  assertExactBlobEntry,
  assertExactSourceEntry,
  assertExactStandaloneEntry,
  assertLedgerMatchesDiff,
  assertLedgerAttributionMatchesHead,
  assertMaterializationState,
  computePatchDiff,
  sourceRegistry,
  validatePatchLedger,
};

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
