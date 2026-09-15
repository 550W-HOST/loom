import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { assertSourceInventory, extractCompilerImports, generatePlan, packageExportTarget, scanCssImports, scanHtmlImports } from "./analyze-ui-port.mjs";

const fixtureRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures");

test("compiler AST extraction handles multiline, re-export, import type, dynamic and require forms", () => {
  const fixture = fs.readFileSync(path.join(fixtureRoot, "compiler-imports.ts"), "utf8");
  const result = extractCompilerImports("compiler-imports.ts", fixture);
  assert.deepEqual(result.imports.map((item) => item.specifier), [
    "./multiline",
    "./re-export",
    "./types",
    "./lazy",
    "./conditional",
    "./required",
  ]);
  assert.deepEqual(result.blockers.map((item) => item.kind), ["dynamic-nonliteral", "require-nonliteral"]);
  assert.equal(result.imports.find((item) => item.kind === "import-type").typeOnly, true);
  assert.equal(result.imports.find((item) => item.specifier === "./conditional").conditional, true);
});

test("CSS scanner ignores comments and strings while retaining imports and URLs", () => {
  const fixture = fs.readFileSync(path.join(fixtureRoot, "styles.css"), "utf8");
  assert.deepEqual(scanCssImports(fixture).map((item) => item.specifier), ["./image.png", "./nested.css", "./theme.css"]);
});

test("HTML scanner ignores comments and script strings", () => {
  const fixture = fs.readFileSync(path.join(fixtureRoot, "page.html"), "utf8");
  assert.deepEqual(scanHtmlImports(fixture).map((item) => item.specifier), ["./page.css", "./image.png", "./small.png", "./large.png"]);
});

test("package exports select source before types/default and support subpath patterns", () => {
  assert.equal(packageExportTarget({ exports: { ".": { source: "./src/index.ts", types: "./dist/index.d.ts", default: "./dist/index.js" } } }), "./src/index.ts");
  assert.equal(packageExportTarget({ exports: { "./*": { source: "./src/*.tsx", default: "./dist/*.js" } } }, "./button"), "./src/button.tsx");
});

test("the real app corpus is complete, partitioned and compiler-consistent", () => {
  const plan = generatePlan();
  assert.equal(plan.nodes.length, 1437);
  assert.equal(plan.batchCoverage.missing.length, 0);
  assert.equal(plan.batchCoverage.duplicates.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.compilerSpecifiers, plan.graph.compilerImportConsistency.graphSpecifiers);
});

test("a newly tracked upstream source cannot silently become classified", () => {
  assert.throws(
    () => assertSourceInventory(Array.from({ length: 1437 }, (_, index) => `src/file-${index}.tsx`).concat("src/upstream-added.ts")),
    /unclassified source inventory/,
  );
});
