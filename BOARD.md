# 任务看板

新需求、缺陷、规划都记在这里。**后续开发只看这一份**——任务条目在表格，方案与设计要点在「条目明细」。`docs/` 现行文档只描述已落地状态，`docs/archive/` 已冻结不再更新。

状态：`TODO` 未开始 · `DOING` 进行中 · `BLOCKED` 卡住（写明卡在什么） · `REVIEW` 待验收 · `DONE` 完成后移到 CHANGELOG 并从本文件删除

优先级：`P0` 阻塞发布 · `P1` 该做 · `P2` 想做 · `P3` 有空再说

---

## DOING

（空）

---

## BLOCKED

| ID | 标题 | 优先级 | 卡在什么 |
|---|---|---|---|
| OPS-1 | 真机 7 天连续运行验收 | P0 | 需要一台机器挂着跑满 7 天，只能等时间 |
| OPS-2 | dreaming 闭环真机验证 | P0 | 需要 use_count 累积 + ≥3 天沉淀，只能等时间 |
| FEAT-2 | 向量语义检索接入 | P3 | 需先定 embedding 来源（provider API 还是本地小模型），决策未做 |

---

## TODO

### 功能

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| FEAT-1 | Provider failover 接线 | P1 | `oc-core/src/model.rs` 的 `failover()` / `resolve()` 已实现且有单测，但全仓无调用方。接线点在 `oc-llm` 重试路径 |
| FEAT-3 | 记忆来源追溯链 | P2 | ~~记忆带出处（source），flush/promote 传递~~ 已落地，见条目明细 |
| FEAT-4 | Dreaming 巩固四动作（CREATE/CORROBORATE/REFINE/CORRECT） | P1 | ~~巩固模型轮输出结构化动作 + 落审计~~ 已落地，见条目明细 |
| FEAT-5 | BM25 词级相关性打分 | P1 | 召回排序升级 tf-idf，2-gram 上算不引入分词器，见条目明细 |
| FEAT-6 | RRF 融合（向量+词法比名次） | P3 | 依赖 FEAT-2 向量落地，纯函数，见条目明细 |

### 重构

| ID | 标题 | 优先级 | 现状 |
|---|---|---|---|
| REFACTOR-1 | 抽出 `oc-core::context` 模块 | P3 | 上下文加载散落在 `oc-server/src/session.rs`，不阻塞功能，纯整洁性 |

---

## 已决定暂缓 / 不做

这些**不是待办**，是「为什么现在不做」的结论——记下来是为了别再重提。要动手做某条，先拆成 `FEAT-*` / `REFACTOR-*` 再进 TODO。来源 `docs/archive/design/01-功能点清单.md`（参照 OpenClaw 的功能盘点）。

| 主题 | 决定 |
|---|---|
| 多渠道（Telegram/Discord 等）§2 | 个人助手定位下单源够用，暂缓 |
| 沙箱（docker/ssh/openshell）§6.3 | 不引入沙箱，靠审批门（`ApprovalMode`）兜底 |
| 插件系统 §15 | `plugin-agnostic` 是设计前提，但当前无外部插件消费方，不做 |
| Hooks §14 | 无 hook 机制，扩展接缝未做 |
| MCP client/server §7 | 未做 |
| 多智能体路由 + 委派 §3 | 单 agent 模式 |
| ClawHub §8/§15 | 外部服务，不在单仓库内可完成；前置 skills 格式（已落地）+ 插件 manifest（未做） |
| 媒体/语音实时、节点配对、Canvas 等 §16~§20 | 未做，P3 愿望清单 |

---

## 条目明细

需要展开说明的条目写在这里，简单条目只留表格行即可。

### OPS-1 真机 7 天连续运行验收

P2 阶段 1（读写分离 / 写线程自愈 / 内存淘汰）代码已落地，但没跑满 7 天。这是 P2 出口标准里唯一还没达成的硬性条件。

挂着跑，观察三件事：

- `oc debug` 的 `idem` 计数和会话行数**不该单调上涨**（判断内存泄漏）
- 日志里**不该有** panic 或写线程降级告警
- 慢查询（大型记忆检索）**不该**阻塞主会话回复

工具：`oc debug --watch` 每秒刷新，日志在 `~/.oc/logs/`。

### OPS-2 dreaming 闭环真机验证

自动化测试只证明了 episodic 候选**落库且形状对**。能否通过门 1 取决于 `use_count` 累积和 ≥3 天沉淀，只有真机挂着跑才看得到完整巩固闭环。

历史上 P0-1 / P1-1 / P1-5 的缺陷全都是真机才暴露的，这条不能靠自动化替代。

### FEAT-1 Provider failover 接线

纯函数已就绪：

- `ModelCatalog::failover(tried) -> Option<&ModelEntry>`
- `ModelCatalog::resolve(alias)`

两者都有单测（`failover_skips_tried`），但 `oc-server` / `oc-llm` / `oc-cli` 里对 `.failover(` 零调用。

接线后的效果：主模型返回 5xx 时自动切到备用 provider，failover 链耗尽才报错。配置形态需要在 `[[models]]` 里加 `failover = ["openai:gpt-4o"]` 之类的字段。

### FEAT-2 向量语义检索（原 BLOCKED 解项）

背景见 BLOCKED 表。ReMe 调研（2026-09-14）结论：embedding 来源不解决，向量检索无法落地。两个可选方向：
- 接 ReMe 的可选 embedding（外部服务，但引入 Python 依赖，违背单二进制）。
- 本地小模型（如 fastembed / candle 系，Rust 原生）。
决策未做，本条仍卡在来源选择。**RRF 融合（FEAT-6）依赖本项落地。**

### FEAT-3 记忆来源追溯链

oc 现有 `origin`（Owner/Agent/Untrusted/System）是**信任分级**，不是**来源追溯**——你不知道一条 curated 是从哪条 episodic 晋升、那条 episodic 是从哪段对话 flush 的。

- 借鉴 ReMe 的 `## Sources`：每条长期记忆带出处。
- 接线：`memory` 表加 `source` 列（可空），`flush_episodic` 写 episodic 时记来源 session，`promote_memory` 晋升 curated 时继承来源。
- 检索热路径不碰此列，零性能影响。
- 验收：`oc memory show <id>` 能显示来源链；audit 表能追溯晋升路径。

**状态（2026-09-14）：已落地。** schema V3 加 `source` 列（`ALTER TABLE ADD COLUMN`，前向迁移 + v2→v3 保数据单测）；`NewMemory`/`MemoryRow`/`MemCandidate` 加 `source` 字段；`flush_episodic` 写 `source=session_id`、`persist_explicit_memory` 也记来源会话；`promote_memory` 只动 tier+importance 故晋升时 source 随行继承（无需额外逻辑）。检索热路径仅映射字段、零排序改动。全量 `cargo test --workspace` 绿。展示走 `oc memory search`（`MemHit.source` 已加，CLI 打印「来源:」）。**注：验收原写 `oc memory show`，实际没有 show 子命令，检索链路即展示入口。**

### FEAT-4 Dreaming 巩固四动作（CREATE/CORROBORATE/REFINE/CORRECT）

现状：dreaming 门 2 通过后 `promote_memory` 就地 `UPDATE tier='curated'`，随后巩固模型轮把新旧内容一起丢给模型**自由重写** MEMORY.md——合并是黑盒，无动作语义、不可审计。

- 借鉴 ReMe auto_dream Integrate 阶段的四种动作：CREATE（新开）/ CORROBORATE（印证追加来源）/ REFINE（补边界步骤）/ CORRECT（纠错）。
- 实现：复用巩固模型轮（`rewrite_memory_md` → `run_model`，无需新增模型调用），把自由 Markdown 输出改为结构化动作 + 正文；每个动作指向「哪条旧 curated」或「无=CREATE」。
- 落 `audit` 表：现写死 `"consolidate"`，扩为 `consolidate:create` / `consolidate:corroborate` / `consolidate:refine` / `consolidate:correct`。
- 性能：只在夜间一轮，热路径零影响。
- 验收：巩固后 audit 能查到每条记忆的动作类型；单测覆盖四种动作的解析。

**状态（2026-09-14）：已落地。** `oc-core/dreaming.rs` 加四动作类型 + 结构化 JSON prompt + 宽容解析（剥代码围栏/容忍夹带文字）；`oc-store` 加 `curated_list()` 读 + `merge_memory()` 写（事务内更新目标行 + 重建 FTS + 删源行）；`oc-server/dreaming.rs` 的 `scan_with` 重构为：双门通过 → 一轮模型调判定每条动作 → 逐条落库（create=promote / 其余=merge，未知 source_id 或非法 target 回落 create）+ 审计 `consolidate:<action>` → 用 merged_text 重写 MEMORY.md。模型无 ctx / 调用失败 / 解析失败回落全部 create（等价旧行为）。全量 `cargo test --workspace` 绿。**注：模型轮从 1 轮变 2 轮（第 1 轮结构化决策 + 第 2 轮 MEMORY.md 重写），夜间后台、热路径零影响。**

### FEAT-5 BM25 词级相关性打分

现状：召回靠 FTS5 布尔命中 + LIKE 复核，排序 `rank = 词法重合度 × 半衰期 × 重要度`。召回阶段无 tf-idf 打分，两条各命中 1 词的记忆分不出强弱相关。

- 借鉴 ReMe BM25：查询时对命中 posting list 做 tf-idf 打分，让「罕见词命中」的强相关信号浮上来。
- **不引入分词器**：直接在 oc 现有 2-gram 上算 BM25（`fts.rs::windows` 已是现成 token 空间，character-bigram BM25 是 CJK 标准做法，零新依赖、零 OOV 维护）。
- 前置（2026-09-14 已实测验证）：FTS5 需存词频（`detail=none` → `detail=col`）。**contentless（`content=''`）配 `detail=col` 建表合法但 bm25() 恒返回 0（contentless 强制 columnsize=0，算不出 tf），不可用。** 正确形态是**外部内容表**：`content='memory'` + `content_rowid='no'` + `detail=col`——原文仍留在 `memory.text`（不存副本、不碰 200~300% 线），词频交给 FTS5 docsize shadow 表，`bm25()` 原生可用。`memory.no` 是显式 rowid 正好当 content_rowid。删除语义由 contentless 的「先删后插」改为 `'delete'` 特殊命令，实现时重写 upsert 路径。
- 性能验收：检索仍 ms 级；索引体积实测从 28% 略涨（存词频，非存原文），不触碰 200~300% 原文副本线。

### FEAT-6 RRF 融合（Reciprocal Rank Fusion）

依赖 FEAT-2（向量）落地后才可做。向量语义分 + 词法分不可直接比大小，改比名次：

```
fused_score = vector_weight/(60+vector_rank) + keyword_weight/(60+keyword_rank)
```

- 纯函数，O(结果数) 名次加权，无新依赖。
- 现不做，等 FEAT-2 解掉 embedding 来源后顺手落地。

---

## 维护约定

- 新条目加到 TODO 对应分类下，ID 用 `类型-序号`（`BUG` / `FEAT` / `TEST` / `OPS` / `REFACTOR`）
- 开始做就移到 DOING，卡住移到 BLOCKED 并写明卡在什么
- 做完从本文件删除，把变更写进 `CHANGELOG.md`
- 需要展开的写「条目明细」，简单的只留表格行
- 「已决定暂缓 / 不做」记的是结论，不是待办；要动手做某条，先拆成 `FEAT-*` / `REFACTOR-*` 再进 TODO
- **后续开发只看本文件**：新需求、方案、设计都写在这里——任务条目进对应表格，方案/设计要点写进「条目明细」。不在 `docs/` 下新建文档
- **`docs/archive/` 已冻结**，不再更新，只在追溯历史决策时翻；`docs/` 现行文档仅描述已落地的当前状态
- **本文件是后续开发的唯一入口**，`CHANGELOG.md` 只记已发布的历史
