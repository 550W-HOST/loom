# 会话历史改造：复核与收敛建议

审查基线：`f4b7639`，2026-09-20。本文是重新规划的建议，不是新增实施授权；不覆盖已确认的 Redis 移除、实体持久化和传输补帧职责。

> **后续状态（2026-09-21 注）。** 本文是复核当时的快照。`docs/sqlite-persistence-plan.md`
> 的阶段一至三在本文之后落地，§2 的核对表因此多了一列「2026-09-21 复测」，§6 的
> 验收在 §7 逐条对照——正文其余部分保留原样，以记录当时的判断。
>
> 当时"本轮为静态审查，未重新执行完整门禁"这一句也已不成立：每个阶段都跑了
> `cargo test --workspace --locked`、`clippy -D warnings`、`cargo +1.88 check
> --workspace --all-targets --locked`、`cargo fmt --check` 与前端 `typecheck`/`test`，
> 当前全绿。本文末尾"是否要求跨机器续聊仍待答复"的问题仍未变：那件事仍未开始。

## 1. 为什么越做越复杂

最初问题是：有界 relay 不能承担长期会话历史。随后选择“只保留可丢弃内存缓存，通过 ACP 重建”，实际上把查看历史变成了远程执行操作，需要处理 worker 在线性、agent 启动、session 占用、加载完成、重试、全量大小限制和缓存代次。

这些机制多数是在实现所选方向，不都是无关扩展。但“代码按计划完成”和“产品目标成立”不是一回事。最近提出的离线查看要求，与先前接受的“没有缓存且 worker 离线时历史不可用”相冲突，必须先改变这项产品约定，再决定存什么，不能一边保持旧约定一边增加镜像和同步系统。

另一个问题是实施并未完全统一读取来源，目前仍有 ACP 缓存与 retained relay 两条会话读取路径。

## 2. 当前代码核对结果

| 项目 | 当前实际状态（基线 `f4b7639`） | 处理建议 | 2026-09-21 复测 |
| --- | --- | --- | --- |
| 主 timeline | 经 `read_thread_history` 读取内存缓存，缺失触发 ACP 加载 | 保留投影与加载适配能力，重新确定读取是否应依赖在线 worker | 已改为读库：`AppState::stored_view` 以库行为准，内存只留未落盘的 overlay 与加载登记；ACP 加载只在尚未同步、显式刷新、run 结束后触发 |
| 输出、会话大纲、prompt history、搜索、重试 | `http.rs` 中仍调用读取 retained relay 的 `thread_domain_events`；project prompt history 也使用它 | 明确哪些读会话、哪些读运行事实；会话接口统一来源，不能只改主页面 | `thread_domain_events` 已不存在；这些路径走 `thread_recorded_messages` → `state.stored_view`。会话内容统一读库，relay 只承担传输与 loom 自有事实 |
| 历史落盘 | 没有 server session 文件镜像或持久历史副本 | 离线方案尚未实施，不要误认为已经同步过 | 已落库：`thread_history` / `thread_history_row`（schema v4），`domain.snapshot` 文件的读写路径整体删除 |
| Redis | 已移除，保留旧配置启动诊断 | 保持现状 | 不变（仍是移除状态加启动诊断） |
| relay shutdown | `14d4b1f` 增加关闭、flush 及错误传播 | 已不属于“尚未调用 flush”，不重复发起同一修复 | `DiskBackend`/`shard-*.log` 已删除，帧同步写库；`AppState::shutdown` 排空写队列并在最后记 clean stop，flush 失败会让 shutdown 失败 |
| generation | 缓存每次构造从 1 开始；客户端新增“响应 generation 更小则丢弃” | server 重启后新响应可能一直被当成旧响应；必须修复身份语义 | `cacheInstance`+`generation` 已删除，改为持久的 `historyRevision`（schema v2、契约字段、客户端比较规则），"重启后新响应被当旧响应"不再存在 |
| 分页请求 | 响应带 generation，请求仍只带旧 sequence/anchor | server 会先用旧游标过滤新基线；客户端看到新 generation 时，收到的可能已是错误切片 | 请求现在带 `historyRevision`；服务端不再把 revision 不匹配的请求当作位置，客户端按 revision 不同即重置 |
| 不可用状态 | 没有完整的用户显式重试入口 | 临时故障不能只能靠重启或 LRU 淘汰恢复 | 失败后 30 秒退避自动重试，另有 `POST /api/v1/threads/{id}/history/refresh` 与线程菜单的 `Refresh history` |
| 旧 binding | 缺 host 的已知 session 仍可能走新建路径 | 先阻止静默换 session，再补明确的恢复/绑定动作 | 缺 host 的绑定读作“无绑定”（`HistoryUnavailable::NoBinding`），加载被拒并给出原因；端到端用例断言加载不重新绑定 session |
| worker 历史加载 | `start_history_load` 直接派生独立任务；server 的部分读取分支避开活动 run | 不能把读取前的检查当作 worker session 互斥，检查后新 run 仍可能到达 | 检查与加载声明现在同在 dispatch 取用的 lifecycle 锁下（`crates/server/src/history.rs`），堵掉了“检查后新 run 到达”；加载仍是有界并发（`Busy` 拒绝而不排队），run 在飞时读方拿到 overlay 与 `RunInFlight`。加载跑起来后仍有新 run 的可能，因此落库前的水位线比较会丢弃已过期的基线（`a_baseline_the_thread_outgrew_is_dropped_instead_of_installed`） |

本轮为静态审查，未重新执行完整门禁，也未验证真实 agent 的跨机器恢复能力。

## 3. 先确认唯一的产品边界

两个目标必须分开：

1. **离线查看**：server 重启、原 worker 离线时，仍能查看已成功同步的历史；继续对话仍使用原 worker。
2. **跨机器续聊**：把 agent 的原始 session 搬到另一台 worker，连同必要的 agent 状态与 workspace 条件恢复执行。

建议本轮只做目标 1。目标 2 需要先验证各 agent 的导出/导入能力、session 映射、cwd 与 workspace 依赖，不能由“ACP 可以 load”推导为“ACP 可以导出并移植原始 session”。单纯复制一个文件也不能证明新机器能继续运行。

如果用户要求目标 2，先停止存储实现，做一个指定 agent 的迁移验证，再出迁移方案；不要把它藏在历史缓存的实现里。

## 4. 若确认目标 1，推荐的最小结构

```text
原 worker 的 agent session
          │ ACP 回放，同步到 server
          ▼
server 持久会话副本 ──► 所有会话内容查询 ──► UI
          ▲
          └─ 内存仅用于加速；丢弃不影响已同步内容
```

这里的持久副本是 ACP 输出的、provider 无关的会话内容，不是 agent 私有 session 文件。它不能用于承诺跨机器续聊。名称可以是 history 或 mirror，但它实质上就是持久 transcript 的职责，必须明确这是对旧“不做持久 transcript”决定的调整，不能换个名字掩盖。

关键约束：

- 一个 thread/binding 只有一个 server 会话读取来源；磁盘副本与内存是同一份数据的两种访问层，不分别做一套权威存储。
- 已同步历史的普通 GET、翻页和搜索不启动 ACP，不要求 worker 在线。未同步的旧会话可以按需安排首次同步，并明确显示尚未同步。
- 同步始终是 worker → server，不将 server 的展示副本反向写入 agent session。
- 首次导入、会话结束和用户显式刷新触发同步。只有“对话前同步”会漏掉随后产生的本轮内容，不能作为唯一同步点。
- 对话中保持现有实时展示；最小版本只承诺最后一次成功同步的持久基线。若还要求突然断线后保留每个已显示分片，需要另行确认增量落盘与确认语义，不能默认附带承诺。
- 新副本完整写入并校验后原子替换；失败保留旧副本，记录同步时间与错误。持久数据不能随内存 LRU 淘汰。
- 不在每次读请求上运行同步，不每次发送前无条件全量 load，不为已知不可恢复错误持续轮询。
- 原始 session 仍只由 agent 管理，实体快照与 relay 职责保持现状，Redis 不恢复。

存储实现优先延伸当前已完成的回放条目和投影，不另外创建第二种消息模型。本轮不同时引入通用数据库、双向同步或 session 文件备份系统。开始实现前明确磁盘配额、单次历史大小限制以及 thread 删除清理规则；超限必须保留旧副本并说明状态。

## 5. 收敛后的实施顺序

### 阶段 A：停止继续扩大架构，修复当前确定性问题

1. 缺少可信 host binding 的已有 session，发送/重试明确报错，不自动建新 session，不覆盖旧映射。
2. 统一游标身份：区分 server/cache 实例与同一实例的 revision。请求携带身份，server 在切片前验证；UI 用请求上下文拒绝迟到结果，不能跨重启仅用整数大小判断新旧。
3. 加载任务使用独立 token；绑定改变、删除、取消和晚到结果核验 token。worker 在同一 session 上协调 prompt 与 load，不能仅靠 server 检查。

这些修正不依赖最后选择内存缓存、持久副本还是 session 迁移。

### 阶段 B：只实现一个完整的离线查看切片（须先确认目标 1）

1. 复用现有 ACP loader 和条目格式，成功同步结果落盘，失败保持上次成功副本。
2. server 重启直接读副本；主 timeline 在 worker 离线时仍能查看、翻页。
3. 提供一个“刷新历史”动作和最后同步状态，临时失败后能够恢复。
4. 验证同一内容在缓存淘汰、server 重启与 worker 断线后三种情况下仍可读取。

先证明这一条用户路径，不先建设自动同步平台。若此切片都无法解释清楚，不向其他接口扩展。

### 阶段 C：统一读取并删除旧路径

1. 迁移 output、outline、prompt history、搜索等会话内容查询。重试读取用户输入时也需明确相同来源。
2. 真实 run 身份与审批等 loom 自有事实保留其原数据来源；缺少原 run 信息的 ACP 行不伪造关联。
3. 删除以 retained relay 冒充完整会话的读取路径，以及围绕“丢了缓存就无法查看”的过时 UI 行为。
4. 更新现有设计文档，使它只表达最终决定；历史过程移至变更记录，不再继续堆叠互相矛盾的补充节。

## 6. 最终验收只围绕用户行为

若选择目标 1，交付必须同时满足：

- 完成一轮并同步成功，关闭 worker、重启 server，历史仍可查看与翻页。
- relay 被其他线程写满，已同步历史和输入历史不消失。
- 清空内存缓存不删除磁盘历史，也不要求重新启动 agent 才能查看。
- 同步失败/中断，旧副本仍能读；用户重试后恢复。
- 正在对话时刷新，不启动第二个会破坏 session 的操作。
- 原 binding 不明时明确提示，不悄悄换会话。
- 新旧代次页面不会混合；server 重启后的有效响应不会永久被丢弃。
- 没有新增原始 session 双向同步、多 server、跨 worker 恢复或第二套 UI 消息模型。

是否要求跨机器续聊仍待本轮用户答复；答复前不启动阶段 B/C 或原始 session 文件同步。

## 7. 验收对照（2026-09-21）

按 §6 的八条，对现在的实现逐条给出依据。测试名都能用
`cargo test --workspace --locked` 跑到；这一节不新增实现，只是把"用户路径是否成立"
逐条落到证据上。

1. **完成一轮并同步成功，关闭 worker、重启 server，历史仍可查看与翻页。**
   `crates/server/tests/history.rs::a_conversation_outside_the_relay_window_is_loaded_from_the_agent`
   用一个新进程（复制上一进程提交的字节，避免两个写者）重启，重启后**在加载之前**就从库里
   读到会话；`crates/loom/tests/history_durability.rs::a_killed_server_leaves_the_conversation_it_committed`
   用真实进程 SIGKILL 覆盖"没有干净关闭"这一半。游标/翻页：
   `http.rs::a_cursor_from_another_numbering_is_a_reset_not_a_slice`。
2. **relay 被其他线程写满，已同步历史和输入历史不消失。**
   `http.rs::a_thread_that_fills_its_shard_still_reports_its_newest_rows`（旧编号会随窗口
   饱和、页面看起来冻住，现在不会）；输入历史走 `thread_recorded_messages` →
   `state.stored_view`，与 relay 还剩几帧无关。
3. **清空内存缓存不删除磁盘历史，也不要求重新启动 agent 才能查看。**
   内存里现在只有未落盘的 overlay 与加载登记；第 1 条那个"新进程 + 加载前读取"就是缓存
   全空的样子，store 侧的 `the_numbering_survives_a_reopen` 固定行号跨重开继续。
4. **同步失败/中断，旧副本仍能读；用户重试后恢复。**
   `history.rs::a_failed_load_leaves_no_conversation_behind`、
   `history.rs::a_failed_load_waits_before_the_next_read_retries_it`、
   `history.rs::a_refresh_asks_for_the_conversation_again`；旧基线保留由 store 侧的
   `a_failed_sync_keeps_the_baseline_and_records_why` 固定（只写 `last_error`）。
5. **正在对话时刷新，不启动第二个会破坏 session 的操作。**
   `history.rs::a_load_yields_to_a_run_in_flight`、`history.rs::concurrent_callers_share_one_load`；
   第 1 条那个端到端用例末尾还断言"加载是读：不派发 run、不改状态、不重新绑定 session"。
6. **原 binding 不明时明确提示，不悄悄换会话。**
   读取侧：缺 host 的绑定读作"无绑定"并拒载（`HistoryUnavailable::NoBinding`），store 侧
   `an_incomplete_binding_reads_as_no_binding`。发送侧见 §8 阶段 A1——它**不会**跨机器
   或跨工作区错接，但缺绑定时是开新会话而不是报错，这一条只做到一半。
7. **新旧代次页面不会混合；server 重启后的有效响应不会永久被丢弃。**
   身份是持久的 `historyRevision`（schema v2）：服务端
   `history.rs::a_rebuild_moves_the_durable_revision`，客户端
   `ui/packages/client-core/src/timeline/timeline-merge.ts` 的比较规则，接口层
   `http.rs::a_cursor_from_another_numbering_is_a_reset_not_a_slice`。
8. **没有新增原始 session 双向同步、多 server、跨 worker 恢复或第二套 UI 消息模型。**
   仍然成立：库里只有会话行、实体视图、relay 帧三类，没有 session 文件镜像，也没有第二套
   消息形状（replay 与 loom 自己的行共用一个投影）。

## 8. 阶段 A/B/C 逐条现状（2026-09-21）

阶段 A、B、C 里与"离线查看"有关的部分，已作为 `docs/sqlite-persistence-plan.md`
阶段一至三落地（那份计划是用户授权的实施范围）；下面是逐条现状，包括**没有**照做的一条。

| 阶段 | 条目 | 现状 |
| --- | --- | --- |
| A1 | 缺可信 host binding 的 session 发送/重试明确报错，不自动建新 session | **部分**：`Thread::may_resume_session` 要求 agent + cwd + host 三者一致才续接，缺绑定或跨机器一律不续接（不会拿另一台机器的 session）。但缺绑定时是**开新会话**，不报错，新 identity 还会替换旧映射——`crates/domain/src/thread.rs` 明确写了这是有意选择（"a fresh session is recoverable while the wrong resume is not"）。是否改成显式报错需要产品决定，不该由实现单方面改 |
| A2 | 统一游标身份，请求携带身份，server 切片前验证 | **已闭环**：`historyRevision` 取代 `cacheInstance`+`generation`，请求带 revision，服务端不再把不匹配的请求当位置，客户端按 revision 不同即重置 |
| A3 | 加载用独立 token；worker 在同一 session 上协调 prompt 与 load | **已闭环（服务端）**：run 检查与加载声明同在 dispatch 取用的 lifecycle 锁下，堵掉了"检查后新 run 到达"；加载结果落库前比对水位线，过期基线丢弃。worker 侧 `UpdatePhase::{Loading, Loaded, Ready}` 把 load 期间的回放与真实 run 分开 |
| B1 | 成功同步落盘，失败保持上次成功副本 | **已闭环**：`replace_replayed` 一个事务；失败只写 `last_error` |
| B2 | server 重启直接读副本，worker 离线仍可查看、翻页 | **已闭环**：第 1 条端到端用例 |
| B3 | "刷新历史"动作 + 最后同步状态，临时失败能恢复 | **已闭环**：30 秒退避 + `POST /api/v1/threads/{id}/history/refresh` + 线程菜单项 |
| B4 | 验证"缓存淘汰 / 重启 / worker 断线"三种情况下仍可读 | **基本闭环，缺一个用例**：重启与淘汰有第 1、3 条；"worker 断线不发 ACP 请求也能读"在代码上成立（读取路径只查库，加载才是 ACP），但现有的端到端用例是在 worker 已 enroll 之后读的，没有"worker 完全不在"的独立断言。`history.rs` 里 `Host { code: "offline" }` 覆盖的是加载失败后旧副本仍可读，不是这一条 |
| C1 | 迁移 output、outline、prompt history、搜索、重试读取 | **已闭环**：`thread_domain_events` 已不存在，这些路径统一走 `state.stored_view` |
| C2 | loom 自有事实保留原数据来源，缺 run 信息的行不伪造关联 | **已闭环**：端到端用例断言 replay 行不编造时间、`turnId` 不能 parse 成 run id |
| C3 | 删除 retained relay 冒充完整会话的读取路径与过时 UI 行为 | **已闭环**：`thread_domain_events` 与其读取路径删除 |
| C4 | 设计文档只表达最终决定，历史过程移到变更记录 | **未做**：`docs/sqlite-persistence-plan.md` 本身就是按日期堆叠进展的记录，且 `docs/acp-history-plan.md` 用"当时快照 + 后续状态"的方式保留历史——这算不算要收敛，需要你定 |

**仍然不在这一轮里的**：§3 的目标 2（跨机器续聊）。它要求的"指定一个 agent 的原始 session
迁移可行性验证"还没做，因此不应从这份文档直接滑进实施。

