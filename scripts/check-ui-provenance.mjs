#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = path.join(repoRoot, "ui", "provenance.json");
const contractManifestPath = path.join(repoRoot, "contracts", "bb", "manifest.json");

const PACKAGE_SOURCES = [
  ["@bb/domain", "domain"],
  ["@bb/server-contract", "server-contract"],
  ["@bb/thread-view", "thread-view"],
  ["@bb/client-core", "client-core"],
  ["@bb/core-ui", "core-ui"],
  ["@bb/shared-ui", "shared-ui"],
  ["@bb/desktop-contract", "desktop-contract"],
];

const DEFAULT_SOURCE = {
  repository: "https://github.com/get-bb/bb",
  commit: "fa1f44ebe9e5676004b669e48c99b3c7606466b6",
  commitTitle: "Cut startup JavaScript by 81 KiB and restore 5% bundle headroom (#3476)",
};

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

function filesUnder(relativePath, root = repoRoot) {
  const absolutePath = path.join(root, relativePath);
  const files = [];

  function visit(directory, prefix) {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) =>
      a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
    )) {
      const entryPath = path.join(directory, entry.name);
      const entryRelativePath = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.name === "node_modules" || entry.name === "dist") continue;
      if (entry.isDirectory()) visit(entryPath, entryRelativePath);
      else if (entry.isFile()) files.push(entryRelativePath);
      else throw new Error(`${relativePath}: unsupported directory entry ${entryRelativePath}`);
    }
  }

  if (!fs.existsSync(absolutePath)) throw new Error(`${relativePath}: path does not exist`);
  visit(absolutePath, "");
  return files;
}

function treeDigest(relativePath, root = repoRoot) {
  const files = filesUnder(relativePath, root);
  const hash = crypto.createHash("sha256");
  for (const relativeFile of files) {
    const digest = fileDigest(path.join(relativePath, relativeFile), root);
    hash.update(`${relativeFile}\0${digest.sha256}\n`);
  }
  return { files: files.length, sha256: hash.digest("hex") };
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

function localAppRecord(previous) {
  const packageJson = readJson(path.join(repoRoot, "ui", "package.json"));
  return {
    upstreamPath: "apps/app",
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

function localProductAppRecord(previous) {
  const packageJson = readJson(path.join(repoRoot, "apps", "app", "package.json"));
  return {
    name: packageJson.name,
    upstreamPath: previous?.upstreamPath ?? "apps/app",
    upstreamDisposition: previous?.upstreamDisposition ?? "source-port-foundation",
    path: "apps/app",
    localSourcePath: "apps/app/src",
    localSourceDisposition: previous?.localSourceDisposition ?? "loom-product-shell",
    sourceTree: treeDigest("apps/app"),
    packageJson: fileDigest("apps/app/package.json"),
    dependencies: dependencySnapshot(packageJson),
    dependencyDigest: dependencyDigest(packageJson),
  };
}

function upstreamRecord(root, previous) {
  if (!root) return previous;
  const appPackageJson = readJson(path.join(root, "apps", "app", "package.json"));
  return {
    repository: DEFAULT_SOURCE.repository,
    commit: gitHead(root),
    app: {
      path: "apps/app",
      tree: treeDigest("apps/app", root),
      packageJson: fileDigest("apps/app/package.json", root),
      dependencies: dependencySnapshot(appPackageJson),
      dependencyDigest: dependencyDigest(appPackageJson),
    },
    packages: PACKAGE_SOURCES.map(([name, directory]) =>
      packageRecord(name, `packages/${directory}`, root),
    ),
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

function buildManifest(existing, upstreamRoot) {
  const source = { ...DEFAULT_SOURCE, ...(existing?.source ?? {}) };
  const localPackages = PACKAGE_SOURCES.map(([name, directory]) => {
    const previous = existing?.local?.packages?.find((item) => item.name === name);
    return packageRecord(name, `ui/packages/${directory}`, repoRoot, {
      upstreamPath: previous?.upstreamPath ?? `packages/${directory}`,
      disposition: previous?.disposition ?? "retain-source",
    });
  });
  const productApp = localProductAppRecord(existing?.local?.productApp);
  const local = {
    referenceApp: localAppRecord(existing?.local?.referenceApp ?? existing?.app),
    productApp,
    packages: localPackages,
    imports: [
      {
        package: productApp.name,
        upstreamPath: productApp.upstreamPath,
        localPath: productApp.path,
        disposition: productApp.upstreamDisposition,
      },
      ...localPackages.map((item) => ({
        package: item.name,
        upstreamPath: item.upstreamPath,
        localPath: item.path,
        disposition: item.disposition,
      })),
    ],
  };

  return {
    format: "loom.ui-provenance/v2",
    generator: "scripts/check-ui-provenance.mjs",
    source,
    upstream: upstreamRecord(upstreamRoot, existing?.upstream),
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

function checkUpstream(manifest, root) {
  const head = gitHead(root);
  assertEqual(head, manifest.source.commit, "upstream checkout HEAD");
  assertEqual(manifest.upstream.repository, manifest.source.repository, "upstream repository");
  assertEqual(manifest.upstream.commit, head, "upstream manifest commit");

  const expectedApp = manifest.upstream.app;
  assertEqual(expectedApp.path, "apps/app", "upstream app path");
  const appPackageJson = readJson(path.join(root, expectedApp.path, "package.json"));
  assertEqual(appPackageJson.name, "@bb/app", "upstream app package name");
  assertEqual(treeDigest(expectedApp.path, root), expectedApp.tree, "upstream apps/app tree");
  assertEqual(fileDigest(path.join(expectedApp.path, "package.json"), root), expectedApp.packageJson, "upstream apps/app package.json");
  assertEqual(dependencySnapshot(appPackageJson), expectedApp.dependencies, "upstream apps/app dependencies");
  assertEqual(dependencyDigest(appPackageJson), expectedApp.dependencyDigest, "upstream apps/app dependency digest");

  assertEqual(manifest.upstream.packages.map((item) => item.name), PACKAGE_SOURCES.map(([name]) => name), "upstream package names");
  for (const item of manifest.upstream.packages) {
    const expectedPath = `packages/${item.name.slice("@bb/".length)}`;
    assertEqual(item.path, expectedPath, `${item.name} upstream path`);
    const packageJson = readJson(path.join(root, item.path, "package.json"));
    assertEqual(packageJson.name, item.name, `${item.name} upstream package name`);
    assertEqual(treeDigest(item.path, root), item.tree, `${item.name} upstream tree`);
    assertEqual(fileDigest(path.join(item.path, "package.json"), root), item.packageJson, `${item.name} upstream package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} upstream dependencies`);
    assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} upstream dependency digest`);
  }
}

function checkLocal(manifest) {
  if (manifest.format !== "loom.ui-provenance/v2") throw new Error("unsupported manifest format");
  if (!/^[0-9a-f]{40}$/.test(manifest.source?.commit ?? "")) {
    throw new Error("source.commit must be a full 40-character git commit");
  }
  if (manifest.source.repository !== DEFAULT_SOURCE.repository) {
    throw new Error(`source.repository must be ${DEFAULT_SOURCE.repository}`);
  }
  assertEqual(manifest.source.commit, manifest.upstream.commit, "source/upstream commit");

  const referenceApp = manifest.local.referenceApp;
  const localAppPackageJson = readJson(path.join(repoRoot, "ui", "package.json"));
  assertEqual(treeDigest(referenceApp.localSourcePath), referenceApp.sourceTree, "local reference app source tree");
  assertEqual(fileDigest("ui/package.json"), referenceApp.packageJson, "local reference app package.json");
  assertEqual(dependencySnapshot(localAppPackageJson), referenceApp.dependencies, "local reference app dependencies");
  assertEqual(dependencyDigest(localAppPackageJson), referenceApp.dependencyDigest, "local reference app dependency digest");
  assertEqual(fileDigest(referenceApp.bundlePath), referenceApp.bundle, "local reference app bundle");

  const productApp = manifest.local.productApp;
  const productAppPackageJson = readJson(path.join(repoRoot, productApp.path, "package.json"));
  assertEqual(productAppPackageJson.name, productApp.name, "local product app package name");
  assertEqual(treeDigest(productApp.path), productApp.sourceTree, "local product app source tree");
  assertEqual(fileDigest(path.join(productApp.path, "package.json")), productApp.packageJson, "local product app package.json");
  assertEqual(dependencySnapshot(productAppPackageJson), productApp.dependencies, "local product app dependencies");
  assertEqual(dependencyDigest(productAppPackageJson), productApp.dependencyDigest, "local product app dependency digest");

  const expectedNames = PACKAGE_SOURCES.map(([name]) => name);
  assertEqual(manifest.local.packages.map((item) => item.name), expectedNames, "local package names");
  for (const item of manifest.local.packages) {
    const expectedPath = `ui/packages/${item.name.slice("@bb/".length)}`;
    assertEqual(item.path, expectedPath, `${item.name} local path`);
    const packageJson = readJson(path.join(repoRoot, item.path, "package.json"));
    assertEqual(packageJson.name, item.name, `${item.name} local package name`);
    assertEqual(treeDigest(item.path), item.tree, `${item.name} local/adapted tree`);
    assertEqual(fileDigest(path.join(item.path, "package.json")), item.packageJson, `${item.name} local package.json`);
    assertEqual(dependencySnapshot(packageJson), item.dependencies, `${item.name} local dependencies`);
    assertEqual(dependencyDigest(packageJson), item.dependencyDigest, `${item.name} local dependency digest`);
  }

  const expectedImports = [
    {
      package: productApp.name,
      upstreamPath: productApp.upstreamPath,
      localPath: productApp.path,
      disposition: productApp.upstreamDisposition,
    },
    ...manifest.local.packages.map((item) => ({
      package: item.name,
      upstreamPath: item.upstreamPath,
      localPath: item.path,
      disposition: item.disposition,
    })),
  ];
  assertEqual(manifest.local.imports, expectedImports, "local import manifest");
  assertEqual(manifest.local.imports.map((item) => item.package), [productApp.name, ...expectedNames], "import package names");
  for (let index = 0; index < expectedImports.length; index += 1) {
    assertEqual(manifest.local.imports[index], expectedImports[index], `import ${index}`);
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

try {
  const manifest = readJson(manifestPath);
  const upstreamRoot = parseUpstreamArgument();
  if (process.argv.includes("--write")) {
    const next = buildManifest(manifest, upstreamRoot);
    fs.writeFileSync(manifestPath, `${JSON.stringify(next, null, 2)}\n`);
    console.log(`wrote ${path.relative(repoRoot, manifestPath)}`);
    checkLocal(next);
    if (upstreamRoot) checkUpstream(next, upstreamRoot);
  } else {
    checkLocal(manifest);
    if (upstreamRoot) checkUpstream(manifest, upstreamRoot);
  }
  console.log(`ui provenance OK: bb ${manifest.source.commit}`);
} catch (error) {
  fail(error.message);
}
