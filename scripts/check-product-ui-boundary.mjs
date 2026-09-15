#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const appRoot = path.join(repoRoot, "apps", "app");
const packagePath = path.join(appRoot, "package.json");
const forbidden = [
  "@get-bb/plugin-sdk",
  "bb-plugin-",
  "plugin-sdk",
  "marketplace",
  "desktopBrowsers",
  "cli-skills",
  "host-daemon",
];

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function sourceFiles(directory) {
  const files = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) files.push(...sourceFiles(entryPath));
    else if (entry.isFile()) files.push(entryPath);
  }
  return files;
}

function fail(message) {
  console.error(`product UI boundary: ${message}`);
  process.exitCode = 1;
}

try {
  const packageJson = readJson(packagePath);
  const dependencyNames = [
    ...Object.keys(packageJson.dependencies ?? {}),
    ...Object.keys(packageJson.optionalDependencies ?? {}),
  ];
  for (const forbiddenName of forbidden) {
    if (dependencyNames.some((dependency) => dependency.includes(forbiddenName))) {
      throw new Error(`forbidden runtime dependency contains ${forbiddenName}`);
    }
  }

  const files = [packagePath, ...sourceFiles(path.join(appRoot, "src"))];
  for (const filePath of files) {
    const source = fs.readFileSync(filePath, "utf8");
    for (const forbiddenName of forbidden) {
      if (source.includes(forbiddenName)) {
        throw new Error(`${path.relative(repoRoot, filePath)} contains ${forbiddenName}`);
      }
    }
  }

  console.log("product UI boundary OK: no excluded runtime surface");
} catch (error) {
  fail(error.message);
}
