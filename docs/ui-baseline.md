## 基线

loom 的 UI source baseline 固定在 bb repository
`https://github.com/get-bb/bb` 的完整 commit
`fa1f44ebe9e5676004b669e48c99b3c7606466b6`（`Cut startup JavaScript by 81 KiB
and restore 5% bundle headroom (#3476)`）。这个 pin 记录在
[`contracts/bb/manifest.json`](../contracts/bb/manifest.json)，是 contract
exporter 的输入：CI 每次从它 fetch 该 commit 并逐字节复现 `contracts/bb`，所以
pin 只有一处。不能改用 bb `main`、版本错位的 npm 包或 opaque bundle。

本文件记录的是**决定**，不是自动闸门：`apps/app` 与 `ui/packages/*` 的适配由本
仓库的普通提交维护，审阅靠 code review 和这里登记的产品 surface 矩阵，没有逐文件
hash 清单或 patch ledger。上一版用 `ui/provenance.json`、
`ui/app-patch-ledger.json` 和 `ui/app-port-plan.json` 做机器校验，逐条要求
issue/owner/reason 并在每次改动时刷新 hash；维护成本高于它提供的信息（归属字段
实际退化为单一 owner 与模板化 reason），三个文件及配套的
`scripts/check-ui-provenance.mjs`、`scripts/analyze-ui-port.mjs` 和 CI 步骤都已
删除。

## App 闭包

`loom/apps/app` 是 bb `apps/app` 的 source port：它在 pnpm workspace 内，
`pnpm --filter @bb/app run build` 产出 `apps/app/dist`，服务器把它编译进
`loom-server`（`crates/server/build.rs`）并提供该客户端，它是本仓库唯一的 UI。
buildless 的 `ui/` reference client 已删除
（`ui/src`、`ui/app.js`、`ui/index.html`、`ui/style.css` 及其 esbuild 构建脚本）；
`ui/` 下保留的 `ui/packages/*` 是 app 构建所依赖的 bb 源码副本，从上述 pin 一次性
取来（取源时的 package 清单与 disposition 见
[`docs/ui-package-sync.md`](ui-package-sync.md)），此后按本仓库一方源码维护。

上游的通用 plugin SDK/host/marketplace 不进入发布 runtime；一方 Automations
产品能力则保留 UI，并由 loom-native typed API、scheduler、worker 和 ACP 边界替代
其 generic plugin RPC。

适配边界靠纪律而不是靠清单：改动 `apps/app` 或 `ui/packages/*` 就是普通提交，
reviewer 从 diff 和下面的产品 surface 矩阵判断它是否越界；不允许选择性重写界面，
也不允许通过 npm bundle 绕过源码审查。

| 层 | 当前路径 | 依赖/边界 | 决策 |
| --- | --- | --- | --- |
| App shell | `apps/app` | `ui/packages/*`（domain、thread-view、server-contract…）；HTTP 与 `/ws` 由 `loom-http`/`ws.ts` 访问同源 server | 本仓库唯一 UI；bb source port，由普通提交与 code review 维护 |
| Domain | `ui/packages/domain` | `zod` | 保留 source；作为类型和事件解码基础 |
| Server contract | `ui/packages/server-contract` | `@bb/domain`、`zod` | 保留 source；HTTP contract 仍由 `contracts/bb` 校验 |
| Thread view | `ui/packages/thread-view` | domain、server-contract、`zod` | 保留 source；纯 event-to-timeline projection |
| Client core | `ui/packages/client-core` | domain、server-contract、thread-view、core-ui、desktop-contract、`zod` | 保留 source；只接入 loom relay/client 边界 |
| Core UI | `ui/packages/core-ui` | domain | 保留 source；纯 presentation helper |
| Shared UI | `ui/packages/shared-ui` | React、Radix、icons、`clsx` 等 | 保留 source；由未来 app 按需引用，不能重复 vendor |
| Desktop contract | `ui/packages/desktop-contract` | `zod` | 保留类型边界；不恢复 Electron/desktop runtime |
| TypeScript config | `ui/packages/tsconfig` | bb build config | 精确源码复用；供 product build 使用 |
| Fuzzy match | `ui/packages/fuzzy-match` | `fzf` | 精确源码复用；保留原 27 项 package tests |
| Config | `ui/packages/config` | domain、zod | 仅公开 app 使用的 browser/build exports；server/desktop exports 不进入 closure |
| Host daemon contract | `ui/packages/host-daemon-contract` | zod | browser schema/types + typed unavailable local client；无 Hono、provider bridge 或 daemon socket |
| Mobile bridge | `ui/packages/mobile-bridge` | zod | 精确 browser-safe source reuse；不引入 native runtime |
| Browser SDK | `ui/packages/sdk` | core-ui | compile-only method surface；调用统一 typed unavailable，W-593 替换为 loom contract mapping |
| Automations UI | `ui/packages/automations` | domain、shared-ui、React、zod | 原 overview/detail/editor + 10-operation typed client；无 generic plugin runtime，W-599 接后端 |
| Static product inputs | root changelog/metadata/logo，`ui/vitest.shared.ts` | pinned blobs | 精确内容与 mode；不引入平台 runtime |
| `apps/app` assembly | `apps/app`（source port） | bb source；在 pnpm workspace 内，`pnpm --filter @bb/app run build` 产出服务 bundle | 本仓库唯一 UI；adaptation 由本仓库提交历史记录，源码不在 workspace 外另存 |
| bb plugin runtime | 不存在 | 任意 JS plugin host、发现和生命周期 | 移除；禁止加入闭包 |

十一个 adapted package、三个精确取源的 package 及其传递 workspace 依赖，就是
pnpm workspace 的依赖闭包：`pnpm --filter` 与 lockfile 是唯一的事实来源。未来 app
应引用这些 workspace package，而不是再复制一份 domain、thread-view 或 shared UI。

## 产品 Surface

以下矩阵是 app source port 的允许边界。状态是产品 surface 的决定，不等同于
contract 中仍可被解码的历史 union 分支。移除项必须同时从 app 导航、路由和
action registry 中删除，不能留下 dead navigation。

| Surface | 状态 | 实现/迁移边界 |
| --- | --- | --- |
| 项目、线程侧栏与 thread sections | 保留 | 使用 server contract 的 project/thread API；状态由 client-core 管理 |
| timeline、assistant/tool rows、reasoning 与 turn 状态 | 保留 | 使用 thread-view projection；不在 UI 伪造 provider event |
| prompt、queued messages、interaction/permission | 保留 | 通过 loom server 的既有 contract；不支持的能力明确返回拒绝 |
| Pi、ACP provider | loom-native 替代 | provider 是一等 provider ID，经 ACP/worker，不走 plugin registry |
| 项目环境、分支、diff、PR 状态 | 保留 | 使用 loom server/worker 的 environment surface |
| workspace files、attachments、file preview | 保留 | 通过 server/worker boundary；控制面不直接碰主机磁盘 |
| terminal | 保留 | 使用 worker terminal contract 和 relay；不嵌入 app 内本地执行 |
| 外观、键盘、实验项、主题、UI 偏好 | 保留 | 使用 system settings surface；provider logo 采用固定 provider 数据 |
| Automations | 保留并原生化 | 保留 get-bb overview/detail/editor、导航与交互；loom 原生实现 cron/once、agent/script、run history、恢复与 realtime，不开放通用 plugin runtime |
| plugin/extension marketplace | 移除 | 删除导航、页面、加载器、registry 请求和 plugin lifecycle |
| provider plugin 管理/安装/启停 | 移除 | provider 只通过 ACP adapter 和 loom 配置管理 |
| skills、CLI skills、skill marketplace | 移除 | 删除导航、resource actions 和安装入口；不实现 skill 文件管理 |
| desktopBrowsers | 移除 | 删除 browser control/import/capture surface；不替换为隐式成功 |
| desktop/Electron shell 专属面 | loom-native 替代 | URL client、PWA 和 desktop 共用 server origin；无 Electron runtime |
| legacy plugin wire types | 兼容解码 | 仅为历史 contract 解析/拒绝所需，不可渲染为 timeline work row 或导航 |

路由级实现状态以 [`docs/api-coverage.md`](api-coverage.md) 为准：当前 149
条有效 API 与 18 条已决策不实现的 route 不因本 UI baseline 改变。contract
中的 plugin、skill 和 desktopBrowsers 类型是来源兼容信息，不是新增产品
承诺。

## 同步流程

1. 在只读 bb checkout 中确认目标 commit，并记录完整 commit 和 commit title。
2. **W-600 已完成。** `apps/app` 与 `ui/packages/*` 的初始取源已经落地；重做这一步
   没有意义，只需按下面的流程处理后续同步。
3. 同步时逐目录比较，删除不支持 surface、隔离 runtime 并适配 loom 输入边界；
   保留原 AppLayout、sidebar、composer、thread workspace、Automations 和样式。
   不要 cherry-pick bb commit，也不要维护 patch series。
4. 若 contract 也变化，运行 contract exporter（`BB_SRC=... scripts/export-bb-contract.sh`），
   检查 route/event/wire diff，特别确认 149/149 的口径没有静默改变；pin 与
   `contracts/bb` 的变化必须在同一个 PR 里说明。
5. 运行 UI typecheck、test、build、`check:bundle` 以及 Rust contract/API
   coverage 检查。

6. **W-604 composition status.** `apps/app` is a `source-port`: the
   composer-first shell, thread workspace, first-party Settings, and direct
   Automations routes compile and build without the generic plugin SDK/runtime.
   Plugin marketplace, Skills, and desktop-browser surfaces are removed or fail
   closed, and stale persisted plugin/browser panes are pruned. The loom
   transport is in place — `loom-http.ts` over the route table, `ws.ts` on the
   public `/ws` socket ([`ui.md`](ui.md)) — and the app is the only UI the server
   serves, from its build output (`apps/app/dist`), compiled into the binary by
   `crates/server/build.rs`.

本仓库是 hard fork，没有 upstream remote，也不维护 bb patch series；同步是精确
source snapshot 加本仓库内可审计的适配提交，不是外部 patch series。
