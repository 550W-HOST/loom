# ACP 会话历史与可重建 timeline：实施交接方案

状态：设计交接，尚未实施。本文优先于旧文档中“relay log 保存完整对话”的假设。

## 1. 已确认的产品决定

- 会话历史以 ACP agent 保存的 session 为准。
- 服务端 timeline 是可丢弃、可重建的内存缓存，不增加永久 `transcripts/`，不把聊天历史放进 `domain.snapshot`。
- 缓存不存在时，查看历史依赖 worker 在线、agent 可用、session 仍存在。
- 重建后接受 ACP 提供的消息、工具记录及其顺序，不要求还原原始流式分片、时间、loom run ID 或当时的全部 UI 细节。
- `domain.snapshot` 继续保存实体状态与 session 映射；relay 继续承担实时传输、游标补帧和当前实现的实体恢复增量。
- 移除 Redis backend 及其配置、测试和部署入口，清理围绕 Redis 共享日志、多 server 节点恢复的旧叙述。保留单 server 配合多个 worker 的部署方式。

以下是本次交接的具体实施建议，不代表当前代码已经具备这些行为。

## 2. 范围与非目标

完成“打开旧 thread → 经 worker 加载 ACP 历史 → 展示 timeline”，并让实时对话不再因 relay 裁剪而冻结或丢掉已缓存的历史。

本次移除 Redis 支持，不再提供或描述 Redis 多 server 部署。不将 relay 改成数据库、不增加会话文件解析器，也不承诺恢复已被 provider 删除的历史。已有 relay/snapshot 的崩溃持久性与增量保留问题仍是独立工作，不能因本次改造就宣称解决。

### Redis 移除与文档表述

实施者须完成实际移除，不能只隐藏文档或命令行选项：

- 删除 `crates/relay/src/backend/redis.rs` 及其专用子模块、模块导出和 Redis 专用测试；移除只服务于 Redis 的依赖或辅助代码，保留 memory/disk 共用的 backend 抽象。
- 删除 `AppConfig::backend_redis`、Redis backend 构建分支、与 data-dir 的互斥检查，以及 CLI 的 `--redis-url`、`LOOM_REDIS_URL` 和相关配置透传。
- 清理 Redis 专用 CI、脚本、部署示例和测试跳过逻辑；检查整个仓库，不限于 Rust 源码。
- 删除或改写 `docs/redis-backend.md`，同步处理入链，并更新架构图、README、升级说明、CLI 帮助与运维文档。现行文档不再把 Redis 列为可选 backend，也不以共享 relay 日志暗示完整的多 server 能力。
- 统一现行叙述：agent 保存会话，server 按需加载并缓存展示；实体快照保存当前状态和绑定，relay 提供有限传输窗口及当前实现的恢复增量。去掉“relay 保存完整对话”“三份永久存储缺一不可”“data-dir 无条件完整恢复”等旧说法。
- 历史变更记录可以注明 Redis 曾存在，但必须标记已移除，不能留下看似仍有效的使用指令。升级说明明确这是破坏性配置变更：已有 Redis 部署不能直接平移其实体状态；本次不提供自动数据迁移。

最终只保留 memory 与 `--data-dir` 两种 backend。旧 `--redis-url` 必须报未知/已移除参数；若仍设置非空 `LOOM_REDIS_URL`，启动应给出明确的已移除错误，避免旧部署悄悄落到 memory。该环境变量检查仅用于迁移诊断，不再是配置入口。

ACP 不拥有的审批状态、排队状态等继续读取实体视图。仅存在于旧 relay 窗口中的诊断、调度失败等，不承诺永久保留；残存记录可以辅助展示，但不能冒充完整会话。

首版一个 thread 的权威会话对应一个当前 session binding。若切换 provider、workspace 或 session，旧 binding 的缓存失效，历史展示切换到新 session，不自动拼接多个 session。若以后需要跨 session 历史，另行增加持久化的 binding 列表；不能从有限 relay 日志猜测该列表。

## 3. 代码事实与改动入口

| 入口 | 当前行为 | 本次用途 |
| --- | --- | --- |
| `crates/server/src/http.rs::thread_domain_events` | 枚举 retained relay 事件，以窗口内下标生成 sequence | 替换会话读取来源；不再把 retained log 当历史 |
| `crates/server/src/http.rs::thread_timeline` | 从上述事件构建消息、思考和工具行 | 复用可复用的投影逻辑，支持历史来源与实时覆盖层 |
| `crates/server/src/assistant_timeline.rs`、`tool_timeline.rs` | 合并流式消息、工具事件 | 为 ACP 回放提供投影基础，补齐跨历史消息的边界处理 |
| `crates/worker/src/acp/session.rs` | 继续对话时抑制 load 历史，随后发送 prompt | 保留此语义；新增独立的只加载历史路径 |
| `crates/worker/src/acp/sessions.rs` | ACP 协商、独立 session/list 连接 | 参考其生命周期与 capability 检查，不读 agent 私有文件 |
| `crates/server/src/host_rpc.rs` | host 身份校验、请求关联、超时、迟到回复丢弃 | 复用请求模式；历史内容需要有界分批传输 |
| `crates/provider-protocol/src/lib.rs` | server/worker 协议 | 增加历史加载请求、分批回复、完成与错误语义 |
| `crates/domain/src/thread.rs` | 保存 session ID、agent/cwd binding | 补足历史请求的 host 身份与兼容迁移 |
| `ui/packages/client-core/src/timeline/timeline-merge.ts` | 按 sequence 合并页面 | 增加缓存代次，阻止重建前后的游标与页面混用 |

检查 `thread_domain_events` 的所有调用方，包括 events、output、工具详情和其他派生查询；会话内容查询应共享同一缓存与代次。读取 loom 自有运行事实的接口须明确保留其语义，不能拿 ACP 历史伪造这些事实。

## 4. 历史加载协议

概念上的请求：

```text
LoadHistory {
  request_id,
  host_id,
  thread_id,
  binding_revision,
  provider_spec,
  provider_session_id,
  cwd,
  deadline
}

HistoryChunk { request_id, batch_index, entries }
HistoryComplete { request_id, batch_count }
HistoryFailed { request_id, code, message }
```

消息名称由实施者按现有代码风格确定，但必须满足以下约束：

1. server 从已保存的 binding 决定目标 host、agent、session 和 cwd；客户端不能提交任意 provider 命令。
2. worker 仅 initialize + load/replay，不发送 prompt，不创建新 run，不触发模型选择、审批执行、自动化或新 session 回退。
3. v1 检查 `loadSession` 并调用 `session/load`；v2 检查 session capability，调用 `session/resume`，显式设置 `replayFrom: { type: "start" }`。
4. worker 将回放转换成 provider 无关的历史条目。可复用现有 translator，但输出不经过 `ProviderReport`，不发布 `ThreadRunEvent`，不更新实体状态。
5. 回复只接受来自目标 host、匹配请求与 binding 的数据；批次须可检测缺失、重复和乱序。完成消息前不提交新基线。
6. 规定单批字节上限、单次加载总量上限、总超时和并发上限；超过限制显式失败，不静默截断后标记完整。首版可以全量读取 ACP，再通过 HTTP 缓存分页；不宣称 ACP 本身支持历史分页。
7. 取消、超时、thread 删除、binding 改变后，清理等待者和临时批次，关闭专用 ACP 连接及其子进程；迟到消息不得复活缓存。
8. 如果请求经过 host relay，其重放不能无限重新启动加载；worker 检查 deadline，并对活动请求去重。

### 回放完成：先解决的协议前置条件

仓库锁定的 pi-acp 提交为 `3ec393fa04c25d3ef054c64781f93f5a842918ca`。其中 load/resume 先响应，再发送历史；ACP v2 schema 对 `replayFrom: start` 的描述则要求先回放、后响应。现有 run 路径也专门抑制响应后的迟到历史。

首选修正 pi-acp 的历史加载时序，并更新锁定依赖和兼容测试，使响应成为可靠的完成边界。这项改动需在 pi-acp 仓库实施；本仓库随后升级到包含修复的提交。不能直接修改 Cargo 缓存目录。

若必须支持响应后回放的 agent，则需该 adapter 提供明确、可测试的回放完成协议，再实现适配；普通 commands 更新或“若干毫秒没有新消息”不能被当作通用结束信号。无法判定完整性时返回明确错误，不缓存成成功结果。

还要验证 ACP SDK 在收到响应时，前序 notification 的应用已经完成。若回调并发执行，需在接收链路中保留顺序并设置处理屏障，不能只等待网络响应。

**已落地（2026-09-20）。** pi-acp 已在 `2f13a18` 修正：v1 `session/load` 与 v2 `session/resume` 都先发布历史（连同标题与命令广告）再返回响应；`session/new` 保持响应在前，因为那里的 session id 由响应产生——原注释里"客户端还没登记这个 session id"的理由只对 `session/new` 成立，load/resume 的 id 是客户端给的。边界由两条测试固定，且都不做轮询即可断言回放已应用：

- `crates/pi-acp/tests/acp_agent.rs::load_history_is_complete_when_the_response_returns`（v1）
- `crates/pi-acp/tests/acp_v2.rs::v2_resume_history_is_complete_when_the_response_returns`（v2）

本仓库的 `Cargo.lock` 已钉到该提交（`6558128`），loom-worker 全量测试在该 pin 下通过。

SDK 侧**无需额外屏障**：`agent-client-protocol` 2.0.0 的 `concepts/ordering.rs` 明确 "the dispatch loop waits for each handler to complete before processing the next message"，且 `on_receive_request` / `on_receive_notification` 回调都在 dispatch loop 内运行。所以"响应返回时前序通知已应用"是 SDK 保证，不是竞态；上面第 3 段担心的"回调并发执行"在 SDK 这一层不成立。唯一前提是加载方用有序注册（`on_receive_notification`）而不是把回放丢进 spawned task 自行并发处理。

顺带修掉的既有破损：`3ec393f` 给 `session/new` 的 `configOptions` 加了每个模型的 thinking ladder，却没有重新生成 golden，`acp_frame_sequence_matches_golden` 在干净树上就是红的；已在 `4d0adab` 按该测试文档规定的方式重新生成。这条与本方案的 load/resume 时序无关。

## 5. binding 与恢复

缓存身份至少包括 `(thread_id, host_id, agent, session_id, cwd, binding_revision)`。

当前 `ProviderSessionBinding` 只有 agent、cwd 和绑定时间，不能单独证明 session 属于哪台 host。实施时在实际 run 学到 session identity 时一并记录所属 host；新增字段按快照兼容规则处理，并随相应实体事件恢复。

旧快照缺少 host 信息时，只能在已有持久状态能明确证明归属的情况下迁移。无法确定则提示需要重新绑定，不能把相同路径的另一台机器当作 session 原主机。后续 environment 或 provider 变更不得悄悄改变旧 session 的查找位置。

恢复服务端实体时不自动加载所有 session。仅在访问会话内容时按需加载；实体列表仍可在 worker 离线时正常使用。

## 6. 缓存与实时事件如何衔接

每个缓存条目包含：binding、generation、ACP 历史基线、实时覆盖层、完整性/刷新状态和最近访问时间。基线与覆盖层均只在内存中保存。

### 首次打开已完成的旧 thread

1. server 检查实体及 binding，合并同一 binding 的并发加载请求。
2. 请求 worker 加载，临时批次暂不替换现有页面。
3. 收到完整结果后，再检查 binding 和会话活动版本是否改变。
4. 校验通过则原子安装新基线，生成新 generation，返回最新一页。
5. 加载失败保留旧缓存并标记过期；没有缓存则返回历史不可用。

### 新 run 与正在进行的 thread

- 实时事件进入独立的内存覆盖层。写入点必须在应用事件接收/发布链路内，不能只靠可能落后于 relay 裁剪的异步 reader。
- 初版不要求加载历史与 prompt 并发安全：worker 对同一 provider session 串行协调两者。正在 prompt 时，不另开一个会影响该 session 的 load 操作。
- run 在进行时已有基线：展示基线加本次实时覆盖层。
- run 在进行时没有基线：明确显示“历史等待当前运行结束后加载”，可以展示已收到的当前运行内容，但不能标记为完整历史。
- 新 prompt 到达正在加载的 session 时，由 worker 协调取消加载并清理连接后再运行，或有界等待加载结束；必须明确实现一种策略，不能与未结束的 loader 同时操作 session。建议以新 prompt 为优先。
- run 结束后，在会话被查看时重新加载 ACP 基线；成功后替换 provider 会话部分并移除已被覆盖的实时内容，更新 generation。
- 加载过程中发生新 run 或 binding 变化，则该结果不能被标记为最新完整基线；丢弃临时结果并在适当时机重试。

不尝试用文本相等来跨基线去重，也不要求 ACP ID 与实时 loom item/run ID 一致。完整回放后的整体替换避免“相同回答重复出现”和错误匹配重复文本。

缓存命中时无需每次 HTTP 请求都调用 ACP。刷新触发点为缓存缺失、binding 改变、运行结束后的待刷新状态以及用户显式刷新；agent 在 loom 外部发生的变化通过显式刷新获取，首版不引入后台扫描。

缓存按总字节数和 thread 数限制并做 LRU 淘汰；加载中的临时数据也计入预算。活动覆盖层同样必须有上限，达到上限可丢弃并标记内容不完整、等待回放，不能裁剪后仍声称完整。thread 删除时清理缓存和等待请求。本次不顺带删除 agent session。

## 7. HTTP、分页与 UI

现有以 retained 窗口下标生成 sequence 的行为会在窗口满后使 `maxSeq` 停止增长，必须随本次改造移除。

为 timeline 响应增加 `generation` 与独立的历史状态；请求携带所持 generation。分页游标实际上是 `(generation, sequence/anchor)`：

- 同一 generation 内，sequence 单调增长；同一消息的流式更新推进其 `sourceSeqEnd`，行 ID 保持稳定。
- 基线重建、缓存淘汰后重新加载、服务端重启或 binding 切换，都生成不复用的新 generation。
- generation 不匹配的增量或旧页请求返回明确的 reset 结果；UI 清空旧代次页面并重新请求最新页，不能继续拼接。
- 为旧客户端明确兼容行为：携带增量游标却不携带 generation 时要求重取最新页，不解释为当前代次游标。
- 异步旧请求晚于新 generation 返回时，UI 丢弃旧结果，防止页面回退。
- 离线但有缓存时显示缓存与“无法刷新”状态；没有缓存时展示不可用原因，不用空数组表达“没有聊天记录”。

历史状态至少能表达 loading、ready、stale、unavailable，并独立标识内容是否完整及机器可读的原因。一个真实空 session 是 ready + 空内容，区别于不支持、超时、缺失或仍在加载。

加载可能较慢：HTTP 返回 loading 状态，由客户端有界重试；多个客户端共享同一后台加载任务。成功/失败时通过现有公开缓存失效机制通知客户端，不能广播整份历史。

ACP 缺失的历史时间允许为未知，不能把加载时刻冒充消息发生时间。协议和 UI 如有必填限制需同步调整。用于投影的本地分组 ID 不能伪装成可操作的真实 loom run ID，历史行不得因此出现错误的 run 操作入口。

## 8. 实施顺序

1. **ACP 回放闭环**：修正/确认完成边界；编写 v1/v2 回放测试，证明只 load、不 prompt、消息顺序与边界正确。此项通过前，不开始依赖完整结果的 UI 集成。
2. **binding 与 worker 协议**：持久化 host 归属，加入有界历史请求、分批结果、取消和错误，验证无 run/relay 副作用。
3. **服务端缓存**：实现按需加载、并发合并、原子基线替换、实时覆盖层、活动 session 协调与 LRU。
4. **HTTP 与客户端一起修改**：接入 timeline 和相关会话读取接口，引入 generation、reset 和可用性展示。同步更新契约与生成类型，不能只修改 handler JSON。
5. **移除 Redis**：删除 backend、配置、CLI、专用测试和部署入口，完成旧配置启动诊断；保留 memory/disk 与多 worker 功能。
6. **文档与验收**：按第 2 节清理 Redis 与共享日志旧叙述，修正 `domain-persistence.md`、`provider-strategy.md`、`acp-adapter.md` 等关于“relay 保存完整会话”的现行承诺，并记录支持的 agent 能力与边界。

## 9. 必须通过的验收

| 场景 | 预期 |
| --- | --- |
| 老对话完全超出 relay 保留窗口；重启 server 后打开 | 使用快照里的 binding 经 ACP 恢复消息/工具记录，不依赖旧 shard 帧 |
| 只清理内存历史缓存 | 下次查看可经 ACP 重建，不损坏实体或 agent session |
| 其他 thread 大量写入同一 shard | 已缓存 timeline 不丢行；当前 run 新消息仍能超过旧游标被读取 |
| v1 load 与 v2 replayFrom=start | 顺序正确、用户与助手角色正确，多轮助手消息不被合成一条 |
| load 响应前后的 notification、空历史、回放中断 | 完成判定可靠；失败不提交半份历史，不采用静默等待阈值 |
| 仅打开历史 | 没有 prompt、新 run、重复实时消息、实体状态修改或执行审批 |
| 十个客户端同时打开同一会话 | 同一 binding 仅一个加载任务，成功后共享缓存 |
| 运行中打开、加载时开始 run、运行完成后刷新 | 无并发破坏 session、旧结果覆盖新状态或基线/覆盖层重复消息 |
| 缓存重建前的分页请求迟到 | generation 校验阻止错页合并；UI 重置并重取 |
| worker 离线、agent 无能力、session 已删除、cwd 缺失 | 显示具体不可用原因；已有缓存可以显示为过期，不回退成假完整日志 |
| host/agent/cwd/session 改变 | 旧请求结果被丢弃；不会加载另一台 host 上碰巧同名的 session |
| 超大历史、慢 worker、超时、取消、thread 删除 | 内存和任务有界，连接/子进程/等待者得到清理 |
| v1 无稳定消息 ID 或原始时间 | 能显示顺序正确的历史，不伪造原始时间与真实 run 身份 |
| Redis 移除后的构建、CLI 帮助与部署文档 | 无 Redis backend 或可用配置入口；残留引用仅限明确的迁移诊断、移除说明和历史记录 |
| 使用旧 Redis CLI 参数或非空环境变量启动 | 明确失败并说明参数/配置已移除，不静默使用 memory |
| memory、data-dir 与单 server 多 worker | 原有传输和实体恢复检查通过，不因删除 Redis 而删除共用能力 |

分层运行 worker 协议测试、server HTTP/缓存测试、客户端分页合并测试，再用真实 pi-acp 做“多轮对话 → 超出 relay 窗口 → 重启 server → 加载历史”的端到端验证。

交接时工作区已有 `crates/server/src/http.rs` 的未提交测试 `a_thread_that_fills_its_shard_still_reports_its_newest_rows`，它针对旧 sequence 饱和问题。保留并适配该回归测试；不要覆盖已有工作区修改。

## 10. 复核补充（对代码逐条核对后的增补）

本节来自一次独立复核：第 1—9 节引用的代码事实全部核对过，未发现事实性错误。以下是要补进正文的内容，按阻塞项、遗留项、实施细节分组。

### 10.1 `sequence` 的分配规则必须写明（阻塞，补进 §7）

§7 已确认"以 retained 窗口下标生成 sequence"必须移除，但没有规定新来源。`crates/server/src/http.rs:1776` 的 `index as u64 + 1` 正是原始缺陷，而内存缓存有两种方式让同一缺陷复发：

- 缓存有 LRU 字节上限（§6），淘汰会前移条目下标；
- 基线与覆盖层合并时若以数组位置编号，同一次合并内的顺序变化即改变旧条目的号。

要求：条目在**安装进基线或追加进覆盖层时**分配一个不可重算、per-generation 单调的序号。淘汰不重新编号；若必须淘汰，按 §7 更换 generation 并明确通知。这与"身份在写入时确定，不由读取时的位置推导"是同一条原则，只是适用范围缩到 generation 内。

验收补一行：同一 generation 内，任何缓存淘汰或条目重排之后，已有条目的 sequence 不变。

### 10.2 两条恢复读路径仍在读有界 relay（遗留，补进 §2 或 §8）

`thread_domain_events` 不是唯一读会话历史的入口。恢复期还有两处：

| 入口 | 行为 |
| --- | --- |
| `crates/server/src/state.rs:888` `latest_active_run_id` | `retained_scope(Scope::Thread)`，判断重启后 thread 是否仍 active |
| `crates/server/src/state.rs:926` `recover_run_flags` | `retained_scope(Scope::Thread)`，找回 terminal |

它们读的是 **loom 自有的领域事件**（`ThreadStatusChanged`、terminal `ThreadRunEvent`），ACP 回放里没有这些，因此不能改走缓存。它们仍受每 shard `max_len` 截断，属于**恢复正确性**问题（判定错误，而非显示错误）。这与 §2 所说的"增量保留问题"是同一件事，但应在此点名，避免实施者以为 timeline 改完即干净。

### 10.3 回放完成时序：v2 有规范要求，v1 没有（补进 §4）

§4 的"协议前置条件"判断正确，补充两点：

- 规范要求只出现在 v2 的 `session/resume` 方法文档："If `replayFrom` is set, the agent should replay conversation history **before responding**"（`agent-client-protocol-schema-1.5.0/src/v2/agent.rs:5315-5317`）。
- v1 的 `session/load` 在 schema 里**没有写时序**（同 crate `src/v1/agent.rs:1163-1166` 只指向 protocol docs 页面）。

因此修正 pi-acp 时 v1、v2 都要处理；loom 不应假定 v1 已经合规。

### 10.4 双连接串行化需要实测依据（补进 §8 第 1 步）

§6 要求 worker 对同一 provider session 串行协调 load 与 prompt，§4 要求加载走专用连接并关闭其子进程。但历史加载使用的是一条**独立的 ACP 连接**，而 pi-acp 每个连接会启动/嵌入一个 agent。"同一 session 在两个连接上并发操作"的行为目前没有依据，需在实施顺序第 1 步实测：

- load 进行中 prompt 到达；
- prompt 进行中 load 到达；
- 第二个连接 `session/load` 对第一个连接运行中 turn 的影响。

此项没有结论前，§6 的协调策略无法验证。

### 10.5 超长会话的悬崖（补进 §2 已知限制）

"只能全量回放" + "缓存不持久" + "超限显式失败，不静默截断"三条合起来产生一个硬边界：**超过单次加载上限的会话，在 server 重启后永久不可读**——缓存已失，唯一来源是超限的全量回放，而超限必须失败。

需要在文档中明确产品行为（明确提示"历史过大，暂不可加载" / 提供降级分页 / 提高上限），并写入已知限制。这是"可丢弃缓存"这一决定最直接的代价。

### 10.6 搜索是受限能力（§3 未覆盖）

`search_threads` 遍历 registry 中**每个** thread 并调用 `thread_search_matches`（`crates/server/src/http.rs:3076` 起），后者读取该 thread 的历史。缓存是按需加载 + LRU 淘汰的，因此：

- 未加载过的 thread 搜不到；
- 搜索覆盖范围取决于缓存当时恰好包含哪些 thread，不确定。

它今天也已经受 relay 截断影响（只扫 `ThreadMessageAdded`）。请在文档中把搜索列为一个明确受限的能力（例如"只覆盖已缓存的 thread"），或另立持久索引——后者与"不增加永久存储"冲突，需要产品决定，不能留成隐藏死角。

### 10.7 UI 契约漂移要记录（补进 §7）

`ui/packages/client-core/src/timeline/timeline-merge.ts` 属于从 bb 移植的层，仓库有 `docs/ui-package-sync.md` 管其溯源。§7 引入 generation、reset 语义与"带游标不带 generation"的兼容分支，会改变该文件的行为。按现有做法记录这次漂移，避免下次同步时冲突。

### 10.8 未提交改动先拆分（补进 §8 第 1 步之前）

当前工作区 `crates/server/src/http.rs` 的未提交 diff 同时包含两项内容：

- 一份无关的 `contextWindowUsage` 投影功能（约 300 行插入）；
- §9 末尾提到的回归测试 `a_thread_that_fills_its_shard_still_reports_its_newest_rows`。

建议先拆成两个提交（前者独立落地，后者按新架构重写），再开始本次改造，否则第 1 步即会与未提交工作冲突。

### 10.9 `thread_domain_events` 调用方清单（补进 §3）

§3 要求"检查所有调用方"。完整清单如下，并标注本次归属：

| 行 | 函数 | 是否会话内容 | 归属 |
| --- | --- | --- | --- |
| 1924 | `thread_output` | 是（工具/输出详情） | 改走缓存 |
| 2939 | `thread_conversation_outline` | 是（会话大纲） | 改走缓存 |
| 3000 | `thread_prompt_history` | 是（用户输入历史） | 改走缓存 |
| 3125 | `thread_search_matches` | 是（搜索） | 受限，见 §10.6 |
| 3459 | `retry_thread` | 是（重试上下文） | 改走缓存；须与"新 prompt 优先"协调 |
| 3952 | `thread_timeline` | 是（主路径） | 改走缓存 |
| 5382 | `thread_event_rows` | 是（provider 事件行） | 改走缓存 |
| 5466 | `thread_turn_summary_details` | 是（turn 详情分页） | 改走缓存 |
| 5738 | `thread_has_goal` | **否**（loom 领域事实） | 保留 relay 语义 |
| 5763 | `thread_has_active_plan` | **否**（loom 领域事实） | 保留 relay 语义 |

最后两行对应 §3 末句"不能拿 ACP 历史伪造这些事实"，但它们与会话读取**共用同一个 helper**。实施时应先把 helper 拆成两条读路径（会话条目 / 领域条目），否则"保留语义"无法表达。

### 10.10 Redis 移除的完整面（补进 §2）

除 §2 已列项外，实际需要处理的位置：

- 代码：`crates/relay/src/backend/redis.rs`、`crates/relay/src/backend/redis/resp.rs`、`crates/relay/tests/redis.rs`，以及 `crates/relay/src/lib.rs`、`crates/relay/src/backend.rs`、`crates/server/src/cli.rs`、`crates/server/src/run.rs`、`crates/server/src/state.rs` 中的引用。
- 文档：`docs/redis-backend.md`（删除或改写）、`docs/upgrades.md`、`docs/architecture.md`、`docs/domain-persistence.md`、`docs/process-model.md`、`docs/handoff.md`、`docs/ci.md`、`docs/remote-access.md`。
- 部署与 CI：`containers/docker-compose.yml`、`containers/loom-server.Dockerfile`、`.github/workflows/ci.yml`、`scripts/verify-release-binaries.sh`。
- 其余：`README.md`、`Cargo.toml`（仅注释提及 `--redis-url`）。

无 Redis 客户端依赖需要移除（RESP2 是手写的）；`crates/relay-hub/src/lib.rs` 中的 Redis 只是注释举例。
