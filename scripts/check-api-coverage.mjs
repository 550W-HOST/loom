#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const contractPath = path.join(repoRoot, "contracts/bb/server-api.json");
const manifestPath = path.join(repoRoot, "contracts/bb/manifest.json");
const httpPath = path.join(repoRoot, "crates/server/src/http.rs");
const documentPath = path.join(repoRoot, "docs/api-coverage.md");

const notApplicableSkillRoutes = new Set([
  "projects.deleteSkill",
  "projects.skillContent",
  "projects.skillFiles",
  "projects.skills",
  "projects.updateSkill",
  "system.cliSkillsStatus",
  "system.installCliSkills",
]);

const batches = [
  {
    id: "B1",
    title: "启动、导航与首个 threads 流程",
    dependency: "B0",
    ui: "启动探活、侧栏初始化、项目/线程列表后的线程打开、时间线读取与发送",
    routes: [
      "projects.sidebarBootstrap",
      "system.version",
      "system.config",
      "system.providers",
      "system.providerStates",
      "system.environmentProviders",
      "system.executionOptions",
      "threads.get",
      "threads.timeline",
      "threads.events",
      "threads.send",
      "threads.read",
      "threads.output",
      "threads.tabs",
    ],
  },
  {
    id: "B2",
    title: "线程控制与辅助视图",
    dependency: "B1",
    ui: "活动线程的默认执行选项、运行状态、搜索、历史、编辑、停止/重试和压缩",
    routes: [
      "projects.defaultExecutionOptions",
      "threads.childSummary",
      "threads.conversationOutline",
      "threads.defaultExecutionOptions",
      "threads.running",
      "threads.search",
      "threads.promptHistory",
      "threads.update",
      "threads.updateTabs",
      "threads.open",
      "threads.stop",
      "threads.retry",
      "threads.compact",
      "threads.editMessage",
    ],
  },
  {
    id: "B3",
    title: "交互、计划与队列发送",
    dependency: "B1 + B2",
    ui: "线程中的交互请求、计划控制和 queued message 的查看/创建/发送",
    routes: [
      "queue.list",
      "threads.interactions",
      "threads.interaction",
      "threads.cancelInteraction",
      "threads.resolveInteraction",
      "threads.respondToInteraction",
      "threads.cancelPlan",
      "threads.clearContext",
      "threads.clearGoal",
      "threads.eventWait",
      "threads.timelineTurnSummaryDetails",
      "threads.createQueuedMessage",
      "threads.queuedMessages",
      "threads.sendQueuedMessage",
    ],
  },
  {
    id: "B4",
    title: "线程生命周期与队列管理",
    dependency: "B3",
    ui: "归档、删除、分叉、置顶/未读，以及 queued message 的删除、排序和更新",
    routes: [
      "threads.archive",
      "threads.archiveAll",
      "threads.unarchive",
      "threads.delete",
      "threads.fork",
      "threads.pin",
      "threads.pinOrder",
      "threads.unpin",
      "threads.unread",
      "threads.resolveMentions",
      "threads.deleteQueuedMessage",
      "threads.reorderQueuedMessage",
      "threads.setQueuedMessageGroupBoundary",
      "threads.updateQueuedMessage",
    ],
  },
  {
    id: "B5",
    title: "线程文件与存储辅助面",
    dependency: "B1 + B4",
    ui: "线程计数、pane action、host file 和 thread storage 的查看",
    routes: [
      "threads.count",
      "threads.hostFileContent",
      "threads.paneAction",
      "threads.rawFile",
      "threads.storageContent",
      "threads.storageFile",
      "threads.storageFiles",
      "threads.storageLocation",
      "threads.storagePaths",
      "threads.worktreeFile",
    ],
  },
  {
    id: "B6",
    title: "环境生命周期与项目仓库状态",
    dependency: "B0 + B1",
    ui: "项目选择后的环境操作、diff/PR 状态与分支选择",
    routes: [
      "environments.actions",
      "environments.archiveThreads",
      "environments.delete",
      "environments.diff",
      "environments.diffBranches",
      "environments.diffFile",
      "environments.diffFiles",
      "environments.diffPatch",
      "environments.paths",
      "environments.pullRequest",
      "environments.status",
      "environments.update",
      "projects.branches",
      "projects.branchOptions",
    ],
  },
  {
    id: "B7",
    title: "项目工作区、附件与 thread sections",
    dependency: "B6",
    ui: "项目文件/来源/附件操作，以及侧栏 section 的维护",
    routes: [
      "projects.attachmentContent",
      "projects.commands",
      "projects.copyAttachments",
      "projects.delete",
      "projects.fileContent",
      "projects.files",
      "projects.paths",
      "projects.promptHistory",
      "projects.reorder",
      "projects.updateSource",
      "projects.uploadAttachment",
      "threadSections.create",
      "threadSections.delete",
      "threadSections.update",
    ],
  },
  {
    id: "B8",
    title: "主机与环境连接能力",
    dependency: "B1 + B6",
    ui: "主机选择、目录选择、路径检查、provider CLI 与更新状态",
    routes: [
      "filePreviews.content",
      "hosts.cloneDefaultPath",
      "hosts.createJoinCode",
      "hosts.delete",
      "hosts.directory",
      "hosts.get",
      "hosts.pathsExist",
      "hosts.pickFolder",
      "hosts.providerCliInstall",
      "hosts.providerCliStatus",
      "hosts.retryUpdate",
      "hosts.update",
      "hosts.updatePermissionCeiling",
      "system.attention",
    ],
  },
  {
    id: "B9",
    title: "文件操作与终端",
    dependency: "B6 + B8",
    ui: "工作区文件读写/移动/预览和终端创建、输入、输出与重启",
    routes: [
      "files.createPreview",
      "files.list",
      "files.listPaths",
      "files.mkdir",
      "files.move",
      "files.read",
      "files.remove",
      "files.write",
      "terminals.close",
      "terminals.create",
      "terminals.get",
      "terminals.input",
      "terminals.list",
      "terminals.output",
      "terminals.resize",
      "terminals.restart",
      "terminals.update",
    ],
  },
  {
    id: "B10",
    title: "设置与系统偏好",
    dependency: "B1",
    ui: "外观、键盘、实验项、provider logo、主题和 UI 偏好设置",
    routes: [
      "system.appearance",
      "system.experiments",
      "system.generalSettings",
      "system.keyboardSettings",
      "system.providerLogo",
      "system.reloadConfig",
      "system.resetUiPreference",
      "system.resolveTheme",
      "system.themes",
      "system.uiPreferences",
      "system.updateUiPreference",
      "system.usageLimits",
      "system.voiceTranscription",
    ],
  },
];

function skipWhitespace(source, index) {
  while (/\s/.test(source[index] ?? "")) index += 1;
  return index;
}

function readRustString(source, index) {
  if (source[index] !== '"') throw new Error("expected a Rust string");
  let value = "";
  let cursor = index + 1;
  while (cursor < source.length) {
    if (source[cursor] === '"' && source[cursor - 1] !== "\\") {
      return { value, next: cursor + 1 };
    }
    value += source[cursor];
    cursor += 1;
  }
  throw new Error("unterminated Rust string");
}

function readRouteCall(source, start) {
  const open = start + ".route(".length - 1;
  let cursor = open + 1;
  cursor = skipWhitespace(source, cursor);
  const pathValue = readRustString(source, cursor);
  cursor = skipWhitespace(source, pathValue.next);

  const expressionStart = cursor;
  let depth = 1;
  let quoted = false;
  for (; cursor < source.length && depth > 0; cursor += 1) {
    const character = source[cursor];
    if (character === '"' && source[cursor - 1] !== "\\") quoted = !quoted;
    if (!quoted) {
      if (character === "(") depth += 1;
      if (character === ")") depth -= 1;
    }
  }
  if (depth !== 0) throw new Error("unterminated Router::route call");

  const expression = source.slice(expressionStart, cursor - 1);
  const methods = [
    ...expression.matchAll(
      /(?:^|[^A-Za-z0-9_:])(?:axum::routing::)?(get|post|put|patch|delete|head|options|trace|connect)\s*\(/g,
    ),
  ].map((match) => match[1].toUpperCase());
  if (methods.length === 0) {
    throw new Error(`no HTTP method found for route ${pathValue.value}`);
  }
  return { path: pathValue.value, methods, next: cursor };
}

function parseSourceRoutes(source) {
  const routes = [];
  let cursor = 0;
  while (cursor < source.length) {
    const start = source.indexOf(".route(", cursor);
    if (start < 0) break;
    const route = readRouteCall(source, start);
    routes.push(route);
    cursor = route.next;
  }
  return routes;
}

function normalizedPath(routePath) {
  return routePath
    .split("/")
    .map((segment) =>
      segment.startsWith(":") || segment.startsWith("{") ? ":param" : segment,
    )
    .join("/");
}

function routeKey(method, routePath) {
  return `${method.toUpperCase()} ${normalizedPath(routePath)}`;
}

function notApplicableReason(routeId) {
  if (routeId.startsWith("desktopBrowsers.")) {
    return "已决策：desktopBrowsers 不实现";
  }
  if (notApplicableSkillRoutes.has(routeId)) {
    return "已决策：skill/CLI skill 不实现";
  }
  return null;
}

function classifyRoutes(contractRoutes, sourceRoutes) {
  const sourceByKey = new Map();
  for (const sourceRoute of sourceRoutes) {
    for (const method of sourceRoute.methods) {
      const key = routeKey(method, sourceRoute.path);
      if (sourceByKey.has(key)) {
        throw new Error(`duplicate source route shape: ${key}`);
      }
      sourceByKey.set(key, { method, path: sourceRoute.path });
    }
  }

  return contractRoutes.map((route) => {
    const reason = notApplicableReason(route.id);
    const implementation = sourceByKey.get(routeKey(route.method, route.fullPath));
    if (reason) return { ...route, status: "不适用（已决策）", reason, implementation: null };
    if (implementation) {
      return { ...route, status: "已实现", reason: "", implementation };
    }
    return { ...route, status: "待实现", reason: "", implementation: null };
  });
}

function batchMap() {
  const map = new Map();
  for (const batch of batches) {
    for (const routeId of batch.routes) {
      if (map.has(routeId)) throw new Error(`route appears in multiple batches: ${routeId}`);
      map.set(routeId, batch.id);
    }
  }
  return map;
}

function validateBatchAssignments(classified, assignments) {
  const byId = new Map(classified.map((route) => [route.id, route]));
  for (const routeId of assignments.keys()) {
    const route = byId.get(routeId);
    if (!route) throw new Error(`batch references unknown contract route: ${routeId}`);
    // A batch remains the historical ownership/dependency grouping after one
    // of its routes lands. Implemented routes move to B0 in the generated
    // coverage table, but must not make regeneration fail.
    if (route.status === "已实现") continue;
    if (route.status !== "待实现") {
      throw new Error(`batch references non-pending route: ${routeId} (${route.status})`);
    }
  }

  const pending = classified.filter((route) => route.status === "待实现");
  const missing = pending.filter((route) => !assignments.has(route.id));
  if (missing.length > 0) {
    throw new Error(`pending routes without a batch: ${missing.map((route) => route.id).join(", ")}`);
  }
  for (const batch of batches) {
    if (batch.routes.length < 10 || batch.routes.length > 20) {
      throw new Error(`${batch.id} must contain 10-20 routes, got ${batch.routes.length}`);
    }
  }
}

function markdownCode(value) {
  return `\`${value}\``;
}

function batchForRoute(route, assignments) {
  if (route.status === "已实现") return "B0";
  if (route.status === "不适用（已决策）") return "-";
  return assignments.get(route.id);
}

function generateDocument(classified, sourceRoutes, assignments, manifest) {
  const implemented = classified.filter((route) => route.status === "已实现");
  const pending = classified.filter((route) => route.status === "待实现");
  const notApplicable = classified.filter((route) => route.status === "不适用（已决策）");
  const effectiveTotal = classified.length - notApplicable.length;
  const percentage = ((implemented.length / effectiveTotal) * 100).toFixed(1);
  const sourceCommit = manifest.source?.commit ?? "unknown";
  const contractKeys = new Set(
    classified.map((route) => routeKey(route.method, route.fullPath)),
  );
  const extras = sourceRoutes.flatMap((route) =>
    route.methods
      .filter((method) => !contractKeys.has(routeKey(method, route.path)))
      .map((method) => `${method} ${route.path}`),
  );

  const rows = classified.map((route) => {
    const loomRoute = route.implementation
      ? markdownCode(`${route.implementation.method} ${route.implementation.path}`)
      : "-";
    const batch = batchForRoute(route, assignments);
    const note = route.reason || (route.status === "待实现" ? "" : "契约路径与方法已匹配") || "-";
    return `| ${markdownCode(route.id)} | ${markdownCode(route.method)} | ${markdownCode(route.fullPath)} | ${route.status} | ${loomRoute} | ${batch} | ${note} |`;
  });

  const batchRows = [
    `| B0 | 当前基础覆盖（基线） | ${implemented.length} | - | 项目/线程基础读写、环境读取和主机列表已存在 |`,
    ...batches.map(
      (batch) =>
        `| ${batch.id} | ${batch.title} | ${batch.routes.length} | ${batch.dependency} | ${batch.ui} |`,
    ),
  ];

  const extraList = extras.length > 0 ? extras.map((route) => `- ${markdownCode(route)}`).join("\n") : "- 无";

  return `<!-- Generated by scripts/check-api-coverage.mjs; edit the script or contract, then regenerate. -->

# API 覆盖清单

这份清单直接读取 \`contracts/bb/server-api.json\` 的 \`routes\`，并以
\`crates/server/src/http.rs\` 的 Axum \`.route\` 声明校验方法和路径形状。契约参数名
与 loom 参数名只要位于同一个路径段就视为相同。

**“已实现”现在同时意味着请求与响应都符合契约。** 路径匹配只是必要条件：一条
路由要计入覆盖率，它的请求体必须落在契约声明的 \`request.schema\` 内，响应体必须
落在对应状态码的 \`responses[].schema\` 内。请求侧无法只靠读源码证明——一个悄悄
重塑请求体的 handler 仍然返回正确响应——所以 \`loom-server\` 用
\`validate_contract_request\` 中间件在运行时对每条契约写路由校验请求，测试则用
\`validate_request\` / \`validate_response\` 双向断言。任何一侧不一致都会使
\`cargo test\` 失败，见 \`docs/contract.md\`。

## 结论

- 契约快照：${markdownCode(sourceCommit)}，共 **${classified.length} 条**路由。
- 已决策不实现：**${notApplicable.length} 条**（desktopBrowsers 11 条，skill/CLI skill 7 条）。
- 当前源码包含 **${sourceRoutes.length} 个 \`.route\` 声明、${sourceRoutes.reduce((count, route) => count + route.methods.length, 0)} 个 HTTP 方法入口**；其中只有 ${implemented.length} 个匹配 bb 契约，另有 ${extras.length} 个契约外入口。契约当前没有 plugin/marketplace 路由条目。
- 有效总数：**${effectiveTotal} 条**；当前已实现 **${implemented.length} 条**，待实现 **${pending.length} 条**。
- 当前有效覆盖率：**${implemented.length}/${effectiveTotal}（${percentage}%）**。
- B0 是现有实现基线；B1-B10 是建议的后续交付批次，每批 ${batches.map((batch) => batch.routes.length).join("、")} 条，均在 10-20 条范围内。

## threads.send 判定

结论是 **遗漏**，不是有意分歧。项目契约决策是让 UI 面向 bb 的公共 API；契约声明
的是 ${markdownCode("POST /api/v1/threads/:id/send")}（${markdownCode("threads.send")}），因此它必须进入兼容实现，已列入 **B1**。

当前 loom 的 ${markdownCode("POST /api/v1/threads/{id}/messages")} 是契约外的旧版参考 UI
写入端点，${markdownCode("ui/src/main.ts")} 仍在使用它。它不能替代 ${markdownCode("threads.send")}，也不计入覆盖率；
${markdownCode("threads.send")} 已接入同一线程发送/发布路径。后续再决定是否移除或保留参考 UI
的兼容端点，不修改 bb UI 的契约调用。

## 请求体兼容性决策（W-554）

**只接受契约形状（camelCase），不保留 snake_case 兼容输入。**

B1 的写路由最初只接受 loom 自造的 snake_case 请求体（${markdownCode("{\"project_id\": \"proj_...\"}")}），
而契约要求 ${markdownCode("threads.create")} 的 ${markdownCode("projectId/origin/input/environment")}。
bb UI 用契约格式发起写请求时会 422，而 CI 与一致性测试全绿。现在两条路由都只接受
契约形状，由 ${markdownCode("validate_contract_request")} 在运行时强制，不一致即 422。

选择“只接受契约形状”而非“两者都收”的理由：

- 契约是 UI 的消费面，bb 客户端无法协商方言。接受一种契约里不存在的形状，等于把
  loom 方言隐藏在兼容层下，而这正是本 issue 要根除的问题。
- 双形状会让“请求是否符合契约”这个可证伪的断言变成一个模糊集合，回归测试也就抓
  不住新的偏离。
- 代价可控：参考 UI ${markdownCode("ui/src/main.ts")} 只有两个写调用（创建 thread、发消息），
  已同步改为契约形状并重建 ${markdownCode("ui/app.js")}。

契约外路由（${markdownCode("/api/v1/threads/{id}/messages")}、${markdownCode("/api/v1/publish")}、环境/主机早期端点）不受
此决策约束：它们不在 bb 契约里，中间件对其不生效，保持各自的 handler 校验。

## 批次与依赖

批次按 UI 从启动、侧栏和线程主流程，到线程控制/队列，再到环境、项目工作区、主机、
文件/终端和设置的实际使用顺序排列。每批的路由可以在其依赖完成后独立验收。

| 批次 | 主题 | 路由数 | 依赖 | UI 交付边界 |
| --- | --- | ---: | --- | --- |
${batchRows.join("\n")}

## 契约外 loom 路由

以下源码路由不在 167 条 bb 契约路由中，不能被标记为已实现：

${extraList}

其中 ${markdownCode("/health")}, ${markdownCode("/ws")}, ${markdownCode("/internal/ws")}, ${markdownCode("/api/v1/publish")} 和 ${markdownCode("/api/v1/replay")} 属于 loom relay/control 面；
${markdownCode("/api/v1/version")} 与契约的 ${markdownCode("/api/v1/system/version")} 路径不同；其余是当前
域模型的早期端点。它们保持“契约外”是显式记录，不将其静默折算进 bb 覆盖率。

## 逐条清单

状态含义：

- **已实现**：源码中存在同 HTTP 方法和同路径形状的 loom route，**且**请求体与响应体都通过 bb 契约校验，附实际路径。
- **待实现**：契约中的有效路由尚未匹配，附后续批次号。
- **不适用（已决策）**：skill/CLI skill 或 desktopBrowsers，按项目决策不实现。

| 契约 ID | 方法 | 契约路径 | 状态 | loom 路由 | 批次 | 备注 |
| --- | --- | --- | --- | --- | --- | --- |
${rows.join("\n")}

## 复核命令

在仓库根目录执行：

${markdownCode("node scripts/check-api-coverage.mjs")}

该命令重新读取契约、解析源码 route 声明并校验本文件是否仍包含同一组 167 个契约
ID；输出已实现/有效总数、所有待实现差异和契约外源码路由。契约或路由发生变化时，
先执行 ${markdownCode("node scripts/check-api-coverage.mjs --write")} 更新本清单，再执行上面的
复核命令。
`;
}

function parseDocumentRows(document) {
  const rows = new Map();
  for (const line of document.split("\n")) {
    if (!line.startsWith("| `")) continue;
    const cells = line
      .split("|")
      .slice(1, -1)
      .map((cell) => cell.trim());
    if (cells.length !== 7) continue;
    const uncode = (cell) => (cell.startsWith("`") && cell.endsWith("`") ? cell.slice(1, -1) : cell);
    const id = uncode(cells[0]);
    rows.set(id, {
      id,
      method: uncode(cells[1]),
      path: uncode(cells[2]),
      status: cells[3],
      loomRoute: uncode(cells[4]),
      batch: cells[5],
    });
  }
  return rows;
}

/**
 * The regression guard for W-554: an implemented route with a JSON request
 * body must have request-side conformance coverage.
 *
 * The blind spot the issue describes was structural — responses were asserted
 * and requests never were, so a route could accept a dialect the contract does
 * not declare and every check stayed green. A route that is counted as
 * implemented and parses a JSON body must therefore appear in a
 * `validate_request*` assertion (or be one of the routes whose contract schema
 * has no required discriminator to assert against); otherwise the coverage
 * number would claim a conformance the tests do not prove.
 */
function validateRequestCoverage(classified) {
  const httpSource = fs.readFileSync(httpPath, "utf8");
  if (!/validate_contract_request/.test(httpSource)) {
    throw new Error(
      "crates/server/src/http.rs no longer wires validate_contract_request; " +
        "implemented write routes would stop validating request bodies",
    );
  }

  const testSources = [
    httpPath,
    ...fs
      .readdirSync(path.join(repoRoot, "crates/server/tests"))
      .filter((name) => name.endsWith(".rs"))
      .map((name) => path.join(repoRoot, "crates/server/tests", name)),
    ...fs
      .readdirSync(path.join(repoRoot, "crates/contract/tests"))
      .filter((name) => name.endsWith(".rs"))
      .map((name) => path.join(repoRoot, "crates/contract/tests", name)),
  ]
    .map((file) => fs.readFileSync(file, "utf8"))
    .join("\n");

  const missing = classified
    .filter((route) => route.status === "已实现")
    .filter((route) => route.request?.source === "json" && route.request.schema)
    .map((route) => route.id)
    .filter((id) => !testSources.includes(`validate_request_by_id("${id}"`));

  if (missing.length > 0) {
    throw new Error(
      `implemented routes with a JSON request body have no request-conformance assertion: ${missing.join(", ")}`,
    );
  }
}

function validateDocument(classified, assignments) {
  if (!fs.existsSync(documentPath)) {
    throw new Error(`${documentPath} is missing; run with --write first`);
  }
  const rows = parseDocumentRows(fs.readFileSync(documentPath, "utf8"));
  const expectedIds = new Set(classified.map((route) => route.id));
  const actualIds = new Set(rows.keys());
  const missing = [...expectedIds].filter((id) => !actualIds.has(id));
  const extra = [...actualIds].filter((id) => !expectedIds.has(id));
  if (missing.length || extra.length || rows.size !== classified.length) {
    throw new Error(
      `document route inventory mismatch (expected ${classified.length}, got ${rows.size}; missing: ${missing.join(", ") || "none"}; extra: ${extra.join(", ") || "none"})`,
    );
  }
  for (const route of classified) {
    const row = rows.get(route.id);
    const expectedBatch = batchForRoute(route, assignments);
    const expectedLoomRoute = route.implementation
      ? `${route.implementation.method} ${route.implementation.path}`
      : "-";
    if (
      row.method !== route.method ||
      row.path !== route.fullPath ||
      row.status !== route.status ||
      row.batch !== expectedBatch ||
      row.loomRoute !== expectedLoomRoute
    ) {
      throw new Error(`document row is stale or incorrect: ${route.id}`);
    }
  }
}

function main() {
  const contract = JSON.parse(fs.readFileSync(contractPath, "utf8"));
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  const source = fs.readFileSync(httpPath, "utf8");
  const contractRoutes = contract.routes;
  if (!Array.isArray(contractRoutes)) throw new Error("server-api.json has no routes array");
  const ids = new Set(contractRoutes.map((route) => route.id));
  if (ids.size !== contractRoutes.length) throw new Error("contract contains duplicate route IDs");

  const sourceRoutes = parseSourceRoutes(source);
  const classified = classifyRoutes(contractRoutes, sourceRoutes);
  const assignments = batchMap();
  validateBatchAssignments(classified, assignments);

  if (process.argv.includes("--write")) {
    fs.writeFileSync(
      documentPath,
      generateDocument(classified, sourceRoutes, assignments, manifest),
      "utf8",
    );
  }
  validateDocument(classified, assignments);
  validateRequestCoverage(classified);

  const implemented = classified.filter((route) => route.status === "已实现");
  const pending = classified.filter((route) => route.status === "待实现");
  const notApplicable = classified.filter((route) => route.status === "不适用（已决策）");
  const effective = classified.length - notApplicable.length;
  const sourceKeys = new Set(
    sourceRoutes.flatMap((route) => route.methods.map((method) => routeKey(method, route.path))),
  );
  const contractKeys = new Set(
    classified.map((route) => routeKey(route.method, route.fullPath)),
  );
  const extras = sourceRoutes.flatMap((route) =>
    route.methods
      .filter((method) => !contractKeys.has(routeKey(method, route.path)))
      .map((method) => `${method} ${route.path}`),
  );

  console.log(`contract routes: ${classified.length}`);
  console.log(`not applicable: ${notApplicable.length}`);
  console.log(`effective routes: ${effective}`);
  console.log(`implemented: ${implemented.length}`);
  console.log(`pending: ${pending.length}`);
  console.log(`source route declarations: ${sourceKeys.size}`);
  console.log(`contract-external source routes: ${extras.length}`);
  console.log("pending contract routes:");
  for (const route of pending) {
    console.log(`- ${route.id}\t${route.method} ${route.fullPath}\t${batchForRoute(route, assignments)}`);
  }
  console.log("contract-external source routes:");
  for (const route of extras) console.log(`- ${route}`);
}

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
