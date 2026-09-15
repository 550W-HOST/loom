import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { assertBatchCoverage, assertCleanAppStatus, assertSourceInventory, batchDetails, canonicalEdgeKey, classifyPath, extractCompilerImports, generatePlan, packageExportTarget, preProcessImportSpecifiers, resolveWithCompilerModule, scanCssImports, scanHtmlImports } from "./analyze-ui-port.mjs";

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

test("preProcessFile independently sees the same literal corpus imports", () => {
  const fixture = fs.readFileSync(path.join(fixtureRoot, "compiler-imports.ts"), "utf8");
  assert.deepEqual(preProcessImportSpecifiers(fixture), ["./multiline", "./re-export", "./types", "./lazy", "./conditional", "./required"]);
});

test("compiler module resolution honors a tsconfig path alias", () => {
  const containingFile = path.join(fixtureRoot, "alias-imports.ts");
  const resolved = resolveWithCompilerModule("@/alias-target", containingFile, {
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    baseUrl: fixtureRoot,
    paths: { "@/*": ["*"] },
    pathsBasePath: fixtureRoot,
  });
  assert.equal(resolved, path.join(fixtureRoot, "alias-target.ts"));
});

test("edge aggregation retains type-only and conditional semantics", () => {
  const base = { from: "src/a.ts", origin: "typescript", kind: "static", specifier: "./same", to: "src/same.ts" };
  assert.notEqual(canonicalEdgeKey({ ...base, typeOnly: false }), canonicalEdgeKey({ ...base, typeOnly: true }));
  assert.notEqual(canonicalEdgeKey({ ...base, conditional: false }), canonicalEdgeKey({ ...base, conditional: true }));
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
  const nodes = new Map(plan.nodes.map((node) => [node.path, node]));
  assert.equal(plan.nodes.length, 1437);
  assert.equal(plan.batchCoverage.missing.length, 0);
  assert.equal(plan.batchCoverage.duplicates.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.compilerSpecifiers, plan.graph.compilerImportConsistency.graphSpecifiers);
  assert.equal(plan.graph.compilerImportConsistency.preProcessFileParity.missing.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.preProcessFileParity.extra.length, 0);
  assert.equal(nodes.get("src/App.legacy-automation-routes.test.tsx").runtimeReachable, false);
  assert.equal(nodes.get("src/App.legacy-automation-routes.test.tsx").disposition, "verification-only");
  const edgeIndexes = Object.fromEntries(plan.graph.edgeFields.map((field, index) => [field, index]));
  assert.ok(plan.graph.edges.some((edge) => plan.graph.fileTable[edge[edgeIndexes.fromFileIndex]] === "src/App.tsx" && typeof edge[edgeIndexes.specifier] === "string" && edge[edgeIndexes.specifier].startsWith("@/") && typeof edge[edgeIndexes.toFileIndexOrPath] === "number"));
});

test("a newly tracked upstream source cannot silently become classified", () => {
  assert.throws(
    () => assertSourceInventory(Array.from({ length: 1437 }, (_, index) => `src/file-${index}.tsx`).concat("src/upstream-added.ts")),
    /unclassified source inventory/,
  );
});

test("classification priority and exact app status reject non-runtime surprises", () => {
  assert.equal(classifyPath("src/test/fixture.ts", [{ status: "unresolved-external" }], { runtime: true }).disposition, "verification-only");
  assert.equal(classifyPath("src/views/mobile-home-story-fixtures.tsx", [], { runtime: true }).disposition, "verification-only");
  assert.equal(classifyPath("src/components/only-story.tsx", [], { runtime: false, story: true }).reasonCode, "story-only-reachable");
  assert.throws(() => assertCleanAppStatus("?? apps/app/.w605-untracked-probe"), /staged, unstaged, or untracked/);
});

test("batch dependencies are complete and the batch graph has an explicit order", () => {
  const dependencies = Array.from({ length: 101 }, (_, index) => ({ from: "src/a.ts", kind: "static", specifier: `pkg-${index}`, package: `pkg-${index}`, status: "declared-external" }));
  const result = batchDetails([{ issue: "W-603", id: "W-603-01", files: ["src/a.ts"] }], dependencies, new Map([["src/a.ts", { disposition: "retain-verbatim" }]]), "W-603");
  assert.equal(result[0].dependencies.length, 101);
  const plan = generatePlan();
  assert.equal(plan.batchGraph.dependencyOrder.length, plan.batchGraph.stronglyConnectedComponents.length);
  assert.ok(plan.batches["W-603"].some((batch) => batch.dependencies.length > 80));
  assert.throws(() => assertBatchCoverage({ expected: 2, assigned: 1, missing: ["b"], duplicates: [] }), /batch partition is incomplete/);
});
