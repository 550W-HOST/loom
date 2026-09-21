# 服务端持久化收敛到 SQLite：分阶段实施任务

状态：待实施（本文只写任务，不含代码）。本文取代
[`acp-history-plan.md`](acp-history-plan.md) §1「不做持久 transcript」的约定，并落实
[`history-convergence-review.md`](history-convergence-review.md) 的目标 1（离线查看）。
复核文档保留原样，不覆盖。

## 0. 一句话目标

loom 仍是**单 server、多 worker**：worker 上的 ACP 原生 session 负责续聊，server 用嵌入式
SQLite 持久保存**该会话的服务端展示历史**，使 worker 离线、server 重启后任何客户端都能读取
**已提交**的完整历史；同一数据库最终承载实体状态与有界 relay 补帧表，成为唯一持久化实现。

非目标（本任务明确不做）：

- 不恢复 Redis，不做多 server，不做共享日志。
- 不做跨机器续聊：不搬运、不导出、不复制 agent 原生 session 文件，也不把它当迁移凭据。
- 不做 PostgreSQL 或任何外部服务；不引入第二套 UI 消息模型。
- 不自动导入旧 thread 的历史（未上线产品，无历史数据兼容包袱）；失败必须显式报错。
- 不同时改造审批/交互等 loom 自有事实的数据来源。

## 1. 已确认的边界与语义

| 主题 | 决定 |
| --- | --- |
| 会话来源 | ACP 回放是唯一来源；server 保存的是 provider 无关的展示事件，不是 agent 私有文件 |
| 续聊 | 仍由原 worker 的原生 session 完成；跨机器续聊另行设计 |
| 持久化默认 | **默认开启**。`--data-dir` 只覆盖路径，不是开关；不需要环境变量 |
| 写入语义 | 先展示、后落盘。崩溃可丢最后未提交的尾部，但未提交内容不得标为已保存 |
| 一库三职责 | ①长期会话历史 ②实体/设置/自动化/运行状态 ③**有界** relay 补帧事件表 |
| WAL | SQLite 事务不等于 EventId 逻辑事件表；worker 的游标补帧语义必须保留 |
| 迁移顺序 | 历史 → 实体 → relay。前两阶段 `domain.snapshot` / `shard-*.log` 仍必需 |
| 退场条件 | 只有对应职责完全迁移且验收通过，才删除旧文件实现 |
| 数据库位置 | `<server-data-dir>/loom.db`，与 `domain.snapshot`、`shard-*.log` 同目录 |

## 2. 默认数据目录（先做，独立于 SQLite）

现状：`crates/server/src/cli.rs` 的 `data_dir: Option<PathBuf>`，`run.rs` 把它直接当
`backend_path`；`AppState::build` 在 `None` 时选 `MemoryBackend`。也就是说"没传 flag = 无持久化"，
这正是要改掉的蛋疼行为。

要求：

1. `None` 表示**默认目录**，不是内存模式。默认值 `$HOME/.loom/server`（worker 的
   `default_data_dir()` 是 `$HOME/.loom`，server 用其子目录，避免两者写同一个根）。
2. 没有 `HOME`（容器、systemd 精简环境）时**启动失败**，错误信息给出路径和 `--data-dir`
   用法；不得退化为 `/tmp`，也不得静默走内存。容器镜像已显式传
   `--data-dir /var/lib/loom/server`（`containers/loom-server.Dockerfile`），保持。
3. 目录不可创建/不可写，或 SQLite 打不开 → 启动失败，绝不回退内存。
4. 测试与一次性运行通过**显式**方式声明无持久化，而不是靠省略 flag：建议
   `AppConfig` 用 `data_dir: DataDir { Default | Path(PathBuf) | Ephemeral }`，
   测试构造 `Ephemeral`。不新增 `--ephemeral` CLI 开关；需要一次性 server 时用
   `--data-dir "$(mktemp -d)"`。
5. `--local-worker` 现在按 data 路径派生子目录（`<root>/local-worker`），默认化之后必须仍
   派生，且不得与 server 目录混用。

验收：不带任何 flag 启动，写入 `~/.loom/server/`；`HOME` 置空启动失败且信息可操作；
`AppConfig::Ephemeral` 下测试不产生任何 `$HOME` 写入（用 `HOME=$(mktemp -d)` 跑一遍
`cargo test -p loom-server` 验证）。

## 3. 最终数据职责图

```text
worker: 原生 ACP session ──回放──► server: 规范化会话事件 ──► SQLite ──► 所有会话内容查询 ──► UI
                                                  ▲                    （内存只作加速）
实体 mutation / relay event ──同一事务提交──► SQLite ──提交成功后──► 内存视图 + 通知
```

规则：

- 一个 thread 只有一个会话读取来源（SQLite）；内存投影是同一份数据的缓存层，不是第二套权威。
- 已提交历史的普通 GET、翻页、搜索**不启动 ACP**、不要求 worker 在线。
- 同步方向永远 worker → server；server 不把展示副本写回 agent session。
- 未同步的旧会话按需首次同步，并在状态里显式标"尚未同步"。

## 4. 阶段一：会话历史落盘并统一读取

### 4.1 表草案

```sql
-- 每个 thread 一行状态：绑定、代次、最后一次成功同步
CREATE TABLE thread_history (
  thread_id            TEXT PRIMARY KEY,
  provider_session_id  TEXT,
  binding_agent        TEXT,
  binding_cwd          TEXT,
  binding_host_id      TEXT,
  revision             INTEGER NOT NULL DEFAULT 0,  -- 每次基线替换 +1，持久，跨重启单调
  synced_at_ms         INTEGER,
  last_error           TEXT
);

-- 可重建 UI 的规范化事件，按 thread 追加
CREATE TABLE thread_history_row (
  thread_id      TEXT    NOT NULL,
  seq            INTEGER NOT NULL,        -- 写入时分配，永不重算，永不重用
  source_kind    TEXT    NOT NULL,        -- 'run' | 'message' | 'replayed'
  source_run_id  TEXT,                    -- 仅 loom 自有事件
  source_at_ms   INTEGER,
  event_json     TEXT    NOT NULL,        -- 与内存投影吃同一份 ProviderEvent
  PRIMARY KEY (thread_id, seq)
);
CREATE INDEX thread_history_row_run ON thread_history_row(source_run_id);
```

语义与现有内存模型对齐（复用 `RowSource` 与已有投影，不新建消息模型）：

- `seq` 在 INSERT 时取 `MAX(seq)+1`（同一事务内），分配后不变；`revision` **持久**，
  取代内存 `generation`，因此 client 游标跨 server 重启仍然有效。
- 基线同步 = 一个事务内删除该 binding 的 `replayed` 行并插入新回放，`revision += 1`；
  `run` / `message` 行（loom 自有：实时消息、错误与恢复诊断）在替换时**保留去重**，
  不随基线消失。该事务失败则旧基线原样保留，只更新 `last_error`。
- 该表同时是 `thread_history` 之外唯一的会话内容来源；删除 thread 时同事务删除两表行。

### 4.2 写入路径（先展示、异步落盘）

落在现有唯一写入缝上：`AppState::publish_domain_event`（已在此喂内存覆盖层）。

1. 事件到达 → 立即更新内存投影并发布（展示不等待磁盘）。
2. 同一事件写一条 `thread_history_row` 的 INSERT 进入 SQLite 写入队列（单写线程/连接，
   有界队列，不阻塞 Tokio 运行时）。
3. 提交成功后更新该 thread 的内存 `synced_at_ms`/`revision` 视图并（必要时）通知客户端。
4. 提交失败或队列超界 → 标记该 thread `last_error`，timeline 的 `history` 状态如实报告
   （例如 `stale`/`unavailable` + reason），**不静默重试到无限、不谎称已保存**。
5. `history.complete` 只描述**已提交**内容；未提交尾部不得被标为完整。

### 4.3 读取路径统一（本阶段必须一起做完）

`thread_domain_events`（读 retained relay）当前被以下位置使用。会话内容一律改走 SQLite；
loom 自有事实保留原来源：

| 位置 | 性质 | 处理 |
| --- | --- | --- |
| `http.rs::thread_output` | 会话内容 | 改 SQLite |
| `http.rs::thread_conversation_outline` | 会话内容 | 改 SQLite |
| `http.rs::thread_prompt_history` | 会话内容 | 改 SQLite |
| `http.rs::thread_search_matches` | 会话内容 | 改 SQLite（不再受 relay 窗口限制） |
| `http.rs::retry_thread` | 重试所需的最后一次用户输入 | 改 SQLite 的 user 行 |
| `b7.rs` project prompt history 聚合 | 会话内容 | 改 SQLite |
| `http.rs::thread_event_rows`（`threads.events`） | relay 传输/游标补帧 | 保留 relay 语义 |
| `http.rs::thread_has_goal` / `thread_has_active_plan` | loom 领域事实 | 保留 relay 语义 |
| `http.rs::thread_timeline` 的 goal/occupancy | loom 领域事实 | 保留 relay 语义 |

实现要求：`thread_domain_events` 拆成两条读路径（会话条目 / 领域条目），调用方显式选择，
不允许再用一个 helper 冒充两种语义。

**进展（2026-09-21，1.9 完成 · 阶段一收尾）**：① **SIGKILL 持久性**：新增 `crates/loom/tests/history_durability.rs`——真实子进程起 server、HTTP 建 thread + 发消息、`SIGKILL`（不走向任何收尾路径）、同一 data-dir 重启、在没有 worker（因此没有任何 load 路径）的前提下时间线仍能看到那条消息，prompt-history 也能。② **未提交尾部如实标为缺失**：硬杀之后没人能说清丢了什么，所以让**文件自己说它没被正常结束**——`store_meta.clean_stop` 在开库时先读再置 0，`shutdown()` 最后一步置 1；启动时若发现上次未正常结束，就把所有**曾经同步成功**的会话标上 `last_error`（「the server stopped without finishing; this conversation may be behind」），于是读出来是 `stale` 且 `complete=false`。这个警告不会被「干净地重启一次」抹掉，只有一次成功的加载（`replace_replayed` 清空 `last_error`）才能证明对话是完整的。测试 `a_conversation_that_outlived_an_unfinished_stop_is_not_complete`（进程内丢弃 state 模拟被杀 → 重启 → 仍未完成 → 加载成功后才完成）。

**进展（2026-09-21，1.9 之二）**：① 删 thread 现在会同事务清掉库里的两表行，并顺手清掉未落盘 overlay、重试租约与编号分配（`Store::delete_thread` 早就有并有测试，这次是把 HTTP 删除路径接上）；删库失败会如实返回错误并说明「thread 已删但会话没删掉」，不静默。② 写盘失败/被拒的 thread 现在会被**读**如实报告：`stored_view` 把 writer 的 unsaved 标记并进状态与原因（有完整基线也降级为 `stale`、`complete=false`），未落盘的那些行仍然照常显示。测试 `a_row_the_store_refused_is_reported_by_the_read`：停掉 writer 后发布的一行会标记该 thread，读回时不是 complete、原因是「没有存上」，同时该行仍在视图里。**还差**：SIGKILL 持久性端到端（放到下一轮，`crates/loom/tests` 里有现成的真实进程 harness 可复用）。

**进展（2026-09-21，1.7 完成）**：线上的游标身份收敛成一个**持久的** `historyRevision`。服务端 `threads.timeline` 响应去掉 `cacheInstance`+`generation`、改发 `historyRevision`（就是该 thread 持久保存的 revision，`null` 表示还没有任何编号可归属），请求参数同样只带 `historyRevision`，游标校验改成「请求里的 revision 是否等于正在服务的 revision」。契约扩展里替换这两项并重新导出（二次导出字节一致）。客户端：`server-contract` 的请求/响应 schema、`client-core` 的 `LoadedTimelineState`/`identityRelation`（小 = 重建前发出的迟到响应 → 丢弃；大 = 重建 → 重来；相等 → 合并）、`useThreadTimelineController` 的翻页参数与竞态校验全部改名。**为什么可以去掉 instance**：行号不再因重启而重排（库里的号跨重启继续），所以「服务器重启」不再是需要单独识别的信号——唯一会改变编号的是重建，而重建会动 revision。客户端因此少一个比较维度，旧的「跨重启把新编号读成回退」问题从根上没有了。测试：`timeline-merge` 的丢弃/重来/合并三例改到新语义，服务端 `historyRevision` 游标一致/不一致两例，重启端到端（`historyRevision >= 1`）。门禁全绿（Rust + pnpm）。

**进展（2026-09-21，1.6 完成）**：会话内容的读取口全部改到库，`thread_domain_events` 更名为 `thread_domain_entries` 并把语义写进文档（它只服务 loom 自己的领域事实：goal/occupancy、状态变更、run 计数、`threads.events` 的补帧）。新增 `thread_recorded_messages`（从 `stored_view` 里取 `RowSource::Message` 行，还原成「谁说的、说了什么、什么时候、在第几位」）。改用它的：`thread_conversation_outline`、`thread_prompt_history`、`thread_search_matches`（`sourceSeq` 现在是库里的号）、`retry_thread` 的「上一条用户输入」（run 计数仍读日志）、`b7` 的项目 prompt history 聚合。`thread_output` 改为复用 timeline 的同一份投影（库为源），并删掉只服务它的 `assistant_message_timeline`。测试 `a_quiet_thread_still_answers_what_was_said`：把消息挤出 relay 保留窗口后，outline / prompt-history / search / output 四个口都仍能答出内容，同时断言日志里确实已经没有这条会话（证明它们不再依赖窗口）。

### 4.4 普通读取不再依赖在线 worker

- `threads.timeline`、output、outline、prompt history、搜索在**已提交**情况下不发 ACP 请求。
- `read_thread_history` 的"缺缓存则触发加载"只保留给**尚未同步**的 thread（首次打开），
  并且失败要显式标记，不因一次失败永久锁死：提供用户可点的"刷新历史"动作
  （见阶段一验收第 5 条）。
- 内存 `HistoryCache` 保留为投影/加速层，不再是唯一存储；它的 LRU 淘汰**不得**影响
  SQLite 历史。

### 4.5 客户端契约

- `generation` 的语义从"内存缓存代次"改为"该 thread 历史 revision（持久）"；`cacheInstance`
  这类进程身份不再需要。若 SQLite 尚未落地，当前代码先用"实例 + revision"修掉
  "server 重启后客户端永久丢弃新响应"的问题（见 §6 A2），阶段一落地后删除实例字段。
- 分页请求必须携带所持 revision；server 在切片前校验，不匹配则返回最新页（明确 reset），
  不允许用旧序号过滤新数据。
- 历史状态至少能表达：`synced/complete`、`loading`、`stale`（有旧副本正在刷新）、
  `unavailable`（含机器可读 reason）。

### 4.6 阶段一验收门禁

1. 完成一轮对话并同步成功 → 关闭 worker、重启 server → 历史可查看、可翻页。
2. relay 被其他 thread 写满 → 已同步历史与输入历史不消失（搜索同样不受窗口限制）。
3. 清空内存缓存不删磁盘历史，也不需要重启 agent 才能查看。
4. 同步失败/中断 → 旧副本仍可读；用户点"刷新历史"后能恢复。
5. 正在对话时刷新不启动第二个会破坏 session 的操作（沿用现有串行协调；见 §6 A3）。
6. 硬 kill（SIGKILL）后重启：不出现"看起来完整实际缺尾"的历史；未提交尾部如实标为缺失。
7. 磁盘写失败/只读目录/磁盘满：显式报错，不静默降级为内存模式。
8. thread 删除后其历史行同事务消失（无残留、无泄漏）。

### 4.6 验收门禁：现状与证据（2026-09-21）

| # | 门禁 | 证据 |
| --- | --- | --- |
| 1 | 同步成功 → 关 worker、重启 → 历史可看可翻页 | `crates/server/tests/history.rs`（重启 + 加载 + 翻页断言）；`a_rebuild_moves_the_durable_revision` |
| 2 | relay 窗口被写满 → 历史/输入历史/搜索不消失 | `http::tests::a_quiet_thread_still_answers_what_was_said`（挤出窗口后 outline/prompt-history/search/output 四个口仍答），`thread_search_matches` 的 `sourceSeq` 改用库里编号 |
| 3 | 清空内存缓存不删磁盘历史、不需重启 agent | 读路径 `stored_view` 以库为源、overlay 只留未落盘行（`history_cache` 测试 + `a_killed_server_leaves_the_conversation_it_committed`） |
| 4 | 同步失败 → 旧副本仍可读；点刷新能恢复 | `a_failed_load_waits_before_the_next_read_retries_it`、`a_refresh_asks_for_the_conversation_again`、`POST /api/v1/threads/{id}/history/refresh` + 菜单入口 |
| 5 | 对话中刷新不启动会破坏 session 的第二个操作 | `a_load_yields_to_a_run_in_flight`；`refresh_thread_history` 对 `RunInFlight` 直接服务现状 |
| 6 | SIGKILL 后重启：不出现「看起来完整实际缺尾」；未提交尾部如实标缺失 | `crates/loom/tests/history_durability.rs`（已提交的行存活）；`a_conversation_that_outlived_an_unfinished_stop_is_not_complete`（未正常结束 → 标为可能落后，只有成功加载才清除） |
| 7 | 磁盘写失败/只读目录/磁盘满：显式报错，不降级为内存 | 启动失败路径（`an_unusable_path_fails_loudly`、`a_file_that_is_not_a_store_is_refused`）+ `a_row_the_store_refused_is_reported_by_the_read`（写失败被读如实报告） |
| 8 | thread 删除后历史行同事务消失 | `deleting_a_thread_removes_its_rows_and_its_header` + HTTP 删除路径清库/清 overlay/清编号（提交 `5544399`） |

**阶段一完成。** 尚未做（刻意留到后续阶段）：跨机器/多 loom 共用一个会话的协调（用户已明确单独处理）；阶段二、阶段三。

## 5. 阶段二：实体状态进 SQLite，退场 `domain.snapshot`

目标：实体/设置/自动化/运行状态在事务中提交，重启不依赖有界 relay。

必须处理的写入路径问题：现在创建 thread 是**先改内存 registry，再发布事件**，发布前还会先
更新内存 timeline 缓存（`http.rs` 的线程创建、`state.rs::publish_domain_event`）。阶段二要把
顺序改成"**数据库事务提交成功 → 内存视图可见 → 发布/通知**"，否则崩溃后会出现内存与磁盘
互相矛盾的实体状态。

任务要点：

- 表：`project` / `thread` / `host` / `environment` / `queued_message` / `interaction` /
  `thread_section` / `settings` / `automation` / `automation_run` / `run`（字段对应现有
  registry 导出结构）；不易结构化的部分允许 JSON 列，但主键与查询字段必须成列。
- 迁移期双层写：先写 DB + 保留 `domain.snapshot` 写盘，用同一份恢复测试对比两者结果，
  再删文件路径。
- 消除正确性缺口：`latest_active_run_id` 与 `recover_run_flags` 现在读有界 relay
  （`state.rs`），阶段二改为读 DB 中的 run/terminal 记录。
- 退场条件：`domain.snapshot` 不再是任何恢复路径的输入，且"崩溃后恢复"测试覆盖
  （快照删除、只留 DB）后才可删文件写入。保留一个版本周期的只读兼容读取即可。

### 5.1 阶段二实施步骤（2026-09-21 规划）

背景：今天 `domain.snapshot` 一个文件装了四样东西——`RegistrySnapshot`（项目/线程/host/环境/排队消息/交互/侧栏分组）、`RunRecord` 列表、`SettingsSnapshot`、`AutomationState`，外加一个 watermark（恢复只重放它之后的事件）。阶段二要把这些搬进库，并让恢复以库为源。

分步：

- **2.1 实体表 + 读写（本步）**：schema v3 增 `entity` 与 `entity_meta` 两张表；`Store::replace_entities(&DomainSnapshot)` 在**一个事务**里整体替换实体视图，`Store::entities() -> Option<DomainSnapshot>` 读出。测试：往返完全相等（`DomainSnapshot` 自身实现了 `PartialEq`，可与文件路径逐字段对比）、替换是原子的（少写的实体不残留）、空库返回 `None`（区分「没有视图」与「空视图」）。
- **2.2 双层写**：在现有 `snapshot()` 的位置同时写库与文件，用同一份恢复测试对比两条路径的结果必须一致。
- **2.3 恢复改读库**：库里有视图就用库（watermark 之后的日志照旧重放）；文件降级为「库里没有视图时的一次性兼容读取」。
- **2.4 退场文件**：不再写 `domain.snapshot`，删掉写路径与文档里的职责描述；`latest_active_run_id` / `recover_run_flags` 改读库里的 run 记录。
- **2.5 收尾**：确认 `domain.snapshot` 不再是任何恢复路径的输入，删除只读兼容。

**2.1 的一个设计取舍（明确记录）**：计划正文列的是「一个实体一张表」，实际实现用**一张 `entity(kind, id, parent_id, json)` + 一张 `entity_meta(key, value)`**。理由：11 张表的列完全同构（主键 id + 一个父 id + JSON），查询方式也同构（按 kind+id、按 kind+parent），分开只会得到 11 份几乎一样的读写代码；计划要求的「主键与查询字段必须成列」由 `kind`/`id`/`parent_id` 满足。若将来某类实体需要真正的列级索引（例如按状态查 run），再为它单独建表并保留 JSON 作为补充。

**进展（2026-09-21，2.5 之一完成：run 状态变更即落库）**：`RunRegistry` 新增一个 `RunSink`（`stored`/`forgotten`），在**每一次**记录变化后调用：insert、mark_started、mark_provider_error、mark_terminal、set/clear_pending_status_event、remove。这些变化都是每轮几次（不是每帧），所以每次一小行的同步写是划算的；写入发生在**锁外**（先克隆记录再写），不把磁盘延迟带进 run 注册表锁。`AppState` 装的是写库的 sink（`upsert_run`/`forget_run`），写失败会明确打日志而不是静默。测试：`runs::tests::every_run_change_reaches_the_sink`（四个变化各写一次、remove 记一次遗忘）、`state::tests::a_run_state_is_written_when_it_changes`（insert 后库里就有一行；mark_started 后那一行已带 `turn_started`+provider 会话；remove 后行消失）。

**2.5 之二——把 run 恢复完全改读库，依赖阶段三（已实测确认，修正了原先的估计）**：`fail_in_flight_runs` 仍用 `recover_run_flags` 从日志重建「turn 是否开始、provider 是否报错、terminal 是否已发布」。我先按「把判决在 append 之前落库、`terminal_published` 之后落库」改了一版并跑测试，结论是**光调顺序不够、而且解决不了**：「terminal 事件已经进了日志」与「记录里 terminal_published 为真」是**两个不同的提交**，中间任何一点崩溃都会让二者不一致——一个方向会重复发 terminal（日志里两条完成事件），另一个方向会漏掉它。要真正做到「只发一次」，需要 terminal 事件与判决**在同一个事务里提交**；而 terminal 事件现在住在 relay 的 shard 日志里，所以这一步的前置是**阶段三把补帧/日志搬进库**。在那之前，日志扫描是兜住这个跨存储窗口的唯一手段，因此保留；已落库的 run 记录不会白写：它们已经是「崩溃前最后一次变化」的权威副本，阶段三落地后即可直接改读库并删除 `recover_run_flags`/`latest_active_run_id`。相关性质由 `a_committed_terminal_is_not_published_again_during_recovery` 与 `a_terminal_in_the_log_is_recovered_without_a_run_snapshot` 两个测试钉住（试改时它们确实变红，这就是证据）。

**进展（2026-09-21，2.4 完成）**：**不再写 `domain.snapshot`**。写入路径只剩库里的一次事务；文件读取保留为**一次性兼容窗口**（仅在库里从未有过视图时读，且只读不写），`persistence::write_snapshot` 降为 `#[cfg(test)]`（读路径的测试需要一个能造文件的写入器）。命名同步纠正：`snapshot()` → `write_entity_view()`、`spawn_snapshotter` → `spawn_entity_writer`、`snapshot_interval` → `entity_write_interval`、`snapshot_stop/lock` → `entity_write_stop/lock`、`snapshot_root` → `legacy_snapshot_root`（并注明只读、下一版删除）。受影响的测试改成新的真源：三个 state 测试原先靠删/改快照文件制造「没有快照」，现在改为清空库里的视图（`drop_entity_view`）；「快照被位翻转」一测由测试自己写一个**legacy 文件**再腐化它，验证兼容读取遇到坏文件不致命。

**进展（2026-09-21，2.2 + 2.3 完成）**：实体视图现在**同时**写库与文件（`snapshot()` 里先 `replace_entities` 再写文件），并有测试逐字段断言两处一致（`the_stored_entity_view_matches_the_snapshot_file`）。恢复改成**先读库**：库里有视图就用库（watermark 之后的日志照旧重放），只有库从未存过视图时才回退读文件（老库/新库旁边放着旧文件），读库失败也不再拒绝启动而是退回日志重建。测试 `the_entity_view_comes_back_without_the_file_or_the_log`：`backend_max_len` 调到 2 把 thread 创建事件挤出日志、删掉 `domain.snapshot`、重启后 thread 仍在——证明确实来自库。顺带：`automations_conformance` 里两个「旧格式还能读」的测试原本手改 snapshot 文件再重启，现在改成手改**库里的视图**（同样的兼容策略，新的真源）；同时删掉了它们专用的手写快照编码/CRC 辅助函数。

**进展（2026-09-21，2.1 完成）**：schema v3 落地 `entity(kind, id, parent_id, json)` + `entity_meta(key, value)`；`Store::replace_entities(&DomainSnapshot)` 在一个事务里整体替换（先清空再写入，崩溃只会看到旧视图或新视图），`Store::entities()` 读出并区分「库里没有视图」（`None`）与「视图是空的」。父 id（thread→project、environment→project、queued_message/interaction/run→thread）成列并有索引。测试 5 个：空库为 `None`、往返逐字段相等、替换后旧实体不残留、单行 JSON 坏掉会**报错**（不静默丢一个 project）、按 parent 列可查。仍未接线：写入与恢复还是走文件（2.2/2.3）。

### 5.2 表形状（2.1）

```sql
CREATE TABLE IF NOT EXISTS entity (
    kind      TEXT NOT NULL,   -- 'project' | 'thread' | 'host' | 'environment'
                               -- | 'queued_message' | 'interaction' | 'thread_section'
                               -- | 'run' | 'settings' | 'automations'
    id        TEXT NOT NULL,
    parent_id TEXT,            -- thread_id / project_id，能查的就成列
    json      TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
CREATE INDEX IF NOT EXISTS entity_parent ON entity (kind, parent_id);

CREATE TABLE IF NOT EXISTS entity_meta (
    key   TEXT PRIMARY KEY,    -- 'personal_project_id' | 'watermark'
    value TEXT NOT NULL
);
```

### 5.3 阶段二退场条件：现状与证据（2026-09-21）

| 条件 | 证据 |
| --- | --- |
| 实体/设置/自动化/run 在库里，一次写入是原子的 | `store::entities` 的 6 个测试（往返、替换不残留、坏 JSON 报错、parent 成列、run 单行写/忘） |
| 恢复不再依赖有界 relay | `the_entity_view_comes_back_without_the_file_or_the_log`（窗口缩到 2、删文件后 thread 仍在） |
| `domain.snapshot` 不再是任何恢复路径的输入 | 文件读写路径已整体删除（提交 `f1d1047`）；`a_view_that_was_never_written_is_rebuilt_from_the_log` 断言数据目录里没有该文件 |
| 崩溃后恢复 | `a_conversation_that_outlived_an_unfinished_stop_is_not_complete`（未正常结束 → 标记可能落后）、`crates/loom/tests/history_durability.rs`（SIGKILL） |
| run 状态变更即落库 | `runs::tests::every_run_change_reaches_the_sink`、`state::tests::a_run_state_is_written_when_it_changes` |
| run 恢复不再读日志 | `recover_run_flags` / `latest_active_run_id` 已删除；三个窗口测试见 3.3 进展 |

**阶段二完成。** `domain.snapshot` 文件不再存在。

## 6. 与阶段一并行、且与存储选择无关的修正（阶段 A）

这些缺陷无论最终是内存缓存还是 SQLite 都存在，先修。

**进展（2026-09-20）**：§2 默认数据目录已落地（`1858384`）；A2 游标身份已落地
（`1858384`，服务端请求校验 + 客户端实例感知；阶段一会用持久 revision 取代
`cacheInstance`）；A1 整条不做（见上）；identity 晚于 run 回收的活路径已修（`53c1cb3`）。
**A3 的 worker 侧串行互斥尚未实现**，是阶段 A 剩下的最后一项。

- **A1 旧 binding：整条不做**（2026-09-20 决定）。产品只面向干净安装的新用户，不承担旧
  数据适配，因此既没有 `session_rebind_required` 拒绝，也没有"用户指定 host 的重新绑定"
  和"新建会话"入口——代码里不留任何相关痕迹（`830150f` 已 revert）。缺 host 的旧 binding
  按其原语义处理：`may_resume_session` 为假，下一轮自己开 session。干净部署下这种状态
  本身已不可达——唯一活路径（worker 的 identity 上报晚于 run 被回收）已修掉
  （`53c1cb3`：没有 run 可归属时不记录那个 id）。
- **A2 游标身份**：区分"server/cache 实例"与"同一实例内的 revision"。请求携带身份，server 在
  切片前校验；UI 用请求上下文拒绝迟到结果，**不能跨重启仅用整数大小判断新旧**（当前
  `f4b7639` 的客户端规则会让重启后的新响应被永久丢弃）。阶段一落地后用持久 revision 取代。
- **A3 加载与运行的 session 互斥：先测，结论是不用建这套机制。**
  2026-09-20 用真 pi 跑了 `crates/worker/tests/session_race_probe.rs`（`#[ignore]`，两次运行）：

  | 观测 | 结果 |
  | --- | --- |
  | load 与正在流式输出的 turn 并发（load 在 turn 进行 1.5s 时启动） | load 成功（1.3s / 2.1s），turn 正常 Completed，无报错 |
  | turn 在 load 进行中启动（load 后 400ms） | 两边都成功，turn Completed |
  | 会话完整性 | 三次 prompt 全在，最终回放 9 entries / 3 user turns |
  | pi 自己的 session 文件 | 10 行、0 行无法解析（34944 bytes） |

  唯一真实存在的交互是：**并发时 load 返回的是不含进行中 turn 的快照**（3 entries / 1 user
  turn，而 turn 结束后同样一次 load 是 6 entries）。这不是损坏，而是"回放=基线、不是实时
  视图"的语义——server 侧已有 `append_mark` + `install_baseline_if_unchanged` 在覆盖层变动
  时拒绝安装，正是为这件事准备的。

  因此**不做** worker 侧 load/prompt 串行、取消协议与 token fencing：没有需要防的损坏。
  仍然值得做的只有一件便宜的 server 侧改动——把"查 run 在飞"与"占住加载 claim"合进同一个
  临界区，避免注定作废的加载（纯优化，最坏情况只是白加载一次）。

  该结论只覆盖 pi。原生 ACP agent（stdio）的会话存储是它自己的实现，可能加锁或在 load 时
  重写；若将来接入这类 agent，再按同样的探针先测，需要时再加 worker 侧保护。

## 7. 阶段三：relay 补帧进 SQLite，退场 `shard-*.log`

- 表：`relay_event(event_id TEXT PRIMARY KEY, shard INTEGER, scope_kind TEXT, scope_id TEXT,
  payload BLOB, created_at_ms INTEGER, origin TEXT)`，按 shard 建 `created_at_ms`/`event_id`
  索引；每 shard 行数上限与现有 `backend_max_len` 一致，trim 删最旧。
- 实现 `RelayBackend`（`append` / `read_after` / `trim` / `len` / `flush`），
  `loom-relay/tests/relay.rs` 的后端契约套件必须原样通过（含 `a_flush_covers_the_writes_queued_before_it`
  与 `a_closed_relay_refuses_appends_and_flushes_what_it_accepted`）。
- 不阻塞 Tokio：单写线程或受控连接池；`flush` 必须在关停顺序中位于最后（见
  `state.rs::shutdown` 的现有顺序：停写 → 快照 → close → 停读 → drain+flush）。
- 退场条件：relay 契约套件在 SQLite 后端全绿，且 worker 断线重连补帧测试通过，才删
  `shard-*.log` 写入路径。

**进展（2026-09-21，收尾之二：run 恢复不再读日志）**：`terminal_outcome` 现在被当作 run 的**判决**，在**发布终帧之前**就写库（新方法 `mark_verdict`，经 sink 同步落盘）；`mark_terminal` 只再记「终帧已到日志」（发布之后写）。这条顺序使崩溃窗口变成**可结算**的：记录里有判决、帧可能缺——恢复时若 `terminal_published` 为真就只补线程状态（`recover_published_terminal`），否则直接 `finish_run` 把终帧补上（终帧恰好一条）。于是 `recover_run_flags`（扫日志重建 turn/error/terminal 标记）与 `latest_active_run_id`（扫日志找活跃 run）**双双删除**，`fail_in_flight_runs` 只读库里的记录。测试改成覆盖真实可达的三种情形：`a_committed_terminal_is_not_published_again_during_recovery`（判决+帧都有 → 不重复发）、`a_run_whose_frame_never_reached_the_log_is_finished_on_recovery`（有判决无帧 → 补发终帧且线程回 Idle）、`a_working_thread_with_no_stored_run_is_failed_on_recovery`（库里没有 run → 判失败，不停在 working）。全仓库门禁全绿。

**进展（2026-09-21，阶段二/三收尾之一：`domain.snapshot` 彻底退场）**：既然只考虑干净安装，兼容读取没有存在理由，于是把它删干净：`persistence.rs` 只剩「实体视图的形状」（`DomainSnapshot`/`SNAPSHOT_VERSION`/错误类型），文件路径、magic/CRC/rename-as-commit 的封装、`read_snapshot`/`write_snapshot`/`snapshot_path`/`SNAPSHOT_FILE`、`legacy_snapshot_root` 字段全部消失；`recover()` 只读库（读不出来就退回日志重建），`write_entity_view()` 每次都写库、不再有「没有 data dir 就跳过」的分支（内存库本来就随进程消失）。受影响的测试：原「快照文件被位翻转仍能启动」改成 `a_view_that_was_never_written_is_rebuilt_from_the_log`（清空库里的视图＋断言数据目录里根本没有 `domain.snapshot` 这个文件），另有一个测试断言 HTTP 目录下不再出现任何 `shard-*.log`。文档（architecture/domain-persistence/plan）里的「文件是家」全部改成一个库。全仓库门禁全绿。

**进展（2026-09-21，3.3 完成：服务器改用 store 后端，`shard-*.log` 退场）**：`AppState::build` 现在**先开库、再用 `StoreBackend` 建 relay**（同一个 store 实例同时喂 relay 与实体/会话），常量数据目录 = 一个 `loom.db`。测试 `a_configured_data_directory_switches_to_the_durable_backend` 改为断言「帧在库里有」且「`shard-*.log` **不存在**」。**没有做双层写**：用户已明确「只有干净的新用户、不做任何旧数据适配」，而「两个存储语义一致」这件事由**同一套 14 个契约场景在 store 后端上全绿**证得（比双层写逐帧对比更强）；`DiskBackend`（按 shard 的追加文件）随后被整体删除：契约套件里那个「真实持久化后端」的角色现在由 store 后端自己承担，保留一份没人用的文件实现只是负担。全仓库门禁全绿。

**进展（2026-09-21，3.2 完成：契约套件可复用 + store 后端通过全部契约）**：把 `crates/relay/tests/relay.rs` 的 14 个后端场景提成 `crates/relay/src/backend/conformance.rs`（relay 侧一个 `conformance` feature，每个场景接受「要跑哪些 case」，`Case` 由 `Backend` trait 提供：name/durable/open）。这个 feature **只对本 crate 的测试构建开启**（dev-dependency 指向自己的技巧），所以普通构建不引入 tempfile/serde_json。relay 自己的 `tests/relay.rs` 变成薄包装（14 个场景 + 保留那个只能在文件上做的「半写尾帧」磁盘专用测试）。新增 `crates/server/src/store/relay_backend.rs`：`StoreBackend` 实现 `RelayBackend`（append/read_after/trim/len，写是**同步**的——一次 publish 对应一次已提交的事务，flush 无事可做，这正是文件后端「写后 flush」的等价物，而边界从文件换成了数据库提交）。`crates/server/tests/relay_store_contract.rs` 用**同一套** 14 个场景逐个跑 store 后端：**14/14 通过**（一测一场景，失败即点名）。

**进展（2026-09-21，3.1 完成）**：schema v4 增 `relay_event(event_id TEXT PK, shard, scope_kind, scope_id, payload BLOB, created_at_ms, origin)`，索引 `(shard, event_id)` 与 `(shard, created_at_ms)`。`Store` 提供 `append_relay_event(shard, record, max_len)`（**插入与按上限裁剪同一个事务**，超限删最旧；子查询在未超限时返回空，所以不会误删）、`read_relay_events(shard, after, limit)`（`event_id > cursor ORDER BY event_id`，与内存/磁盘后端**同一套语义**：游标在读取里而不是由调用方过滤，所以「同一毫秒的超大突发」不会卡住读者）、`trim_relay_events`、`relay_event_count`/`relay_event_total`。测试 5 个：往返逐字段相等（含 BLOB 载荷与 origin）、突发大于一页仍逐页推进不重不漏、满 shard 删最旧且顺序不变、按时间裁剪只删更旧的、六种 scope 全部往返。

**3.2/3.3 的做法（下一步）**：把 `crates/relay/tests/relay.rs` 那套后端契约提成可复用套件（relay 侧一个非默认 feature 暴露的 `conformance` 模块），这样 `crates/server` 能用同一个 `RelayBackend` 实现跑**同一套**场景，而不是抄一份；store 后端实现落在 `crates/server/src/store/relay_backend.rs`（relay 不能依赖 store，所以实现只能在 server 侧）。先做同步实现跑通契约（每次 append 一次同步提交），量化每次 publish 的代价，再决定是否加「单写线程 + 读取时合并未落盘尾巴」（与阶段一 overlay 同构）。之后才是双层写、恢复改读库、删 `shard-*.log`。

### 7.1 阶段三退场条件：现状与证据（2026-09-21）

| 条件 | 证据 |
| --- | --- |
| 后端契约套件在库后端全绿 | `crates/server/tests/relay_store_contract.rs`：relay 自带的 14 个场景逐个跑 store 后端，14/14（relay 自己那份只跑内存后端） |
| worker 断线重连补帧通过 | `crates/worker/tests/provider_e2e.rs` 的 `a_dispatch_missed_while_disconnected_is_replayed_on_reconnect` 与 `a_reconnect_recovers_more_dispatches_than_one_replay_page`；客户端侧 `crates/server/tests/ws.rs` 的 `a_reconnecting_client_can_resume_from_a_cursor` |
| 服务器不再写 `shard-*.log` | `state::tests::a_configured_data_directory_switches_to_the_durable_backend` 断言帧在库里且 shard 文件不存在 |
| 每-shard 上限仍在 | `store::relay_log` 的 `a_full_shard_drops_its_oldest_frame` |

**阶段三完成。** 服务器只写一个 `loom.db`；按 shard 的追加文件实现（`DiskBackend`）已从 relay crate 删除，relay 只保留内存后端，持久化后端在 server 侧。

## 8. 通用测试清单（每阶段都要有）

- 完整性：提交后读取与内存投影逐行一致（同一 `event_json` 投影出的行相同）。
- 重复/乱序：同一事件重复写不产生两行；批次乱序不改变已分配 `seq`。
- 分页：revision 匹配时游标切片正确；revision 不匹配返回最新页并明确 reset。
- 重启：进程重启后已提交历史可读，未提交尾部不冒充完整。
- worker 离线：不发 ACP 请求即可读历史；未同步 thread 显示明确状态。
- 硬崩溃：SIGKILL 后 DB 可用（WAL 恢复），历史不出现"半行"。
- 磁盘失败：只读目录/磁盘满 → 显式错误 + 旧副本保留，不回退内存。
- 并发：多客户端同时打开同一 thread 只有一次同步；同步期间实时事件不丢。

## 9. 交付顺序与门禁

1. 默认数据目录（§2）+ 失败语义测试 —— 独立可交付，先做。
2. 阶段 A 的三项修正（§6）—— 与存储选择无关，先做。
3. 阶段一（§4）—— 主路径：历史落盘 + 读取统一 + 客户端 revision。
4. 阶段二（§5）—— 实体进库，`domain.snapshot` 退场。
5. 阶段三（§7）—— relay 进库，`shard-*.log` 退场。

每阶段一个独立 PR；每阶段结束时更新
[`history-convergence-review.md`](history-convergence-review.md) 的状态表（该文件由用户维护时
先询问）与 [`architecture.md`](architecture.md) 的职责描述，不新增互相矛盾的补充节。

## 10. 阶段一实施步骤（逐步交付）

每步一个提交、先写测试、跑完 `cargo test --workspace --locked` + `clippy -D warnings` + `pnpm` 门禁再进下一步。

**进展（2026-09-21，续）**：1.3 已落地（`crates/server/src/store/writer.rs` 单写线程 + 1024 条有界队列；`publish_domain_event` → `cache_live_event` 先更内存 overlay 再 `enqueue`，入队不碰磁盘；入队被拒或写失败 → 该 thread 粘性 `unsaved` 标记并带原因；`shutdown()` 顺序为 停止周期写 → 快照 → 关 relay → 停 pump → **drain store writer** → flush relay，drain 失败会让 shutdown 返回错误）。3 个集成测试：发布的消息最终落库且带 `RowSource`、shutdown 排空 25 条积压后新开库能读回、库打不开则启动失败。

### 4.4 重建时「保留去重」的落地方式（2026-09-21 决定，1.4b 前）

`replace_replayed` 只删 `replayed` 行，**loom 自有的 `run`/`message` 行一行不删**：

- 理由一（不丢数据）：基线替换发生在「会话内容被 ACP 重放覆盖」时，但 loom 行里有两类 ACP 会话
  **不会**包含的东西——用户发了但 agent 从未收到的 prompt、以及 provider error/恢复诊断。按 `seq < mark`
  一刀切删除会把这些一起删掉，且是静默的。
- 理由二（去重有现成的位置）：重复的**可见**后果只在投影层。assistant/tool/reasoning 帧按 provider
  item id 折叠（run 的 delta 与 replay 的 completed 同 id 合并成一行，已有测试覆盖）；用户消息是唯一
  按内容配对的：replay 的 user 帧带 agent 的 item id，而 loom 自己记的 prompt 用的是 loom message id，
  两者不可能按 id 合并。因此 `cached_timeline_inputs` 做一次**有序文本配对**：按 seq 顺序维护 loom
  `Message` 用户消息的文本队列，遇到 `Replayed` 用户消息时若队首文本相同，则这一帧不开新行
  （`user_frame_opens_row=false`），否则照常开行。
- 未覆盖的情况：用户在离线时发的 prompt 文本若恰好与 session 里已有的一条相同，则只显示一条，
  但失败诊断行仍在（用户仍能看到「这条没发出去」）。这是可接受的取舍，且不丢存储中的数据。
- 反向风险（重复显示）由集成测试盯住：`real_pi.rs` 里「跑一轮 → 再 load 历史」不得出现重复的
  user/assistant 行；发现 id 不匹配导致的重复，再补规则。

**进展（2026-09-21，1.8 完成）**：界面入口已接上。因为 loom 的客户端不许直接拼 URL——每条能调的路径都必须在导出的契约里有对应条目——所以先按契约扩展的既有做法，在 `tools/contract-export` 的 `applyLoomExtensions` 里**新增一条路由** `threads.historyRefresh`（`POST /api/v1/threads/:id/history/refresh`，内联响应 schema，随导出自动 intern），重新导出 `contracts/bb` 并确认二次导出字节一致；服务端处理器返回 200 + `{status, reason}`。app 侧依次补：`loom-api-routes` 路由表、`loom-api-request-spec` 的请求/响应、`loom-*-runtime` 的调用函数、`sdk` 映射、`ui/packages/sdk` 的方法声明、`useRefreshThreadHistory`（成功后失效该 thread 的 timeline 查询）、线程菜单里的「Read history from the agent」。门禁：`pnpm run typecheck` 干净、前端 50 个文件 489 个测试全过（含契约漂移检查）、Rust 侧全绿。

**进展（2026-09-21，1.8 的自动部分 + 服务端入口）**：失败后的重试不再是「每次读都问一遍」。新增 `HistoryCache` 的失败时间记录（`mark_sync_failed`/`clear_sync_failure`/`may_retry`，内存态，因为「等多久」是进程属性不是会话属性），`read_thread_history` 在 `partial`/`stale`/`unavailable` 三种状态下都要先过 `may_retry(HISTORY_RETRY_BACKOFF = 30s)` 才发起加载——所以 agent 恢复后下一次读（或页面刷新）会自己接上，而 agent 不在时不会把页面变成失败请求流。加载成功清标记，失败/被拒/写库失败都记标记。显式入口：`AppState::refresh_thread_history`（清掉退避、无条件发起加载、返回当前存储视图）+ `POST /api/v1/threads/{id}/history/refresh`（202 + `{status, reason}`，无 binding/未知 provider 时 409 + 原因）——这是唯一能发现「会话在本服务看不到的地方又聊了」的手段，定时轮询做不到。测试：`a_failed_load_waits_before_the_next_read_retries_it`、`a_refresh_asks_for_the_conversation_again`、`a_failed_load_waits_before_it_is_tried_again`。**还差 UI 上的入口**（下一步）。

**进展（2026-09-21，1.4b 落地）**：读路径已统一到库，内存缓存退化为「还没落盘的尾巴 + 正在加载的登记」。具体：`HistoryCache` 删掉 baseline/`generation`/`instance`/状态机，只留 `rows`（未落盘行）、binding、`begin_load/finish_load`、`confirm_written`（写盘成功后由写线程回调裁掉）；新增 `AppState::stored_view`：库行 + overlay 中库里没有的行，按号排序，`instance`/`revision` 取自库，**状态是推导出来的**（是否已同步、是否有加载在飞、上次失败原因），因此重启后状态不会丢；`read_thread_history`/`complete_view`/`settle_history_load` 全部改走它，加载成功即 `replace_replayed` 落库 + `adopt_binding` + `clear_unsaved`，失败只写 `last_error`（旧基线原样保留）。`http.rs` 的 timeline 与 turn 细节都从 `stored_view` 投影。

**4.4 的去重已实现并有测试**：`replayed_copies_loom_already_recorded` 按角色、按顺序把 replay 里的消息与 loom 自己记的 `message` 行配对（assistant 的正文取 delta 累加后的整段，而不是单帧文本），配上的**整条消息的所有帧**都不再投影，loom 那条留下（它有真实时间）；replay 独有的是保留的。测试：`a_replayed_copy_of_a_recorded_message_does_not_open_a_second_row`（单元）+ `crates/server/tests/history.rs` 的重启端到端（4 行、顺序、两类行各自的时间语义）。

**同批完成：数据库引擎先换成 turso，实测后又换回 `rusqlite`（`bundled`）**。当天先按 turso 落地（当时记下的三条事实：pre-release、`roaring` 钉 0.11.2、仍需 C 编译器 + libclang），随后逐条实测两者，结论是 **libclang 那条不成立**：turso 的 `bindgen` 只是声明却从不运行的 build-dependency（`turso_sdk_kit/build.rs` 在非 Windows 上直接 return，构建产物里没有 `bindings.rs`），`clang-sys` 打开的是 `runtime` 特性，`LIBCLANG_PATH=/nonexistent cargo check -p loom-server --locked` 通过。而 turso 的代价都是实测到的：一个 2025-07 才发布、`0.8.0-pre.11` 仍是 pre-release 的引擎（crates.io 总下载 94.6 万，对比 `rusqlite` 1.09 亿）；`roaring` 被迫钉在 0.11.2（MSRV 1.88）；约 90 处 `block_on` 与 async 驱动；单进程独占（服务端开着时 `sqlite3` 连只读打开都被 `database is locked` 拒绝，turso 的 COMPAT 也明说不支持 SQLite/turso 混用的多进程场景）；以及 `cargo build -p loom --locked` 因为 `turso_core` 的 `cfg(loom)` 目标依赖把 `loom 0.7.2` 带进 `Cargo.lock` 而变成歧义，CI 与发布脚本里的该命令直接失败。文件格式与 SQL 都是 SQLite 的，双向实测可读写（turso 写出的 `loom.db` 用内置 SQLite 3.53.2 打开：`journal_mode=wal`、`user_version=4`、`integrity_check=ok`、六张表可读可写；SQLite 写过后 turso 重启照常服务），所以换回是机械操作：SQL 一行未改，只删掉 `block_on` 层、列取值小工具和 turso/`futures-executor` 两个依赖。全仓库 `cargo test --workspace --locked`、`clippy -D warnings`、`cargo +1.88 check --workspace --all-targets --locked` 全绿。

**进展（2026-09-21，1.4a）**：编号归属已改到发布缝。`Store` schema 升到 v2，新增 `store_meta` 记 instance id（随文件mint、重启不变，客户端 cursor 的 instance 不再随进程变）；`append_row(thread, seq, …)` / `replace_replayed(thread, binding, first_seq, …)` 改为由调用方给号，库里只把 `next_seq` 单向前推（`MAX(next_seq, seq+1)`），重复号被主键拒绝；新增 `store::SeqAllocator`（启动时从 `thread_next_seq` 播种），`AppState` 持有它，`cache_live_event` 只 reserve 一次号，同一号同时进内存 overlay 和写队列。测试：重复号被拒、编号跨重开继续（`the_numbering_survives_a_reopen`）、instance 跨重开不变、不同 store 不同 instance。

**1.4 必须解决的事**：`seq` 现在由写线程分配，而内存 overlay 不持有 `seq`；读路径要合并「库里的行」与「还没落库的行」，两边的 `seq` 必须同源。因此 1.4 把 `seq` 分配提到发布缝（每条 thread 一个分配器，启动时从库播种），overlay 行与库行共用同一个号码，读路径按未落库水位线拼接。

**进展（2026-09-21）**：1.1 已落地（`220301a`：`rusqlite` bundled + `Store::open` 迁移/拒绝语义，MSRV 1.88 已验证）；1.2 已落地（`0ab49ca`：`store::history` 类型化读写 + `AppState` 开库，文件库/内存库同一条代码路径）。1.2 的两处实现选择：`seq` 来自线程自己的 `next_seq` 计数列（不是 `MAX(seq)`，否则重建删行后号码会复用）；`AppState.store` 在任何服务器上都存在，没有"跳过持久化"的分支。

**1.1 依赖与打开。** 引擎为 **SQLite 本体**，通过 **`rusqlite`**（`0.40`，`default-features = false`，`features = ["bundled", "cache"]`）：进程内、无服务、无运行时库，`bundled` 把 SQLite（当前 3.53.2）从源码编进二进制，`FROM scratch` 镜像仍然只多一个文件。**两条必须一起记住的事实**（2026-09-21 实测）：① `rusqlite` 是 2014 年起的稳定 crate（总下载 1.09 亿，最新 0.40.2），SQLite 本身是这一层里最难测坏的软件；② 它**仍然是 C 构建**——`bundled` 会为目标平台编译 SQLite 的 C 源码，所以 §10 的 musl 交叉发布仍需要目标平台的 C 交叉编译器（与 turso 相同，见下一条的 ⚠️），但**不需要 libclang**。store 的 API 是同步的，与所有调用者一致（HTTP 读、写线程、测试），中间没有 driver、没有 `block_on`、没有 Tokio 运行时要求。新增 `crates/server/src/store/`（`mod.rs` 打开库、`schema.rs` 迁移）。`<server-data-dir>/loom.db`，WAL、`synchronous=NORMAL`、`foreign_keys=ON`、`busy_timeout`，`PRAGMA user_version` 记 schema 版本。打不开/迁移失败 = 启动失败，绝不回退内存。测试：建库幂等、版本表、坏库显式报错。

**⚠️ 这一步会改变发布工具链（已完成）。** `rusqlite` 的 `bundled` 会为目标平台编译 SQLite 的 C 源码，因此发布机需要**宿主 C 编译器**；交叉到 musl 时还需要目标平台的 C 交叉编译器（与选哪个引擎无关；turso 时期同样缺，只是它的 `simsimd` 把缺失吞成 warning、到链接才报 `cannot find -lsimsimd`）。`release.yml` 的 `build` 任务现在按目标下载 musl.cc 的 musl-cross-make 档案（摘要钉在任务的 matrix 里，`sha256sum --check` 后才解包）、把 `bin/` 追加到 `PATH`——档案里的 `<triple>-gcc` 正是 `cc` crate 要找的名字，所以这就是全部配置，链接方式一行未改（aarch64 仍是 `rust-lld`）。2026-09-21 本地用同一份档案构建了两个目标，并跑过仓库自己的 `scripts/verify-release-binaries.sh`：x86_64 的产物被真正启动、两种角色跑通、内嵌 app 与契约写读都过，aarch64 通过 ELF 检查。细节见 `docs/releasing.md` 的 “The C compiler the build scripts need”。

**1.2 表与迁移（v1）。** `thread_history`（binding、`provider_session_id`、`revision`、`synced_at_ms`、`last_error`）与 `thread_history_row`（`thread_id, seq, source_kind, source_run_id, source_at_ms, event_json`，`seq` 写入时分配、永不重算）。测试：往返、并发分配不重号、同 `(thread_id, seq)` 唯一、删除 thread 同事务清行。

**1.3 写入缝（先展示、异步落盘）。** 单写线程 + 有界队列，落点是现有唯一写入缝 `publish_domain_event`。入队不阻塞运行时；提交成功才更新该 thread 的已提交 revision 与 `synced_at_ms`；队列满或写失败 → 标记未保存并带上原因，不无限重试、不谎称已保存。测试：发布后最终落库、队列满不涨内存、写失败保留旧基线并报告、运行时不被阻塞。

**1.4 读取改走库。** 主 timeline 与所有会话内容读库；内存缓存降级为投影加速层，`revision` 取代 `cacheInstance`+`generation`；ACP 加载只在"尚未同步"与显式刷新时触发。测试：同步成功后**worker 离线**重启，历史仍可读可翻页（§9 验收第 1 条）。

**1.5 同步落库（原子替换）。** 一次加载 = 一个事务：删该 binding 的 `replayed` 行、插新回放、`revision += 1`；loom 自有的 `run`/`message` 行保留并按标识去重；失败保留旧基线，只更新 `last_error`。测试：原子替换、失败保旧、诊断行在替换后仍在。

**1.6 读取统一。** `thread_output`、`thread_conversation_outline`、`thread_prompt_history`、`thread_search_matches`、retry 取用户输入、`b7.rs` project prompt history 改走库；`thread_event_rows`（传输）与 goal/active-plan（loom 自有事实）保持 relay。`thread_domain_events` 拆成"会话条目 / 领域条目"两个显式读路径。测试：超出 relay 窗口的历史对每条路由可见；既有 conformance 保持绿。

**1.7 客户端 revision。** 契约把 `generation` 改为持久 `historyRevision`，删掉 `cacheInstance`；请求携带 revision；客户端规则收敛为"revision 不同 → 重置；更小 → 迟到丢弃"。测试：客户端合并测试 + 服务端旧 revision 触发 reset。

**1.8 显式刷新入口。** `POST /api/v1/threads/{id}/history/refresh`：强制一次同步；失败保留旧基线，成功推进 revision；运行中沿用现有串行规则。UI 最小入口（stale 时出现"重试加载"）。测试：失败后旧内容仍可读、点刷新后恢复。

**1.9 收尾。** thread 删除清行、磁盘写失败显式报错、SIGKILL 后重启不出现"看似完整实则缺尾"、更新 `architecture.md` / `domain-persistence.md` / 本文件状态。

---

## 11. 完成状态（2026-09-21）

三个阶段的实施步骤全部完成，每个子步骤都有测试与门禁证据，逐条列在 §4.6、§5.3、§7.1：

- **阶段一**：会话历史落盘、读取统一、客户端持久 revision、显式刷新入口、收尾（删 thread 清行、写盘失败如实上报、SIGKILL、文档）。
- **阶段二**：实体/设置/自动化/run 进库（一实体一行、整视图一个事务、run 变更即写），`domain.snapshot` 文件**读写路径整体删除**。
- **阶段三**：relay 帧进库（`relay_event`，按上限自裁剪）、后端契约套件在库后端 14/14、服务器改用库后端、`shard-*.log` 不再写、run 恢复改读库（判决先于终帧落库，日志扫描彻底删除）。

现在服务器只写一个 `<data-dir>/loom.db`：会话行、实体视图、relay 帧、run 记录全在其中。没配 `--data-dir` 时用默认目录 `$HOME/.loom/server`；内存库只有测试用，服务器上没有这条路径。

**尚未做（刻意留在本计划之外）**：

1. 跨机器/多 loom 共用一个 ACP 会话的协调（用户已明确「跨机器续聊继续单独处理」）。
2. `docs/history-convergence-review.md` 的状态表（该文件由用户维护，未改动）。

**已补上的一项**：`release.yml` 的 musl 发布工具链（目标是给 `rusqlite`/`bundled` 编 SQLite 用的 C 交叉编译器）已在本计划之外单独做完，两个目标的产物都跑过 `scripts/verify-release-binaries.sh`——见 §1.1 的 ⚠️ 与 `docs/releasing.md`。
