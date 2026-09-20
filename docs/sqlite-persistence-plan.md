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

**同批完成：数据库引擎换成 turso**（见 §1.1 的三条事实：pre-release、`roaring` 钉 0.11.2、仍需 C 编译器 + libclang）。全仓库 `cargo test --workspace --locked`、`clippy -D warnings`、`cargo +1.88 check --workspace --all-targets --locked` 全绿。

**进展（2026-09-21，1.4a）**：编号归属已改到发布缝。`Store` schema 升到 v2，新增 `store_meta` 记 instance id（随文件mint、重启不变，客户端 cursor 的 instance 不再随进程变）；`append_row(thread, seq, …)` / `replace_replayed(thread, binding, first_seq, …)` 改为由调用方给号，库里只把 `next_seq` 单向前推（`MAX(next_seq, seq+1)`），重复号被主键拒绝；新增 `store::SeqAllocator`（启动时从 `thread_next_seq` 播种），`AppState` 持有它，`cache_live_event` 只 reserve 一次号，同一号同时进内存 overlay 和写队列。测试：重复号被拒、编号跨重开继续（`the_numbering_survives_a_reopen`）、instance 跨重开不变、不同 store 不同 instance。

**1.4 必须解决的事**：`seq` 现在由写线程分配，而内存 overlay 不持有 `seq`；读路径要合并「库里的行」与「还没落库的行」，两边的 `seq` 必须同源。因此 1.4 把 `seq` 分配提到发布缝（每条 thread 一个分配器，启动时从库播种），overlay 行与库行共用同一个号码，读路径按未落库水位线拼接。

**进展（2026-09-21）**：1.1 已落地（`220301a`：`rusqlite` bundled + `Store::open` 迁移/拒绝语义，MSRV 1.88 已验证）；1.2 已落地（`0ab49ca`：`store::history` 类型化读写 + `AppState` 开库，文件库/内存库同一条代码路径）。1.2 的两处实现选择：`seq` 来自线程自己的 `next_seq` 计数列（不是 `MAX(seq)`，否则重建删行后号码会复用）；`AppState.store` 在任何服务器上都存在，没有"跳过持久化"的分支。

**1.1 依赖与打开。** 引擎为 **turso**（`=0.8.0-pre.11`，`default-features = false`）：进程内、SQLite 兼容、Rust 实现，替换原先的 `rusqlite`（`bundled` C 版）。turso 的驱动是 async，store 用 `futures-executor::block_on` 在调用线程上驱动它，对外 API 保持同步（HTTP 读、写线程、测试都是同步调用；turso 不需要 Tokio 运行时即可推进，已实测四种上下文）。**三个必须一起记住的事实**（2026-09-21 实测）：① turso 是 pre-release，不是 1.0；② `roaring` 在 `Cargo.lock` 中钉在 `0.11.2`（0.11.3+ 要求 rustc 1.90，本仓库 MSRV 1.88），`cargo update` 必须保留该钉；③ turso **不是无 C 构建**——`cc` 会编译 SIMD/AEAD 内核，`bindgen`/libclang 生成扩展 ABI，因此 §10 的 musl 发布要求从「只需 musl C 编译器」变成「musl C 编译器 + 构建机 libclang」。新增 `crates/server/src/store/`（`mod.rs` 打开库、`schema.rs` 迁移）。`<server-data-dir>/loom.db`，WAL、`synchronous=NORMAL`、`foreign_keys=ON`、`busy_timeout`，`PRAGMA user_version` 记 schema 版本。打不开/迁移失败 = 启动失败，绝不回退内存。测试：建库幂等、版本表、坏库显式报错。

**⚠️ 这一步会改变发布工具链。** 这是本仓库第一个 C 依赖：`bundled` 需要用 C 编译器编 SQLite。`x86_64-unknown-linux-musl` 现在靠宿主 `cc` 链接（`.cargo/config.toml` 注释），`aarch64-unknown-linux-musl` 只有 `rust-lld`；两者都没有 musl 交叉 C 编译器。因此 `release.yml` 必须同时加：x86_64 装 `musl-tools`，aarch64 装 aarch64 的 musl C 交叉编译器并设 `CC_aarch64_unknown_linux_musl`（`.cargo/config.toml` 记一笔）。本地只能验证 gnu 目标；musl 两个目标必须由 CI 或装了交叉工具链的机器验证后，才可以说发布路径完好。

**1.2 表与迁移（v1）。** `thread_history`（binding、`provider_session_id`、`revision`、`synced_at_ms`、`last_error`）与 `thread_history_row`（`thread_id, seq, source_kind, source_run_id, source_at_ms, event_json`，`seq` 写入时分配、永不重算）。测试：往返、并发分配不重号、同 `(thread_id, seq)` 唯一、删除 thread 同事务清行。

**1.3 写入缝（先展示、异步落盘）。** 单写线程 + 有界队列，落点是现有唯一写入缝 `publish_domain_event`。入队不阻塞运行时；提交成功才更新该 thread 的已提交 revision 与 `synced_at_ms`；队列满或写失败 → 标记未保存并带上原因，不无限重试、不谎称已保存。测试：发布后最终落库、队列满不涨内存、写失败保留旧基线并报告、运行时不被阻塞。

**1.4 读取改走库。** 主 timeline 与所有会话内容读库；内存缓存降级为投影加速层，`revision` 取代 `cacheInstance`+`generation`；ACP 加载只在"尚未同步"与显式刷新时触发。测试：同步成功后**worker 离线**重启，历史仍可读可翻页（§9 验收第 1 条）。

**1.5 同步落库（原子替换）。** 一次加载 = 一个事务：删该 binding 的 `replayed` 行、插新回放、`revision += 1`；loom 自有的 `run`/`message` 行保留并按标识去重；失败保留旧基线，只更新 `last_error`。测试：原子替换、失败保旧、诊断行在替换后仍在。

**1.6 读取统一。** `thread_output`、`thread_conversation_outline`、`thread_prompt_history`、`thread_search_matches`、retry 取用户输入、`b7.rs` project prompt history 改走库；`thread_event_rows`（传输）与 goal/active-plan（loom 自有事实）保持 relay。`thread_domain_events` 拆成"会话条目 / 领域条目"两个显式读路径。测试：超出 relay 窗口的历史对每条路由可见；既有 conformance 保持绿。

**1.7 客户端 revision。** 契约把 `generation` 改为持久 `historyRevision`，删掉 `cacheInstance`；请求携带 revision；客户端规则收敛为"revision 不同 → 重置；更小 → 迟到丢弃"。测试：客户端合并测试 + 服务端旧 revision 触发 reset。

**1.8 显式刷新入口。** `POST /api/v1/threads/{id}/history/refresh`：强制一次同步；失败保留旧基线，成功推进 revision；运行中沿用现有串行规则。UI 最小入口（stale 时出现"重试加载"）。测试：失败后旧内容仍可读、点刷新后恢复。

**1.9 收尾。** thread 删除清行、磁盘写失败显式报错、SIGKILL 后重启不出现"看似完整实则缺尾"、更新 `architecture.md` / `domain-persistence.md` / 本文件状态。
