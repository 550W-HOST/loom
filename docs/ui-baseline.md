## 基线

loom 的 UI source baseline 固定在 bb repository
`https://github.com/get-bb/bb` 的完整 commit
`fa1f44ebe9e5676004b669e48c99b3c7606466b6`（`Cut startup JavaScript by 81 KiB
and restore 5% bundle headroom (#3476)`）。这个 pin 同时用于 contract 导出和
UI package 追溯；不能改用 bb `main`、版本错位的 npm 包或 opaque bundle。

可重现信息集中在 [`ui/provenance.json`](../ui/provenance.json)。它包含：

- pinned checkout 中 `apps/app` 的 tree、`package.json` bytes/SHA-256、完整
  dependency snapshot/digest，以及 7 个 upstream package tree/package.json
  dependency digest；
- loom 中 reference app、product app 与 adapted package 的独立
  tree/package.json/digest、workspace/runtime/development dependency 闭包；
- 当前生成的 `ui/app.js` 的 bytes/SHA-256，以及每个导入项的本地路径、上游
  路径和 disposition；
- `contracts/bb/manifest.json` 的 hash、五份 JSON Schema artifact 的 hash，
  以及 contract exporter 使用的 source package 清单。

检查或重新计算当前工作树的 hash：

```bash
node scripts/check-ui-provenance.mjs
BB_SRC=/path/to/bb-at-fa1f44ebe9e5676004b669e48c99b3c7606466b6 \
  node scripts/check-ui-provenance.mjs
node scripts/check-ui-provenance.mjs --upstream /path/to/bb-at-fa1f44ebe9e5676004b669e48c99b3c7606466b6 --write
```

默认模式只读并失败于任何 local/reference source、adapted package、contract
artifact 或禁止 app import 漂移。设置 `BB_SRC` 或传入 `--upstream` 后，检查器
还会读取 checkout 的实际 `git rev-parse HEAD`，并重算 pinned `apps/app`、其
`package.json` dependency digest 和七个 upstream package tree/digest。CI 在
运行 upstream 模式前从 manifest 的 pin fetch 一个干净 checkout。`--write` 只
更新由当前 checkout 推导的 hash；source commit、路径和迁移 disposition 仍由
清单中的审阅字段决定。contract 内容本身仍由
`scripts/export-bb-contract.sh` 从该 commit 导出并由 CI 做 byte-for-byte
检查；本检查不会修改 HTTP、WS 或 daemon 协议。

## App 闭包

当前没有把 bb 的完整 `apps/app` 复制进 loom，也没有把 bundle 当作 source。
`ui/src` 是 loom-native 的 reference client，`ui/app.js` 是其可重建的服务
产物；`apps/app` 是从同一 pin 开始的独立 product shell 构建目标。
`ui/provenance.json` 同时验证 pinned checkout 的 `apps/app` source tree、
本地 product app source tree 与 dependency digest。上游 app 的
plugin SDK、plugin automation 及其他未纳入 local closure 的依赖，按下方
product surface 矩阵移除或改由 loom-native 能力替代。后续引入产品 app 时，
必须从同一个 bb commit 做 source-level 对照，逐项纳入下表允许的闭包；不
允许通过 npm bundle 绕过源码审查。

| 层 | 当前路径 | 依赖/边界 | 决策 |
| --- | --- | --- | --- |
| App shell | `ui/src` | `@bb/domain`、`@bb/thread-view`，server 由同源 relay client 访问 | 保留 loom-native 替代 |
| Domain | `ui/packages/domain` | `zod` | 保留 source；作为类型和事件解码基础 |
| Server contract | `ui/packages/server-contract` | `@bb/domain`、`zod` | 保留 source；HTTP contract 仍由 `contracts/bb` 校验 |
| Thread view | `ui/packages/thread-view` | domain、server-contract、`zod` | 保留 source；纯 event-to-timeline projection |
| Client core | `ui/packages/client-core` | domain、server-contract、thread-view、core-ui、desktop-contract、`zod` | 保留 source；只接入 loom relay/client 边界 |
| Core UI | `ui/packages/core-ui` | domain | 保留 source；纯 presentation helper |
| Shared UI | `ui/packages/shared-ui` | React、Radix、icons、`clsx` 等 | 保留 source；由未来 app 按需引用，不能重复 vendor |
| Desktop contract | `ui/packages/desktop-contract` | `zod` | 保留类型边界；不恢复 Electron/desktop runtime |
| Product app shell | `apps/app` | React/Vite entry、主题、资产与静态 workspace layout | 本阶段 source-level foundation |
| `apps/app` assembly | `apps/app` | 需要未来 route-by-route 接入，暂不切默认发布目标 | 延后；reference client 继续保留 |
| bb plugin runtime | 不存在 | 任意 JS plugin host、发现和生命周期 | 移除；禁止加入闭包 |

七个 package 的传递 workspace 依赖，以及 runtime/development 外部依赖，均由
manifest 机器计算；这让 source、build、test 三种闭包都可审阅。product app 与未来页面应
引用这些 workspace package，而不是再复制一份 domain、thread-view 或 shared
UI。

## 产品 Surface

以下矩阵是 app source port 的允许边界。状态是产品 surface 的决定，不等同于
contract 中仍可被解码的历史 union 分支。移除项必须同时从 app 导航、路由和
action registry 中删除，不能留下 dead navigation。

| Surface | 状态 | 实现/迁移边界 |
| --- | --- | --- |
| 项目、线程侧栏与 thread sections | 保留 | 使用 server contract 的 project/thread API；状态由 client-core 管理 |
| timeline、assistant/tool rows、reasoning 与 turn 状态 | 保留 | 使用 thread-view projection；不在 UI 伪造 provider event |
| prompt、queued messages、interaction/permission | 保留 | 通过 loom server 的既有 contract；不支持的能力明确返回拒绝 |
| Pi、ACP provider | loom-native 替代 | provider 是一等 provider ID，经 ACP/daemon，不走 plugin registry |
| 项目环境、分支、diff、PR 状态 | 保留 | 使用 loom server/daemon 的 environment surface |
| workspace files、attachments、file preview | 保留 | 通过 server/daemon boundary；控制面不直接碰主机磁盘 |
| terminal | 保留 | 使用 daemon terminal contract 和 relay；不嵌入 app 内本地执行 |
| 外观、键盘、实验项、主题、UI 偏好 | 保留 | 使用 system settings surface；provider logo 采用固定 provider 数据 |
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
2. 对照对应的 `apps/app` 与 `packages/*` source tree，先更新依赖闭包和
   surface 矩阵，再只移植符合 loom 边界的源码。
3. 若 contract 也变化，运行 contract exporter，检查 route/event/wire diff，
   特别确认 149/149 的口径没有静默改变。
4. 运行 `node scripts/check-ui-provenance.mjs --write`，审阅 hash、imports 和
   disposition；source pin 变化必须和 package/contract 变化在同一 PR 说明。
5. 运行 UI typecheck、test、build 以及 Rust contract/API coverage 检查。
   首屏浏览器验收可运行 `pnpm exec playwright install chromium` 和
   `pnpm ui:browser`；截图默认写入 `artifacts/product-app/`。

本仓库是 hard fork，没有 upstream remote，也不维护 bb patch series；同步是
有意的 source comparison 和最小移植，不是 cherry-pick upstream commit。
