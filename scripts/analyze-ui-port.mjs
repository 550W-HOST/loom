#!/usr/bin/env node

import assert from "node:assert/strict";
import fs from "node:fs";
import { builtinModules } from "node:module";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const planPath = path.join(repoRoot, "ui", "app-port-plan.json");
const reviewSummaryPath = path.join(repoRoot, "ui", "app-port-plan.summary.json");
const expectedAppTree = "8ba6eb6f3d2f67f8ba2703f1fd0c123f788c6d52";
const sourceRoot = "apps/app/src";

const SOURCE_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]);
const CSS_EXTENSIONS = new Set([".css"]);
const HTML_EXTENSIONS = new Set([".html", ".htm"]);
const ASSET_EXTENSIONS = new Set([".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".woff", ".woff2"]);
const NODE_BUILTIN_MODULES = new Set(builtinModules.map((name) => name.replace(/^node:/, "")));
const CONDITIONAL_KINDS = new Set([
  ts.SyntaxKind.IfStatement,
  ts.SyntaxKind.ConditionalExpression,
  ts.SyntaxKind.SwitchStatement,
  ts.SyntaxKind.BinaryExpression,
]);

const UNSUPPORTED_COMPOSITION_ROOTS = [
  "src/components/plugin/management",
  "src/components/plugin/browse-hero",
  "src/views/SkillsView.tsx",
  "src/components/tools/SkillDetailView.tsx",
  "src/components/tools/SkillsBrowse.tsx",
  "src/components/tools/SkillsCollection.tsx",
  "src/components/tools/SkillsLibrary.tsx",
  "src/components/tools/skill-taxonomy.ts",
  "src/hooks/cache-owners/skills-cache-effects.ts",
  "src/hooks/queries/skills-queries.ts",
  "src/lib/skills-registry.ts",
  "src/components/secondary-panel/BrowserFindBar.tsx",
  "src/components/secondary-panel/BrowserNewTabScreen.tsx",
  "src/components/secondary-panel/BrowserTabContent.tsx",
  "src/components/secondary-panel/BrowserTabDeck.tsx",
  "src/components/secondary-panel/browserViewVisibilityCoordinator.ts",
  "src/components/settings/BrowserImportDialog.tsx",
  "src/components/settings/BrowserSettingsSection.tsx",
  "src/components/settings/BrowserSourceIcon.tsx",
  "src/components/settings/browser-import-wizard.ts",
  "src/lib/browser-history.ts",
  "src/lib/browser-storage.ts",
  "src/lib/browser-url.ts",
  "src/lib/browser-view-bounds-sync.ts",
  "src/lib/in-app-browser-link-preference.ts",
  "src/hooks/useBrowserDimmingModal.ts",
  "src/lib/use-desktop-browser-reveal.ts",
  "src/components/plugin/PluginsOverview.tsx",
  "src/components/plugin/PluginSettings.tsx",
  "src/components/plugin/PluginSettingsSections.tsx",
  "src/hooks/cache-owners/plugin-cache-owner.ts",
  "src/hooks/queries/plugin-catalog-queries.ts",
  "src/hooks/queries/plugin-contribution-queries.ts",
  "src/hooks/queries/plugin-settings-queries.ts",
  "src/hooks/queries/plugin-client.ts",
  "src/components/settings/plugin-settings-entries.ts",
  "src/lib/command-palette/palette-plugin-actions.ts",
  "src/lib/command-palette/palette-plugin-page-actions.ts",
];

const ADAPTER_BOUNDARY_PATHS = new Set([
  "src/App.tsx",
  "src/components/layout/AppLayout.tsx",
  "src/components/promptbox/NewThreadComposer.tsx",
  "src/components/promptbox/NewThreadPromptBox.tsx",
  "src/components/plugin/AppFileExternalNavigationHost.tsx",
  "src/components/plugin/ComposerExtensionHost.tsx",
  "src/components/plugin/PluginAppOverlays.tsx",
  "src/components/plugin/PluginComposerActions.tsx",
  "src/components/plugin/PluginComposerBanners.tsx",
  "src/components/plugin/PluginNewThreadComposer.tsx",
  "src/components/plugin/PluginPendingInteractionComposer.tsx",
  "src/components/plugin/PluginHomepageSections.tsx",
  "src/components/plugin/PluginNavSidebarItems.tsx",
  "src/components/plugin/PluginSidebarFooterItems.tsx",
  "src/components/tools/PluginCapabilities.tsx",
  "src/components/tools/PluginDetail.tsx",
  "src/components/tools/plugin-detail-banner.tsx",
  "src/components/tools/plugin-detail-table.tsx",
  "src/components/sidebar/PluginThreadList.tsx",
  "src/components/sidebar/SidebarPluginAttentionGlyph.tsx",
  "src/components/thread/timeline/PluginTimelineRendererBody.tsx",
  "src/components/plugin/PluginProviderModelPicker.tsx",
  "src/components/plugin/PluginSlotMount.tsx",
  "src/components/plugin/plugin-composer-host.tsx",
  "src/components/plugin/plugin-context.ts",
  "src/components/plugin/plugin-execution-routing.ts",
  "src/components/plugin/plugin-page-panel-state.ts",
  "src/components/plugin/plugin-thread-panel-navigation.tsx",
  "src/hooks/usePluginFrontendBoot.ts",
  "src/lib/plugin-frontend-boot-schedule.ts",
  "src/lib/plugin-frontend-boot-state.ts",
  "src/lib/plugin-frontend-lazy.ts",
  "src/lib/plugin-frontend.ts",
  "src/lib/plugin-sdk-app-impl.tsx",
  "src/lib/plugin-sdk-hooks.ts",
  "src/lib/plugin-slots.ts",
  "src/lib/plugin-slot-resolvers.ts",
  "src/components/commands/CommandPalette.tsx",
]);

const AUTOMATION_PATHS = new Set([
  "src/App.tsx",
  "src/App.legacy-automation-routes.test.tsx",
  "src/components/tools/Automations.stories.tsx",
  "src/components/tools/automation-overview.test.tsx",
  "src/hooks/usePromptDraftStorage.ts",
  "src/lib/resource-edit-prompt.test.ts",
  "src/lib/resource-edit-prompt.ts",
  "src/lib/route-paths.ts",
]);

const TSCONFIG_BUILD_INPUTS = new Set([
  "src/types/ansi-to-html.d.ts",
  "src/types/bb-desktop.d.ts",
  "src/vite-env.d.ts",
]);

const EXPECTED_COMPILE_ONLY_LOCAL = [
  "src/components/pickers/model-picker-option.ts",
  "src/components/secondary-panel/secondaryPanelTab.ts",
  "src/components/showcase-hero/showcase-archetype.ts",
  "src/components/thread/timeline/types.ts",
  "src/components/ui/markdown-link.ts",
  "src/hooks/cache-effect-types.ts",
  "src/hooks/mutations/mutation-request-types.ts",
  "src/lib/command-palette/palette-action.ts",
  "src/lib/split-layout/types.ts",
  "src/lib/thread-secondary-panel.ts",
  "src/views/thread-detail/threadDetailMutationTypes.ts",
];

const PRESERVED_SURFACE_PREFIXES = [
  "src/components/layout/",
  "src/components/promptbox/",
  "src/components/thread/",
  "src/components/sidebar/",
  "src/components/secondary-panel/Thread",
  "src/views/RootCompose",
  "src/views/SplitWorkspaceRoute",
  "src/views/SettingsView",
  "src/views/ProjectDetailSettingsView",
  "src/views/MachineSettingsView",
  "src/hooks/queries/",
  "src/hooks/mutations/",
  "src/lib/route-",
  "src/lib/sdk",
  "src/lib/themes",
];

const PACKAGE_DECISION_OVERRIDES = new Map([
  ["@bb/tsconfig", ["adapter", "adapt-build-config-boundary", false]],
  ["@bb/config", ["adapter", "adapt-config-boundary", false]],
  ["@bb/fuzzy-match", ["copy", "copy-source-utility", true]],
  ["@bb/host-daemon-contract", ["adapter", "adapt-daemon-boundary", false]],
  ["@bb/mobile-bridge", ["adapter", "adapt-browser-capability-boundary", false]],
  ["@bb/sdk", ["adapter", "adapt-typed-client-boundary", false]],
  ["@bb/templates", ["adapter", "adapt-template-boundary", false]],
  ["@get-bb/plugin-sdk", ["adapter", "generic-plugin-sdk-boundary", false]],
  ["@bb/test-helpers", ["remove", "verification-only-helper", false]],
  ["bb-plugin-automations", ["adapter", "automations-native-port-W-599", false]],
]);

function posix(value) {
  return value.split(path.sep).join("/");
}

function relativeTo(root, absolute) {
  return posix(path.relative(root, absolute));
}

function isInside(root, absolute) {
  const relative = path.relative(root, absolute);
  return relative === "" || (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function packageName(specifier) {
  if (specifier.startsWith("@")) return specifier.split("/").slice(0, 2).join("/");
  return specifier.split("/")[0];
}

export function isNodeBuiltin(specifier) {
  return NODE_BUILTIN_MODULES.has(specifier.replace(/^node:/, ""));
}

function isPackageSpecifier(specifier) {
  return !specifier.startsWith(".") && !specifier.startsWith("/") && !specifier.startsWith("@/") && !isNodeBuiltin(specifier);
}

function trackedFiles(repo, relativePath) {
  try {
    return execFileSync("git", ["-C", repo, "ls-files", "--", relativePath], { encoding: "utf8" })
      .split(/\r?\n/)
      .filter(Boolean)
      .map((file) => posix(path.relative(relativePath === "apps/app" ? path.join(repo, "apps/app") : repo, path.join(repo, file))));
  } catch {
    const absolute = path.join(repo, relativePath);
    const result = [];
    function visit(directory, prefix) {
      for (const entry of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
        if (entry.name === "node_modules" || entry.name === "dist") continue;
        const entryPath = path.join(directory, entry.name);
        const entryRelative = prefix ? `${prefix}/${entry.name}` : entry.name;
        if (entry.isDirectory()) visit(entryPath, entryRelative);
        else if (entry.isFile()) result.push(entryRelative);
      }
    }
    visit(absolute, "");
    return result;
  }
}

function appTrackedFiles(repo) {
  try {
    return execFileSync("git", ["-C", repo, "ls-files", "--", "apps/app"], { encoding: "utf8" })
      .split(/\r?\n/)
      .filter(Boolean)
      .map((file) => posix(path.relative(path.join(repo, "apps/app"), path.join(repo, file))));
  } catch {
    return trackedFiles(repo, "apps/app");
  }
}

function gitTree(repo, relativePath) {
  try {
    return execFileSync("git", ["-C", repo, "rev-parse", `HEAD:${relativePath}`], { encoding: "utf8" }).trim();
  } catch {
    return null;
  }
}

function parseTsConfig(configPath, cwd) {
  const readResult = ts.readConfigFile(configPath, ts.sys.readFile);
  if (readResult.error) throw new Error(ts.flattenDiagnosticMessageText(readResult.error.messageText, "\n"));
  const configHost = {
    ...ts.sys,
    onUnRecoverableConfigFileDiagnostic: () => {},
  };
  const parsed = ts.parseJsonConfigFileContent(readResult.config, configHost, cwd, undefined, configPath);
  return {
    options: {
      ...parsed.options,
      allowJs: true,
      resolveJsonModule: true,
      module: parsed.options.module ?? ts.ModuleKind.ESNext,
      moduleResolution: parsed.options.moduleResolution ?? ts.ModuleResolutionKind.Bundler,
    },
    errors: parsed.errors,
  };
}

function scriptKind(filePath) {
  switch (path.extname(filePath).toLowerCase()) {
    case ".tsx": return ts.ScriptKind.TSX;
    case ".jsx": return ts.ScriptKind.JSX;
    case ".js":
    case ".mjs":
    case ".cjs": return ts.ScriptKind.JS;
    default: return ts.ScriptKind.TS;
  }
}

function stringLiteralValue(node) {
  return node && (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) ? node.text : null;
}

function declarationIsTypeOnly(node) {
  if (node.isTypeOnly) return true;
  const clause = node.importClause;
  if (!clause) return false;
  if (clause.isTypeOnly || clause.name || !clause.namedBindings) return Boolean(clause.isTypeOnly);
  if (ts.isNamespaceImport(clause.namedBindings)) return false;
  return clause.namedBindings.elements.length > 0 && clause.namedBindings.elements.every((element) => element.isTypeOnly === true);
}

function importTypeOnlyForm(node) {
  if (node.importClause?.isTypeOnly) return "import-clause";
  const namedBindings = node.importClause?.namedBindings;
  if (namedBindings && ts.isNamedImports(namedBindings) && namedBindings.elements.length > 0 && namedBindings.elements.every((element) => element.isTypeOnly === true)) return "named-specifiers";
  return null;
}

function exportDeclarationIsTypeOnly(node) {
  if (node.isTypeOnly) return true;
  return Boolean(node.exportClause) && ts.isNamedExports(node.exportClause) && node.exportClause.elements.length > 0 && node.exportClause.elements.every((element) => element.isTypeOnly === true);
}

function conditionalAncestor(ancestors) {
  return ancestors.some((ancestor) => CONDITIONAL_KINDS.has(ancestor.kind));
}

export function extractCompilerImports(filePath, text) {
  const sourceFile = ts.createSourceFile(filePath, text, ts.ScriptTarget.Latest, true, scriptKind(filePath));
  const imports = [];
  const blockers = [];
  const addImport = (node, specifier, kind, typeOnly, ancestors, typeOnlyForm = null) => {
    const conditional = conditionalAncestor(ancestors);
    imports.push({
      specifier,
      kind,
      typeOnly: Boolean(typeOnly),
      conditional,
      line: sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile)).line + 1,
      ...(typeOnlyForm ? { typeOnlyForm } : {}),
    });
  };
  const addDynamic = (node, kind, ancestors) => {
    const argument = node.arguments[0];
    const specifier = stringLiteralValue(argument);
    if (specifier !== null) {
      addImport(node, specifier, kind, false, ancestors);
      return;
    }
    blockers.push({
      kind: kind === "require" ? "require-nonliteral" : "dynamic-nonliteral",
      expression: argument ? argument.getText(sourceFile) : "",
      line: sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile)).line + 1,
      conditional: conditionalAncestor(ancestors),
    });
  };
  function visit(node, ancestors) {
    if (ts.isImportDeclaration(node)) {
      const specifier = stringLiteralValue(node.moduleSpecifier);
      if (specifier !== null) addImport(node, specifier, "static", declarationIsTypeOnly(node), ancestors, importTypeOnlyForm(node));
    } else if (ts.isExportDeclaration(node)) {
      const specifier = node.moduleSpecifier ? stringLiteralValue(node.moduleSpecifier) : null;
      if (specifier !== null) addImport(node, specifier, "re-export", exportDeclarationIsTypeOnly(node), ancestors);
    } else if (ts.isImportEqualsDeclaration(node)) {
      const reference = node.moduleReference;
      if (ts.isExternalModuleReference(reference)) {
        const specifier = stringLiteralValue(reference.expression);
        if (specifier !== null) addImport(node, specifier, "import-equals", false, ancestors);
      }
    } else if (ts.isImportTypeNode(node)) {
      const argument = node.argument;
      const specifier = ts.isLiteralTypeNode(argument) ? stringLiteralValue(argument.literal) : null;
      if (specifier !== null) addImport(node, specifier, "import-type", true, ancestors);
    } else if (ts.isCallExpression(node)) {
      if (node.expression.kind === ts.SyntaxKind.ImportKeyword) addDynamic(node, "dynamic", ancestors);
      else if (ts.isIdentifier(node.expression) && node.expression.text === "require") addDynamic(node, "require", ancestors);
    }
    ts.forEachChild(node, (child) => visit(child, [...ancestors, node]));
  }
  visit(sourceFile, []);
  return { imports, blockers };
}

export function preProcessImportSpecifiers(text) {
  const sourceFile = ts.createSourceFile("preprocess.ts", text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const ambientModuleSpans = [];
  function collectAmbientModules(node) {
    if (ts.isModuleDeclaration(node) && (node.flags & ts.NodeFlags.Ambient) !== 0) {
      ambientModuleSpans.push({ start: node.getStart(sourceFile), end: node.end });
    }
    ts.forEachChild(node, collectAmbientModules);
  }
  collectAmbientModules(sourceFile);
  return ts.preProcessFile(text, true, true).importedFiles
    .filter((entry) => !ambientModuleSpans.some((span) => entry.pos >= span.start && entry.pos < span.end))
    .map((entry) => entry.fileName);
}

export function resolveWithCompilerModule(specifier, containingFile, options, host = moduleResolutionHost()) {
  return ts.resolveModuleName(specifier, containingFile, options, host).resolvedModule?.resolvedFileName ?? null;
}

function skipCssTrivia(text, start) {
  let index = start;
  while (index < text.length) {
    if (/\s/.test(text[index])) {
      index += 1;
      continue;
    }
    if (text.startsWith("/*", index)) {
      const end = text.indexOf("*/", index + 2);
      index = end < 0 ? text.length : end + 2;
      continue;
    }
    break;
  }
  return index;
}

function readQuoted(text, start) {
  const quote = text[start];
  let value = "";
  for (let index = start + 1; index < text.length; index += 1) {
    if (text[index] === "\\" && index + 1 < text.length) {
      value += text[index + 1];
      index += 1;
    } else if (text[index] === quote) {
      return { value, end: index + 1 };
    } else value += text[index];
  }
  return { value, end: text.length };
}

function readCssUrl(text, start) {
  let index = skipCssTrivia(text, start);
  if (text[index] === "\"" || text[index] === "'") return readQuoted(text, index);
  const close = text.indexOf(")", index);
  const end = close < 0 ? text.length : close;
  return { value: text.slice(index, end).trim(), end: close < 0 ? end : end + 1 };
}

export function scanCssImports(text) {
  const imports = [];
  let index = 0;
  while (index < text.length) {
    if (text.startsWith("/*", index)) {
      const end = text.indexOf("*/", index + 2);
      index = end < 0 ? text.length : end + 2;
      continue;
    }
    if (text[index] === "\"" || text[index] === "'") {
      index = readQuoted(text, index).end;
      continue;
    }
    if (!/[A-Za-z_-]/.test(text[index])) {
      index += 1;
      continue;
    }
    const start = index;
    while (index < text.length && /[A-Za-z0-9_-]/.test(text[index])) index += 1;
    const identifier = text.slice(start, index).toLowerCase();
    if (identifier !== "url") {
      if (identifier === "@import") index += 1;
      continue;
    }
    const open = skipCssTrivia(text, index);
    if (text[open] !== "(") continue;
    const parsed = readCssUrl(text, open + 1);
    if (parsed.value) imports.push({ specifier: parsed.value, kind: "css-url" });
    index = parsed.end;
  }

  index = 0;
  while (index < text.length) {
    if (text.startsWith("/*", index)) {
      const end = text.indexOf("*/", index + 2);
      index = end < 0 ? text.length : end + 2;
      continue;
    }
    if (text[index] === "\"" || text[index] === "'") {
      index = readQuoted(text, index).end;
      continue;
    }
    if (text[index] !== "@") {
      index += 1;
      continue;
    }
    const start = index;
    index += 1;
    while (index < text.length && /[A-Za-z0-9_-]/.test(text[index])) index += 1;
    if (text.slice(start, index).toLowerCase() !== "@import") continue;
    index = skipCssTrivia(text, index);
    let parsed;
    let importUsesUrl = false;
    if (text[index] === "\"" || text[index] === "'") parsed = readQuoted(text, index);
    else if (text.slice(index, index + 3).toLowerCase() === "url") {
      const open = skipCssTrivia(text, index + 3);
      importUsesUrl = true;
      parsed = text[open] === "(" ? readCssUrl(text, open + 1) : null;
    }
    if (parsed?.value && !importUsesUrl) imports.push({ specifier: parsed.value, kind: "css-import" });
    index = parsed?.end ?? index + 1;
  }
  return imports;
}

function htmlAttributeImports(text) {
  const imports = [];
  let index = 0;
  while (index < text.length) {
    if (text.startsWith("<!--", index)) {
      const end = text.indexOf("-->", index + 4);
      index = end < 0 ? text.length : end + 3;
      continue;
    }
    if (text[index] !== "<") {
      index += 1;
      continue;
    }
    if (text.startsWith("<!", index) || text.startsWith("<?", index)) {
      const end = text.indexOf(">", index + 2);
      index = end < 0 ? text.length : end + 1;
      continue;
    }
    index += 1;
    while (index < text.length && /\s/.test(text[index])) index += 1;
    const tagStart = index;
    while (index < text.length && /[A-Za-z0-9:_-]/.test(text[index])) index += 1;
    const tagName = text.slice(tagStart, index).toLowerCase();
    while (index < text.length && text[index] !== ">") {
      while (index < text.length && /\s/.test(text[index])) index += 1;
      if (text[index] === ">") break;
      if (text[index] === "/") {
        index += 1;
        continue;
      }
      const nameStart = index;
      while (index < text.length && /[A-Za-z0-9:_-]/.test(text[index])) index += 1;
      const name = text.slice(nameStart, index).toLowerCase();
      while (index < text.length && /\s/.test(text[index])) index += 1;
      if (text[index] !== "=") {
        while (index < text.length && !/[\s>]/.test(text[index])) index += 1;
        continue;
      }
      index = skipCssTrivia(text, index + 1);
      let value = "";
      if (text[index] === "\"" || text[index] === "'") {
        const parsed = readQuoted(text, index);
        value = parsed.value;
        index = parsed.end;
      } else {
        const valueStart = index;
        while (index < text.length && !/[\s>]/.test(text[index])) index += 1;
        value = text.slice(valueStart, index);
      }
      if (name === "src" || name === "href" || name === "poster") {
        if (value) imports.push({ specifier: value, kind: `html-${name}` });
      } else if (name === "srcset") {
        for (const candidate of value.split(",")) {
          const specifier = candidate.trim().split(/\s+/)[0];
          if (specifier) imports.push({ specifier, kind: "html-srcset" });
        }
      }
    }
    if (text[index] === ">") {
      index += 1;
      if (tagName === "script" || tagName === "style") {
        const close = text.toLowerCase().indexOf(`</${tagName}`, index);
        index = close < 0 ? text.length : close;
      }
    } else index = text.length;
  }
  return imports;
}

export function scanHtmlImports(text) {
  return htmlAttributeImports(text);
}

function exportConditionTarget(value, conditions, subpath, wildcard = "") {
  if (value === null) return null;
  if (typeof value === "string") return value.replaceAll("*", wildcard);
  if (Array.isArray(value)) {
    for (const candidate of value) {
      const result = exportConditionTarget(candidate, conditions, subpath, wildcard);
      if (result) return result;
    }
    return null;
  }
  if (typeof value !== "object") return null;
  const keys = Object.keys(value);
  if (keys.some((key) => key.startsWith("."))) {
    const exact = value[subpath];
    if (exact !== undefined) return exportConditionTarget(exact, conditions, subpath, wildcard);
    const pattern = keys.find((key) => key.endsWith("*") && subpath.startsWith(key.slice(0, -1)));
    if (pattern) {
      const patternWildcard = subpath.slice(pattern.length - 1);
      return exportConditionTarget(value[pattern], conditions, subpath, patternWildcard);
    }
    return null;
  }
  for (const condition of conditions) {
    if (value[condition] !== undefined) {
      const result = exportConditionTarget(value[condition], conditions, subpath, wildcard);
      if (result) return result;
    }
  }
  if (value.default !== undefined) return exportConditionTarget(value.default, conditions, subpath, wildcard);
  return null;
}

export function packageExportTarget(packageJson, subpath = ".") {
  const exports = packageJson.exports;
  if (exports !== undefined) {
    return exportConditionTarget(exports, ["source", "types", "import", "require", "default"], subpath);
  }
  if (subpath !== ".") return `.${subpath}`;
  return packageJson.types ?? packageJson.module ?? packageJson.main ?? "./index.ts";
}

function workspacePackages(repo) {
  const result = new Map();
  const packagesRoot = path.join(repo, "ui", "packages");
  if (!fs.existsSync(packagesRoot)) return result;
  for (const entry of fs.readdirSync(packagesRoot, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    if (!entry.isDirectory()) continue;
    const root = path.join(packagesRoot, entry.name);
    const packagePath = path.join(root, "package.json");
    if (!fs.existsSync(packagePath)) continue;
    const packageJson = readJson(packagePath);
    if (typeof packageJson.name === "string") result.set(packageJson.name, { root, packageJson });
  }
  return result;
}

function moduleResolutionHost() {
  return {
    fileExists: ts.sys.fileExists,
    readFile: ts.sys.readFile,
    directoryExists: ts.sys.directoryExists,
    getDirectories: ts.sys.getDirectories,
    realpath: ts.sys.realpath,
    trace: () => {},
  };
}

function resolverFor(repo, app, options, packageMap, declared) {
  const host = moduleResolutionHost();
  const resolveWithTs = (specifier, containingFile, resolveOptions = options) => {
    const resolved = ts.resolveModuleName(specifier, containingFile, resolveOptions, host).resolvedModule;
    if (!resolved?.resolvedFileName) return null;
    return path.resolve(resolved.resolvedFileName);
  };
  const resolveWorkspace = (specifier) => {
    const name = packageName(specifier);
    const workspace = packageMap.get(name);
    if (!workspace) return null;
    const suffix = specifier.slice(name.length);
    const subpath = suffix ? `.${suffix}` : ".";
    const target = packageExportTarget(workspace.packageJson, subpath);
    if (!target || !target.startsWith(".")) return { status: "unresolved-workspace-export", package: name };
    const targetPath = path.resolve(workspace.root, target);
    const virtualImporter = path.join(workspace.root, ".loom-module-resolution.ts");
    const resolved = resolveWithTs(`./${relativeTo(workspace.root, targetPath)}`, virtualImporter, {
      ...options,
      baseUrl: workspace.root,
      paths: undefined,
      pathsBasePath: undefined,
    });
    if (resolved && fs.existsSync(resolved)) return { status: "resolved-workspace", path: relativeTo(app, resolved), package: name };
    if (fs.existsSync(targetPath)) return { status: "resolved-workspace", path: relativeTo(app, targetPath), package: name };
    return { status: "unresolved-workspace-export", package: name };
  };
  return (specifier, containingFile, typeOnly = false) => {
    if (isNodeBuiltin(specifier)) return { status: "builtin", package: specifier };
    const workspaceResult = isPackageSpecifier(specifier) ? resolveWorkspace(specifier) : null;
    if (workspaceResult) return workspaceResult;
    const resolved = resolveWithTs(specifier, containingFile);
    if (resolved) {
      if (isInside(app, resolved)) return { status: "resolved-local", path: relativeTo(app, resolved) };
      if (isInside(repo, resolved)) return { status: "resolved-repository", path: relativeTo(repo, resolved) };
      return { status: "resolved-external", package: packageName(specifier), path: resolved };
    }
    if (specifier.startsWith(".") || specifier.startsWith("/")) {
      const base = specifier.startsWith("/") ? path.join(app, specifier.slice(1)) : path.resolve(path.dirname(containingFile), specifier);
      for (const candidate of [base, ...[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".css", ".json", ".svg", ".png"].map((extension) => `${base}${extension}`), ...["index.ts", "index.tsx", "index.js", "index.jsx"].map((entry) => path.join(base, entry))]) {
        if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return { status: isInside(app, candidate) ? "resolved-local" : "resolved-repository", path: relativeTo(app, candidate) };
      }
      return { status: "unresolved-local" };
    }
    const name = packageName(specifier);
    const declaration = declared.get(name);
    if (typeOnly && !declaration && declared.has(`@types/${name}`)) return { status: "declared-type-package", package: name, typePackage: `@types/${name}` };
    if (declaration?.version?.startsWith("workspace:") && !packageMap.has(name)) return { status: "unresolved-workspace-package", package: name };
    if (declaration) return { status: "declared-external", package: name };
    return { status: "unresolved-external", package: name };
  };
}

function dependencyDeclarations(packageJson) {
  const result = new Map();
  for (const group of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
  for (const [name, version] of Object.entries(packageJson[group] ?? {})) result.set(name, { group, version });
  }
  return result;
}

function layerForPath(filePath) {
  if (filePath.includes(".stories.") || filePath.includes(".story.") || filePath === "src/views/mobile-home-story-fixtures.tsx") return "story";
  if (filePath.includes(".test.") || filePath.includes(".spec.") || filePath.startsWith("src/test/") || filePath.includes(".fixtures.")) return "test";
  if (filePath.startsWith(".ladle/") || filePath === "index.html" || !filePath.startsWith("src/")) return "build";
  return "runtime";
}

function isVerificationPath(filePath) {
  return filePath.includes(".test.") || filePath.includes(".spec.") || filePath.includes(".stories.") || filePath.includes(".story.") || filePath.startsWith("src/test/") || filePath.includes(".fixtures.") || filePath === "src/views/mobile-home-story-fixtures.tsx";
}

function underRoot(filePath, root) {
  return filePath === root || filePath.startsWith(`${root}/`);
}

function explicitUnsupportedRoot(filePath) {
  return UNSUPPORTED_COMPOSITION_ROOTS.find((root) => underRoot(filePath, root)) ?? null;
}

function surfaceTags(filePath, disposition) {
  const tags = new Set();
  if (disposition === "asset-build") tags.add("asset");
  if (disposition === "adapt-boundary") tags.add("boundary-adapter");
  if (TSCONFIG_BUILD_INPUTS.has(filePath)) tags.add("build-input");
  if (isVerificationPath(filePath)) tags.add(layerForPath(filePath) === "story" ? "story" : "test");
  if (AUTOMATION_PATHS.has(filePath) || filePath === "src/components/tools/Automations.stories.tsx") tags.add("automations");
  if (filePath === "src/App.tsx" || filePath.startsWith("src/views/RootCompose")) tags.add("runtime-main");
  if (filePath.startsWith("src/components/layout/") || filePath.startsWith("src/components/sidebar/")) tags.add("layout");
  if (filePath.startsWith("src/components/promptbox/")) tags.add("composer");
  if (filePath.startsWith("src/components/thread/") || filePath.startsWith("src/views/SplitWorkspaceRoute")) tags.add("thread-workspace");
  if (filePath.startsWith("src/views/Settings") || filePath.startsWith("src/views/ProjectDetailSettings") || filePath.startsWith("src/views/MachineSettings")) tags.add("settings");
  if (ADAPTER_BOUNDARY_PATHS.has(filePath)) tags.add("boundary-adapter");
  const unsupported = explicitUnsupportedRoot(filePath);
  if (unsupported?.includes("plugin")) tags.add("unsupported-plugin-marketplace");
  if (unsupported?.includes("skill")) tags.add("unsupported-skills");
  if (unsupported?.includes("Browser") || unsupported?.includes("browser") || unsupported?.includes("in-app")) tags.add("unsupported-desktop-browsers");
  return [...tags].sort();
}

export function classifyPath(filePath, fileEdges = [], reachability = { runtime: true, test: false, story: false, build: false }) {
  const extension = path.extname(filePath).toLowerCase();
  const compileReachable = reachability.compile ?? reachability.runtime;
  if (isVerificationPath(filePath)) {
    return { disposition: "verification-only", reasonCode: "test-or-story-root", preserveStructure: false };
  }
  if (CSS_EXTENSIONS.has(extension) || ASSET_EXTENSIONS.has(extension) || extension === ".md") {
    return { disposition: "asset-build", reasonCode: "static-asset-or-style-input", preserveStructure: false };
  }
  const unsupported = explicitUnsupportedRoot(filePath);
  if (unsupported) {
    return { disposition: "delete-unsupported", reasonCode: `unsupported-composition:${unsupported}`, preserveStructure: false };
  }
  if (TSCONFIG_BUILD_INPUTS.has(filePath)) {
    return { disposition: "retain-verbatim", reasonCode: "tsconfig-ambient-build-input", preserveStructure: false };
  }
  if (!compileReachable) {
    return {
      disposition: "verification-only",
      reasonCode: reachability.story ? "story-only-reachable" : reachability.test ? "test-only-reachable" : reachability.build ? "build-only-reachable" : "unreachable-source-review",
      preserveStructure: false,
    };
  }
  const unresolvedBoundary = fileEdges.find((edge) =>
    (!(edge.typeOnly && edge.package === "@get-bb/plugin-sdk") && (
      edge.status === "unresolved-workspace-package" ||
      edge.status === "unresolved-workspace-export" ||
      edge.status === "unresolved-local" ||
      edge.status === "non-literal" ||
      edge.status === "unresolved-external"
    )) ||
    (edge.package === "@get-bb/plugin-sdk" && !edge.typeOnly),
  );
  if (unresolvedBoundary) {
    return {
      disposition: "adapt-boundary",
      reasonCode: unresolvedBoundary.package === "@get-bb/plugin-sdk" ? "generic-plugin-sdk-boundary" : "compiler-unresolved-boundary",
      preserveStructure: true,
    };
  }
  if (ADAPTER_BOUNDARY_PATHS.has(filePath)) {
    return { disposition: "adapt-boundary", reasonCode: filePath === "src/lib/plugin-slots.ts" ? "plugin-slot-composition-consumer" : "loom-boundary-preserves-composition", preserveStructure: true };
  }
  if (PRESERVED_SURFACE_PREFIXES.some((prefix) => filePath.startsWith(prefix)) || filePath === "src/main.tsx") {
    return { disposition: "retain-verbatim", reasonCode: "preserved-product-surface", preserveStructure: false };
  }
  return { disposition: "retain-verbatim", reasonCode: "runtime-graph-node", preserveStructure: false };
}

export function canonicalEdgeKey(edge) {
  return [edge.from, edge.origin, edge.kind, edge.specifier ?? "<non-literal>", edge.to ?? "<unresolved>", Boolean(edge.typeOnly), Boolean(edge.conditional)].join("\u0000");
}

function walkReachability(roots, edgeMap) {
  const distance = new Map();
  const witness = new Map();
  const queue = [];
  for (const root of roots) {
    distance.set(root, 0);
    witness.set(root, [root]);
    queue.push(root);
  }
  for (let index = 0; index < queue.length; index += 1) {
    const current = queue[index];
    for (const edge of edgeMap.get(current) ?? []) {
      if (!edge.to || distance.has(edge.to)) continue;
      distance.set(edge.to, distance.get(current) + 1);
      witness.set(edge.to, [...witness.get(current), edge.to]);
      queue.push(edge.to);
    }
  }
  return { distance, witness };
}

function buildPackageDecisions(appPackageJson, declarations, actualImports, packageMap) {
  const names = new Set([...declarations.keys(), ...actualImports.keys()]);
  return [...names].sort().map((name) => {
    const declaration = declarations.get(name);
    const actual = actualImports.get(name) ?? { total: 0, runtime: 0, runtimeValue: 0, runtimeType: 0, test: 0, story: 0, verificationValue: 0, verificationType: 0 };
    const override = PACKAGE_DECISION_OVERRIDES.get(name);
    const typePackage = !declaration && actual.runtimeType > 0 && declarations.has(`@types/${name}`) ? `@types/${name}` : null;
    const typeOnlyRuntime = actual.runtime > 0 && actual.runtimeValue === 0;
    const [decision, reasonCode, runtimeAllowed] = override ?? (typePackage
      ? ["reuse", `declared-type-package:${typePackage}`, false]
      : packageMap.has(name)
        ? ["reuse", "existing-workspace-source", true]
        : ["reuse", "declared-external-package", true]);
    const effectiveRuntimeAllowed = override ? runtimeAllowed : typeOnlyRuntime ? false : actual.runtimeValue > 0;
    return {
      name,
      declaredAs: declaration?.group ?? null,
      declaredVersion: declaration?.version ?? null,
      workspace: declaration?.version?.startsWith("workspace:") ?? false,
      sourcePath: packageMap.get(name) ? relativeTo(repoRoot, packageMap.get(name).root) : null,
      actualImports: actual.total,
      runtimeImports: actual.runtime,
      runtimeValueImports: actual.runtimeValue,
      runtimeTypeImports: actual.runtimeType,
      verificationImports: actual.test + actual.story,
      verificationValueImports: actual.verificationValue,
      verificationTypeImports: actual.verificationType,
      ...(typePackage ? { typePackage } : {}),
      decision,
      disposition: decision === "adapter" ? "adapt-boundary" : decision === "remove" ? "delete-unsupported" : decision === "copy" ? "copy-source" : decision === "reuse" ? "reuse-source" : decision,
      reasonCode,
      runtimeAllowed: effectiveRuntimeAllowed,
    };
  });
}

function makeBlocker(edge, layer, category, detail = null) {
  return {
    source: edge.from,
    layer,
    kind: edge.kind,
    specifier: edge.specifier,
    category,
    ...(detail ? { detail } : {}),
  };
}

function batchFiles(paths, issue, size = 120) {
  const batches = [];
  for (let index = 0; index < paths.length; index += size) {
    batches.push({ issue, id: `${issue}-${String(batches.length + 1).padStart(2, "0")}`, files: paths.slice(index, index + size) });
  }
  return batches;
}

export function batchDetails(rawBatches, edges, nodeByPath, issue, globalBatchByFile = null) {
  const batchByFile = globalBatchByFile ?? new Map(rawBatches.flatMap((batch) => batch.files.map((file) => [file, batch])));
  return rawBatches.map((batch) => {
    const fileSet = new Set(batch.files);
    const dependencies = new Set();
    const dependsOnBatchIds = new Set();
    for (const edge of edges) {
      if (!fileSet.has(edge.from)) continue;
      if (edge.to && !fileSet.has(edge.to)) {
        dependencies.add(edge.to);
        const dependencyBatch = batchByFile.get(edge.to);
        if (dependencyBatch && dependencyBatch.id !== batch.id) dependsOnBatchIds.add(dependencyBatch.id);
      }
      if (edge.package) dependencies.add(`package:${edge.package}`);
      if (!edge.to && !edge.package && edge.status !== "resolved-local" && edge.specifier) dependencies.add(`specifier:${edge.specifier}`);
    }
    const cutPoints = batch.files.filter((file) => explicitUnsupportedRoot(file) || ADAPTER_BOUNDARY_PATHS.has(file));
    if (cutPoints.length === 0) cutPoints.push(issue === "W-603" ? "preserve-product-composition" : "unsupported-composition-policy");
    return {
      ...batch,
      kind: "reviewChunk",
      executable: false,
      dependencies: [...dependencies].sort(),
      dependsOnBatchIds: [...dependsOnBatchIds].sort(),
      cutPoints: [...new Set(cutPoints)].sort(),
      verificationCommands: [
        "pnpm run port-plan:check",
        "pnpm run provenance:check",
        "pnpm --filter @loom/ui run typecheck",
      ],
      dispositions: [...new Set(batch.files.map((file) => nodeByPath.get(file)?.disposition).filter(Boolean))].sort(),
    };
  });
}

function batchDependencyGraph(batchGroups) {
  const batches = Object.values(batchGroups).flat();
  const dependencies = new Map(batches.map((batch) => [batch.id, new Set(batch.dependsOnBatchIds)]));
  const indexById = new Map();
  const lowLinkById = new Map();
  const stack = [];
  const onStack = new Set();
  const components = [];
  let nextIndex = 0;
  function visit(id) {
    indexById.set(id, nextIndex);
    lowLinkById.set(id, nextIndex);
    nextIndex += 1;
    stack.push(id);
    onStack.add(id);
    for (const dependency of dependencies.get(id) ?? []) {
      if (!indexById.has(dependency)) {
        visit(dependency);
        lowLinkById.set(id, Math.min(lowLinkById.get(id), lowLinkById.get(dependency)));
      } else if (onStack.has(dependency)) {
        lowLinkById.set(id, Math.min(lowLinkById.get(id), indexById.get(dependency)));
      }
    }
    if (lowLinkById.get(id) !== indexById.get(id)) return;
    const component = [];
    let member;
    do {
      member = stack.pop();
      onStack.delete(member);
      component.push(member);
    } while (member !== id);
    components.push(component.sort());
  }
  for (const batch of batches) if (!indexById.has(batch.id)) visit(batch.id);
  components.sort((left, right) => left[0].localeCompare(right[0]));
  const componentOf = new Map(components.flatMap((component, componentIndex) => component.map((id) => [id, componentIndex])));
  const componentDependencies = components.map(() => new Set());
  for (const [id, ids] of dependencies) {
    const component = componentOf.get(id);
    for (const dependency of ids) {
      const dependencyComponent = componentOf.get(dependency);
      if (dependencyComponent !== undefined && dependencyComponent !== component) componentDependencies[component].add(dependencyComponent);
    }
  }
  const pending = componentDependencies.map((ids) => ids.size);
  const dependents = components.map(() => new Set());
  for (const [component, ids] of componentDependencies.entries()) for (const dependency of ids) dependents[dependency].add(component);
  const ready = pending.map((count, component) => count === 0 ? component : null).filter((component) => component !== null);
  const dependencyOrder = [];
  while (ready.length) {
    const component = ready.shift();
    dependencyOrder.push(component);
    for (const dependent of dependents[component]) {
      pending[dependent] -= 1;
      if (pending[dependent] === 0) ready.push(dependent);
    }
  }
  if (dependencyOrder.length !== components.length) throw new Error("batch dependency graph condensation is not acyclic");
  return {
    dependsOnBatchIds: Object.fromEntries([...dependencies.entries()].map(([id, ids]) => [id, [...ids].sort()])),
    stronglyConnectedComponents: components.map((ids, component) => ({ component, batchIds: ids, cyclic: ids.length > 1 || dependencies.get(ids[0])?.has(ids[0]) === true })),
    dependencyOrder,
  };
}

const EDGE_FIELDS = ["fromFileIndex", "kind", "specifier", "toFileIndexOrPath", "status", "typeOnly", "conditional", "package", "occurrences"];

function compactEdge(edge, fileIndex) {
  return [
    fileIndex.get(edge.from) ?? edge.from,
    edge.kind,
    edge.specifier,
    edge.to ? (fileIndex.get(edge.to) ?? edge.to) : null,
    edge.status ?? null,
    edge.typeOnly ? 1 : null,
    edge.conditional ? 1 : null,
    edge.package ?? null,
    edge.occurrences > 1 ? edge.occurrences : null,
  ];
}

function exactSnapshotCheck(repo, sourceFiles) {
  const tree = gitTree(repo, "apps/app");
  if (tree !== expectedAppTree) throw new Error(`apps/app git tree changed: expected ${expectedAppTree}, got ${tree}`);
  if (sourceFiles.length !== 1437) throw new Error(`apps/app/src tracked file count changed: expected 1437, got ${sourceFiles.length}`);
  const status = execFileSync("git", ["-C", repo, "status", "--porcelain=v1", "--untracked-files=all", "--", "apps/app"], { encoding: "utf8" }).trim();
  assertCleanAppStatus(status);
}

export function assertCleanAppStatus(status) {
  if (status.trim()) throw new Error(`apps/app has staged, unstaged, or untracked changes: ${status.trim().split(/\r?\n/)[0]}`);
}

export function assertSourceInventory(sourceFiles) {
  const unique = new Set(sourceFiles);
  if (sourceFiles.length !== 1437 || unique.size !== sourceFiles.length) {
    throw new Error(`unclassified source inventory: expected 1437 unique tracked files, got ${sourceFiles.length} (${unique.size} unique)`);
  }
}

export function assertBatchCoverage(batchCoverage) {
  if (batchCoverage.missing.length || batchCoverage.duplicates.length || batchCoverage.assigned !== batchCoverage.expected || batchCoverage.missingCrossStageEdges || batchCoverage.missingCrossIssueBatchDependencies) throw new Error("W-603/W-604 batch partition is incomplete");
}

export function analyzeApp({ repo = repoRoot, app = path.join(repo, "apps", "app") } = {}) {
  const appPackageJson = readJson(path.join(app, "package.json"));
  const declarations = dependencyDeclarations(appPackageJson);
  const packageMap = workspacePackages(repo);
  const config = parseTsConfig(path.join(app, "tsconfig.json"), app);
  const allFiles = appTrackedFiles(repo);
  const sourceFiles = allFiles.filter((file) => file.startsWith("src/"));
  assertSourceInventory(sourceFiles);
  const resolver = resolverFor(repo, app, config.options, packageMap, declarations);
  const edges = [];
  const compilerRecords = [];
  const preProcessRecords = [];
  const blockers = [];
  const actualImports = new Map();
  const compileEdgeMap = new Map();
  const runtimeEdgeMap = new Map();
  const edgeIndex = new Map();
  const addEdge = (from, extracted, origin) => {
    const resolution = extracted.specifier === null
      ? { status: "non-literal" }
      : resolver(extracted.specifier, path.join(app, from), Boolean(extracted.typeOnly));
    const edge = {
      from,
      kind: extracted.kind,
      specifier: extracted.specifier,
      ...(extracted.typeOnly ? { typeOnly: true } : {}),
      ...(extracted.conditional ? { conditional: true } : {}),
      ...(resolution.path ? { to: resolution.path } : {}),
      ...(resolution.package ? { package: resolution.package } : {}),
    };
    Object.defineProperty(edge, "origin", { value: origin, enumerable: false });
    Object.defineProperty(edge, "status", {
      value: resolution.status,
      enumerable: resolution.status !== "resolved-local",
    });
    const key = canonicalEdgeKey(edge);
    const existingIndex = edgeIndex.get(key);
    if (existingIndex !== undefined) {
      const existing = edges[existingIndex];
      existing.occurrences = (existing.occurrences ?? 1) + 1;
      return existing;
    }
    edgeIndex.set(key, edges.length);
    edges.push(edge);
    if (edge.to && edge.status === "resolved-local") {
      if (!compileEdgeMap.has(from)) compileEdgeMap.set(from, []);
      compileEdgeMap.get(from).push(edge);
      if (!edge.typeOnly) {
        if (!runtimeEdgeMap.has(from)) runtimeEdgeMap.set(from, []);
        runtimeEdgeMap.get(from).push(edge);
      }
    }
    return edge;
  };

  for (const file of allFiles) {
    const extension = path.extname(file).toLowerCase();
    const absolute = path.join(app, file);
    if (!fs.existsSync(absolute) || !fs.statSync(absolute).isFile()) continue;
    const text = fs.readFileSync(absolute, "utf8");
    if (SOURCE_EXTENSIONS.has(extension)) {
      const extracted = extractCompilerImports(absolute, text);
      preProcessRecords.push(...preProcessImportSpecifiers(text).map((specifier) => ({ from: file, specifier })));
      for (const record of extracted.imports) {
        const edge = addEdge(file, { ...record, specifier: record.specifier }, "typescript");
        compilerRecords.push({ from: file, kind: record.kind, specifier: record.specifier, typeOnly: Boolean(record.typeOnly), typeOnlyForm: record.typeOnlyForm ?? null, conditional: Boolean(record.conditional), line: record.line });
        if (isPackageSpecifier(record.specifier)) {
          const name = packageName(record.specifier);
          const actual = actualImports.get(name) ?? { total: 0, runtime: 0, runtimeValue: 0, runtimeType: 0, test: 0, story: 0, verificationValue: 0, verificationType: 0 };
          actual.total += 1;
          const layer = layerForPath(file);
          if (layer === "runtime") {
            actual.runtime += 1;
            if (record.typeOnly) actual.runtimeType += 1;
            else actual.runtimeValue += 1;
          }
          if (layer === "test") {
            actual.test += 1;
            if (record.typeOnly) actual.verificationType += 1;
            else actual.verificationValue += 1;
          }
          if (layer === "story") {
            actual.story += 1;
            if (record.typeOnly) actual.verificationType += 1;
            else actual.verificationValue += 1;
          }
          actualImports.set(name, actual);
        }
        if (edge.status === "unresolved-local" || edge.status === "unresolved-workspace-export" || edge.status === "unresolved-workspace-package" || edge.status === "unresolved-external") {
          blockers.push({ ...makeBlocker(edge, layerForPath(file), edge.status, edge.status === "unresolved-workspace-export" ? "package exports target is unavailable" : null), line: record.line });
        }
        if (record.conditional) blockers.push({ ...makeBlocker(edge, layerForPath(file), "conditional-import", "literal import appears under a conditional AST node"), line: record.line });
      }
      for (const dynamicBlocker of extracted.blockers) {
        const edge = addEdge(file, { ...dynamicBlocker, specifier: null, kind: dynamicBlocker.kind }, "typescript");
        compilerRecords.push({ from: file, kind: dynamicBlocker.kind, specifier: null, typeOnly: false, conditional: Boolean(dynamicBlocker.conditional), line: dynamicBlocker.line });
        blockers.push({ ...makeBlocker(edge, layerForPath(file), dynamicBlocker.kind, dynamicBlocker.expression), line: dynamicBlocker.line });
      }
    } else if (CSS_EXTENSIONS.has(extension)) {
      for (const record of scanCssImports(text)) {
        if (/^(data:|https?:|\/\/|#)/i.test(record.specifier)) continue;
        addEdge(file, record, "css");
      }
    } else if (HTML_EXTENSIONS.has(extension)) {
      for (const record of scanHtmlImports(text)) {
        if (/^(data:|https?:|\/\/|#)/i.test(record.specifier)) continue;
        addEdge(file, record, "html");
      }
    }
  }

  const consistencyKey = (record) => [record.from, record.kind, record.specifier, record.typeOnly, record.conditional].join("\u0000");
  const compilerKeys = compilerRecords.map(consistencyKey).sort();
  const graphCompilerKeys = edges
    .filter((edge) => edge.origin === "typescript")
    .flatMap((edge) => Array.from({ length: edge.occurrences ?? 1 }, () => [edge.from, edge.kind, edge.specifier, Boolean(edge.typeOnly), Boolean(edge.conditional)].join("\u0000")))
    .sort();
  const missing = compilerKeys.filter((key, index) => key !== graphCompilerKeys[index]);
  const extra = graphCompilerKeys.filter((key, index) => key !== compilerKeys[index]);
  if (missing.length || extra.length) throw new Error(`compiler import graph mismatch: missing=${missing.length} extra=${extra.length}`);
  const compilerLiteralKeys = compilerRecords.filter((record) => record.specifier !== null).map((record) => `${record.from}\u0000${record.specifier}`).sort();
  const preProcessKeys = preProcessRecords.map((record) => `${record.from}\u0000${record.specifier}`).sort();
  const preProcessMissing = compilerLiteralKeys.filter((key, index) => key !== preProcessKeys[index]);
  const preProcessExtra = preProcessKeys.filter((key, index) => key !== compilerLiteralKeys[index]);
  if (preProcessMissing.length || preProcessExtra.length) throw new Error(`preProcessFile import parity mismatch: missing=${preProcessMissing.length} extra=${preProcessExtra.length}`);
  const allCorpusNamedTypeOnlyImports = compilerRecords.filter((record) => record.typeOnlyForm === "named-specifiers").length;
  const storyNamedTypeOnlyImports = compilerRecords.filter((record) => record.typeOnlyForm === "named-specifiers" && layerForPath(record.from) === "story").length;
  const allNamedTypeOnlyImports = allCorpusNamedTypeOnlyImports - storyNamedTypeOnlyImports;

  const runtimeRoots = allFiles.includes("src/main.tsx") ? ["src/main.tsx"] : [];
  const testRoots = sourceFiles.filter((file) => layerForPath(file) === "test").sort();
  const storyRoots = sourceFiles.filter((file) => layerForPath(file) === "story").sort();
  const buildRoots = allFiles.filter((file) => layerForPath(file) === "build" && (SOURCE_EXTENSIONS.has(path.extname(file).toLowerCase()) || HTML_EXTENSIONS.has(path.extname(file).toLowerCase()))).sort();
  const compileReachability = walkReachability(runtimeRoots, compileEdgeMap);
  const runtimeReachability = walkReachability(runtimeRoots, runtimeEdgeMap);
  const testReachability = walkReachability(testRoots, compileEdgeMap);
  const storyReachability = walkReachability(storyRoots, compileEdgeMap);
  const buildReachability = walkReachability(buildRoots, compileEdgeMap);

  const nodes = sourceFiles.sort().map((file) => {
    const classification = classifyPath(file, edges.filter((edge) => edge.from === file), {
      runtime: runtimeReachability.distance.has(file),
      compile: compileReachability.distance.has(file),
      test: testReachability.distance.has(file),
      story: storyReachability.distance.has(file),
      build: buildReachability.distance.has(file),
    });
    return {
      path: file,
      disposition: classification.disposition,
      reasonCode: classification.reasonCode,
      surfaceTags: surfaceTags(file, classification.disposition),
      runtimeReachable: runtimeReachability.distance.has(file),
      runtimeCompileReachable: compileReachability.distance.has(file),
      runtimeEmittedReachable: runtimeReachability.distance.has(file),
      runtimeWitness: runtimeReachability.witness.has(file) ? runtimeReachability.witness.get(file).join(">") : null,
      ...(classification.preserveStructure ? { preserveStructure: true } : {}),
    };
  });
  const nodeByPath = new Map(nodes.map((node) => [node.path, node]));
  const appSourceSet = new Set(sourceFiles);
  const directUnsupported = nodes.filter((node) => node.disposition === "delete-unsupported").map((node) => node.path);
  const policyClosure = new Map();
  for (const root of UNSUPPORTED_COMPOSITION_ROOTS) {
    const rootFiles = directUnsupported.filter((file) => underRoot(file, root));
    if (!rootFiles.length) continue;
    const closure = walkReachability(rootFiles, compileEdgeMap).distance;
    const files = [...closure.keys()].filter((file) => appSourceSet.has(file)).sort();
    policyClosure.set(root, files);
  }

  const packageDecisions = buildPackageDecisions(appPackageJson, declarations, actualImports, packageMap);
  const blockerLayers = { runtime: [], test: [], story: [], build: [] };
  for (const blocker of blockers) blockerLayers[blocker.layer].push(blocker);
  for (const diagnostic of config.errors) {
    blockerLayers.build.push({
      source: "tsconfig.json",
      layer: "build",
      kind: "tsconfig-diagnostic",
      specifier: null,
      category: "tsconfig-extends",
      detail: ts.flattenDiagnosticMessageText(diagnostic.messageText, " "),
    });
  }
  for (const values of Object.values(blockerLayers)) values.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));

  const runtimeFiles = nodes.filter((node) => node.runtimeReachable).map((node) => node.path);
  const compileOnlyLocalPaths = nodes.filter((node) => node.runtimeCompileReachable && !node.runtimeEmittedReachable).map((node) => node.path).sort();
  if (JSON.stringify(compileOnlyLocalPaths) !== JSON.stringify(EXPECTED_COMPILE_ONLY_LOCAL)) throw new Error(`compile-only local corpus changed: expected ${EXPECTED_COMPILE_ONLY_LOCAL.length}, got ${compileOnlyLocalPaths.length}`);
  const w603Paths = nodes.filter((node) => ["retain-verbatim", "adapt-boundary", "asset-build"].includes(node.disposition)).map((node) => node.path).sort();
  const w604Paths = nodes.filter((node) => ["delete-unsupported", "verification-only"].includes(node.disposition)).map((node) => node.path).sort();
  const rawBatches = {
    "W-603": batchFiles(w603Paths, "W-603"),
    "W-604": batchFiles(w604Paths, "W-604"),
  };
  const globalBatchByFile = new Map(Object.values(rawBatches).flatMap((group) => group.flatMap((batch) => batch.files.map((file) => [file, batch]))));
  const batches = {
    "W-603": batchDetails(rawBatches["W-603"], edges, nodeByPath, "W-603", globalBatchByFile),
    "W-604": batchDetails(rawBatches["W-604"], edges, nodeByPath, "W-604", globalBatchByFile),
  };
  const batchGraph = batchDependencyGraph(batches);
  const assigned = [...batches["W-603"], ...batches["W-604"]].flatMap((batch) => batch.files);
  const assignedCounts = new Map();
  for (const file of assigned) assignedCounts.set(file, (assignedCounts.get(file) ?? 0) + 1);
  const batchCoverage = {
    expected: sourceFiles.length,
    assigned: assigned.length,
    missing: sourceFiles.filter((file) => !assignedCounts.has(file)),
    duplicates: [...assignedCounts.entries()].filter(([, count]) => count > 1).map(([file]) => file).sort(),
  };
  const batchByFile = new Map([...batches["W-603"], ...batches["W-604"]].flatMap((batch) => batch.files.map((file) => [file, batch])));
  let crossChunkEdges = 0;
  let crossChunkOccurrences = 0;
  let missingCrossStageEdges = 0;
  let crossIssueEdges = 0;
  let crossIssueLogicalEdges = 0;
  let crossIssueOccurrences = 0;
  let crossIssueLogicalOccurrences = 0;
  let missingCrossIssueBatchDependencies = 0;
  const crossIssueLogicalKeys = new Set();
  for (const edge of edges) {
    if (!edge.to) continue;
    const sourceBatch = batchByFile.get(edge.from);
    const targetBatch = batchByFile.get(edge.to);
    if (!sourceBatch || !targetBatch || sourceBatch.id === targetBatch.id) continue;
    const occurrences = edge.occurrences ?? 1;
    crossChunkEdges += 1;
    crossChunkOccurrences += occurrences;
    if (!sourceBatch.dependsOnBatchIds.includes(targetBatch.id)) missingCrossStageEdges += 1;
    if (sourceBatch.issue !== targetBatch.issue) {
      crossIssueEdges += 1;
      crossIssueOccurrences += occurrences;
      const logicalKey = [edge.from, edge.kind, edge.specifier, edge.to].join("\u0000");
      if (!crossIssueLogicalKeys.has(logicalKey)) {
        crossIssueLogicalKeys.add(logicalKey);
        crossIssueLogicalEdges += 1;
        crossIssueLogicalOccurrences += occurrences;
      }
      if (!sourceBatch.dependsOnBatchIds.includes(targetBatch.id)) missingCrossIssueBatchDependencies += 1;
    }
  }
  batchCoverage.crossStageEdges = crossChunkEdges;
  batchCoverage.missingCrossStageEdges = missingCrossStageEdges;
  batchCoverage.crossChunkEdges = crossChunkEdges;
  batchCoverage.crossChunkOccurrences = crossChunkOccurrences;
  batchCoverage.crossIssueEdges = crossIssueEdges;
  batchCoverage.crossIssueOccurrences = crossIssueOccurrences;
  batchCoverage.crossIssueLogicalEdges = crossIssueLogicalEdges;
  batchCoverage.crossIssueLogicalOccurrences = crossIssueLogicalOccurrences;
  batchCoverage.missingCrossIssueBatchDependencies = missingCrossIssueBatchDependencies;
  assertBatchCoverage(batchCoverage);
  const w603Files = new Set(batches["W-603"].flatMap((batch) => batch.files));
  if (EXPECTED_COMPILE_ONLY_LOCAL.some((file) => !w603Files.has(file) || nodeByPath.get(file)?.disposition === "verification-only")) throw new Error("compile-only local modules must be retained in W-603");
  if ([...TSCONFIG_BUILD_INPUTS].some((file) => !w603Files.has(file) || nodeByPath.get(file)?.disposition !== "retain-verbatim")) throw new Error("tsconfig ambient declarations must be retained in W-603");
  const batchPlan = {
    kind: "reviewChunks",
    executable: false,
    schedulingUnits: "issueStagesWithSCCConflicts",
    atomicStageCount: Object.keys(batches).length,
    sccCount: batchGraph.stronglyConnectedComponents.length,
    atomicStages: Object.entries(batches).map(([issue, group]) => ({
      issue,
      batchIds: group.map((batch) => batch.id),
      cyclic: group.some((batch) => batch.dependsOnBatchIds.some((id) => id.startsWith(`${issue}-`))),
    })),
    crossIssueEdges,
    crossIssueLogicalEdges,
    crossIssueOccurrences,
    crossIssueLogicalOccurrences,
    crossIssueCycles: batchGraph.stronglyConnectedComponents.filter((component) => component.cyclic && component.batchIds.some((id) => id.startsWith("W-603-")) && component.batchIds.some((id) => id.startsWith("W-604-"))).length,
    crossStageConflict: crossIssueEdges > 0,
  };

  const requiredAppEdge = edges.some((edge) => edge.from === "src/App.tsx" && edge.origin === "typescript" && edge.specifier === "react-router-dom");
  const requiredRouteEdge = edges.some((edge) => edge.from === "src/App.tsx" && edge.origin === "typescript" && edge.specifier === "./lib/route-paths");
  if (!requiredAppEdge || !requiredRouteEdge) throw new Error("App.tsx compiler graph is missing required route imports");
  const composerNode = nodeByPath.get("src/components/promptbox/NewThreadComposer.tsx");
  if (!composerNode) throw new Error("NewThreadComposer.tsx is not tracked");
  if (!composerNode.runtimeReachable && !blockerLayers.runtime.some((blocker) => blocker.source === composerNode.path)) throw new Error("NewThreadComposer.tsx is neither runtime reachable nor explained by a runtime blocker");
  const automationAssertions = ["src/App.legacy-automation-routes.test.tsx", "src/components/tools/Automations.stories.tsx"].map((file) => nodeByPath.get(file));
  for (const node of automationAssertions) {
    if (!node || node.disposition !== "verification-only" || !node.surfaceTags.includes("automations")) throw new Error(`automation verification classification failed for ${node?.path ?? "missing file"}`);
  }
  const pluginSlots = nodeByPath.get("src/lib/plugin-slots.ts");
  if (!pluginSlots || !["adapt-boundary", "delete-unsupported"].includes(pluginSlots.disposition)) throw new Error("plugin-slots.ts must be an adapter or explicit deletion");
  const fileIndex = new Map(allFiles.map((file, index) => [file, index]));

  return {
    format: "loom.app-port-plan/v1",
    generatedBy: "scripts/analyze-ui-port.mjs",
    reviewSummary: "ui/app-port-plan.summary.json",
    source: {
      root: sourceRoot,
      trackedFiles: sourceFiles.length,
      exactGitTree: gitTree(repo, "apps/app"),
    },
    resolver: {
      compiler: "typescript",
      config: "apps/app/tsconfig.json",
      moduleResolution: ts.ModuleResolutionKind[config.options.moduleResolution] ?? String(config.options.moduleResolution),
      conditions: ["source", "types", "import", "require", "default"],
      configDiagnostics: config.errors.length,
    },
    roots: {
      runtime: runtimeRoots,
      test: testRoots,
      story: storyRoots,
      build: buildRoots,
    },
    nodes,
    graph: {
      edgeFields: EDGE_FIELDS,
      fileTable: allFiles,
      edges: edges.map((edge) => compactEdge(edge, fileIndex)),
      parsers: {
        typescript: {
          files: allFiles.filter((file) => SOURCE_EXTENSIONS.has(path.extname(file).toLowerCase())).length,
          specifiers: compilerRecords.length,
          preProcessFileSpecifiers: preProcessRecords.length,
        },
        css: {
          files: allFiles.filter((file) => CSS_EXTENSIONS.has(path.extname(file).toLowerCase())).length,
          specifiers: edges.filter((edge) => edge.origin === "css").reduce((sum, edge) => sum + (edge.occurrences ?? 1), 0),
        },
        html: {
          files: allFiles.filter((file) => HTML_EXTENSIONS.has(path.extname(file).toLowerCase())).length,
          specifiers: edges.filter((edge) => edge.origin === "html").reduce((sum, edge) => sum + (edge.occurrences ?? 1), 0),
        },
      },
      typeOnlySemantics: {
        allNamedTypeOnlyImports,
        storyNamedTypeOnlyImports,
        allCorpusNamedTypeOnlyImports,
      },
      requiredImports: [
        { source: "src/App.tsx", kind: "static", specifier: "react-router-dom" },
        { source: "src/App.tsx", kind: "static", specifier: "./lib/route-paths" },
      ],
      externalModules: [...new Set(edges.filter((edge) => edge.package && edge.status !== "builtin").map((edge) => edge.package))].sort(),
      reachability: {
        runtime: runtimeFiles.length,
        runtimeEmitted: runtimeFiles.length,
        runtimeCompile: compileReachability.distance.size,
        compileOnlyLocal: compileOnlyLocalPaths.length,
        runtimeReachableSemantics: "runtimeEmittedReachable",
        test: testReachability.distance.size,
        story: storyReachability.distance.size,
        build: buildReachability.distance.size,
      },
      compilerImportConsistency: {
        compilerSpecifiers: compilerRecords.length,
        graphSpecifiers: graphCompilerKeys.length,
        missing: [],
        extra: [],
        preProcessFileParity: {
          compilerLiteralSpecifiers: compilerLiteralKeys.length,
          preProcessFileSpecifiers: preProcessKeys.length,
          missing: [],
          extra: [],
        },
      },
    },
    policy: {
      unsupportedCompositionRoots: UNSUPPORTED_COMPOSITION_ROOTS,
      adapterBoundaries: [...ADAPTER_BOUNDARY_PATHS].sort(),
      tsconfigBuildInputs: [...TSCONFIG_BUILD_INPUTS].sort(),
      deleteClosure: Object.fromEntries([...policyClosure.entries()].map(([root, files]) => [root, files])),
      classificationPriority: ["verification-only", "asset-build", "delete-unsupported", "adapt-boundary", "retain-verbatim"],
    },
    workspacePackages: packageDecisions,
    compileBlockers: blockerLayers,
    batches,
    batchGraph,
    batchPlan,
    batchCoverage,
    assertions: {
      appImports: {
        reactRouterDom: requiredAppEdge,
        routePaths: requiredRouteEdge,
      },
      composer: {
        path: composerNode.path,
        runtimeReachable: composerNode.runtimeReachable,
        runtimeWitness: composerNode.runtimeWitness,
        blocker: blockerLayers.runtime.find((blocker) => blocker.source === composerNode.path) ?? null,
      },
      automationsVerificationOnly: automationAssertions.map((node) => ({ path: node.path, disposition: node.disposition, surfaceTags: node.surfaceTags })),
      pluginSlots: { path: pluginSlots.path, disposition: pluginSlots.disposition, preserveStructure: pluginSlots.preserveStructure ?? false },
    },
    summary: {
      nodes: nodes.length,
      runtimeReachable: runtimeFiles.length,
      runtimeCompileReachable: compileReachability.distance.size,
      runtimeEmittedReachable: runtimeFiles.length,
      dispositions: Object.fromEntries(["retain-verbatim", "adapt-boundary", "delete-unsupported", "verification-only", "asset-build"].map((value) => [value, nodes.filter((node) => node.disposition === value).length])),
      edges: edges.length,
      blockers: Object.fromEntries(Object.entries(blockerLayers).map(([layer, values]) => [layer, values.length])),
      workspacePackages: packageDecisions.length,
    },
  };
}

function stableJson(value) {
  return `${JSON.stringify(value)}\n`;
}

function reviewSummary(plan) {
  const blockerCategories = Object.fromEntries(Object.entries(plan.compileBlockers).map(([layer, blockers]) => {
    const categories = {};
    for (const blocker of blockers) categories[blocker.category] = (categories[blocker.category] ?? 0) + 1;
    return [layer, { count: blockers.length, categories }];
  }));
  return {
    format: "loom.app-port-plan-summary/v1",
    plan: "ui/app-port-plan.json",
    planBytes: stableJson(plan).length,
    source: plan.source,
    summary: plan.summary,
    dispositions: plan.summary.dispositions,
    exceptions: {
      boundary: plan.nodes.filter((node) => node.disposition === "adapt-boundary").map((node) => ({ path: node.path, reasonCode: node.reasonCode, preserveStructure: node.preserveStructure ?? false })),
      unsupported: plan.nodes.filter((node) => node.disposition === "delete-unsupported").map((node) => ({ path: node.path, reasonCode: node.reasonCode })),
      nonRuntime: plan.nodes.filter((node) => !node.runtimeReachable && node.disposition !== "asset-build").map((node) => ({ path: node.path, disposition: node.disposition, reasonCode: node.reasonCode })),
      compileOnlyLocal: plan.nodes.filter((node) => node.runtimeCompileReachable && !node.runtimeEmittedReachable).map((node) => node.path),
      tsconfigBuildInputs: plan.policy.tsconfigBuildInputs,
    },
    blockers: blockerCategories,
    requiredAssertions: plan.assertions,
    compilerConsistency: plan.graph.compilerImportConsistency,
    parsers: plan.graph.parsers,
    batchCoverage: plan.batchCoverage,
    batchPlan: plan.batchPlan,
    batchGraph: plan.batchGraph,
    batches: Object.fromEntries(Object.entries(plan.batches).map(([issue, batches]) => [issue, batches.map((batch) => ({
      id: batch.id,
      kind: batch.kind,
      executable: batch.executable,
      fileCount: batch.files.length,
      dispositions: batch.dispositions,
      dependencyCount: batch.dependencies.length,
      dependsOnBatchIds: batch.dependsOnBatchIds,
      cutPoints: batch.cutPoints,
      verificationCommands: batch.verificationCommands,
    }))])),
    workspacePackages: plan.workspacePackages.map((item) => ({
      name: item.name,
      declaredAs: item.declaredAs,
      actualImports: item.actualImports,
      runtimeImports: item.runtimeImports,
      runtimeValueImports: item.runtimeValueImports,
      runtimeTypeImports: item.runtimeTypeImports,
      verificationValueImports: item.verificationValueImports,
      verificationTypeImports: item.verificationTypeImports,
      decision: item.decision,
      reasonCode: item.reasonCode,
      runtimeAllowed: item.runtimeAllowed,
    })),
  };
}

function stableReviewSummary(plan) {
  const summary = JSON.stringify(reviewSummary(plan), null, 2);
  if (summary.split("\n").length > 30000) throw new Error("ui/app-port-plan.summary.json is larger than 30000 lines");
  return `${summary}\n`;
}

function checkPlan(plan) {
  assert.equal(plan.format, "loom.app-port-plan/v1");
  assert.equal(plan.reviewSummary, "ui/app-port-plan.summary.json");
  assert.equal(plan.source.trackedFiles, 1437);
  assert.equal(plan.source.exactGitTree, expectedAppTree);
  assert.equal(plan.nodes.length, 1437);
  assert.equal(plan.graph.reachability.compileOnlyLocal, 11);
  assert.equal(plan.graph.reachability.runtimeReachableSemantics, "runtimeEmittedReachable");
  assert.ok(stableJson(plan).length <= 2 * 1024 * 1024, `plan is larger than 2 MiB: ${stableJson(plan).length}`);
  assert.ok(stableJson(plan).split("\n").length <= 30000, "plan is larger than 30000 lines");
  const paths = plan.nodes.map((node) => node.path);
  assert.equal(new Set(paths).size, paths.length);
  for (const node of plan.nodes) {
    assert.ok(["retain-verbatim", "adapt-boundary", "delete-unsupported", "verification-only", "asset-build"].includes(node.disposition), node.path);
    assert.equal(Object.prototype.hasOwnProperty.call(node, "bytes"), false);
    assert.equal(Object.prototype.hasOwnProperty.call(node, "sha256"), false);
    assert.ok(Array.isArray(node.surfaceTags));
    assert.equal(typeof node.runtimeCompileReachable, "boolean");
    assert.equal(typeof node.runtimeEmittedReachable, "boolean");
  }
  assert.equal(plan.batchCoverage.missing.length, 0);
  assert.equal(plan.batchCoverage.duplicates.length, 0);
  assert.equal(plan.batchCoverage.assigned, 1437);
  assert.equal(plan.batchCoverage.missingCrossStageEdges, 0);
  assert.equal(plan.batchCoverage.missingCrossIssueBatchDependencies, 0);
  assert.equal(plan.batchCoverage.crossIssueEdges, 1519);
  assert.equal(plan.batchPlan.kind, "reviewChunks");
  assert.equal(plan.batchPlan.executable, false);
  assert.equal(plan.batchPlan.atomicStageCount, 2);
  assert.equal(plan.graph.compilerImportConsistency.missing.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.extra.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.preProcessFileParity.missing.length, 0);
  assert.equal(plan.graph.compilerImportConsistency.preProcessFileParity.extra.length, 0);
  assert.equal(plan.graph.typeOnlySemantics.allNamedTypeOnlyImports, 20);
  assert.equal(plan.resolver.configDiagnostics, 2);
  for (const issue of ["W-603", "W-604"]) {
    assert.ok(Array.isArray(plan.batches[issue]) && plan.batches[issue].length > 0);
    for (const batch of plan.batches[issue]) {
      assert.ok(Array.isArray(batch.dependencies));
      assert.ok(Array.isArray(batch.cutPoints));
      assert.ok(Array.isArray(batch.verificationCommands) && batch.verificationCommands.length > 0);
    }
  }
  assert.equal(plan.assertions.appImports.reactRouterDom, true);
  assert.equal(plan.assertions.appImports.routePaths, true);
  assert.equal(plan.assertions.automationsVerificationOnly[0].disposition, "verification-only");
  assert.equal(plan.assertions.automationsVerificationOnly[1].disposition, "verification-only");
  assert.ok(["adapt-boundary", "delete-unsupported"].includes(plan.assertions.pluginSlots.disposition));
}

export function generatePlan(options = {}) {
  const plan = analyzeApp(options);
  checkPlan(plan);
  return plan;
}

function run() {
  const shouldWrite = process.argv.includes("--write");
  const plan = generatePlan();
  const summary = stableReviewSummary(plan);
  if (shouldWrite) fs.writeFileSync(planPath, stableJson(plan));
  else {
    if (!fs.existsSync(planPath)) throw new Error("ui/app-port-plan.json is missing; run node scripts/analyze-ui-port.mjs --write");
    const current = fs.readFileSync(planPath, "utf8");
    if (current !== stableJson(plan)) throw new Error("ui/app-port-plan.json is stale; run node scripts/analyze-ui-port.mjs --write and review the diff");
  }
  if (shouldWrite) fs.writeFileSync(reviewSummaryPath, summary);
  else {
    if (!fs.existsSync(reviewSummaryPath)) throw new Error("ui/app-port-plan.summary.json is missing; run node scripts/analyze-ui-port.mjs --write");
    const currentSummary = fs.readFileSync(reviewSummaryPath, "utf8");
    if (currentSummary !== summary) throw new Error("ui/app-port-plan.summary.json is stale; run node scripts/analyze-ui-port.mjs --write and review the diff");
  }
  console.log(`app port plan OK: ${plan.summary.nodes} nodes, ${plan.summary.edges} edges, ${plan.summary.blockers.runtime + plan.summary.blockers.test + plan.summary.blockers.story + plan.summary.blockers.build} blockers`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  try {
    exactSnapshotCheck(repoRoot, trackedFiles(repoRoot, "apps/app/src"));
    run();
  } catch (error) {
    console.error(`app port plan: ${error.message}`);
    process.exitCode = 1;
  }
}
