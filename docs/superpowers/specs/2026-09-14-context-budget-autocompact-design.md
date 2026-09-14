# 上下文优化：滚动摘要 + 收紧预算 设计

日期：2026-09-14
状态：已确认，待写实现计划

## 背景与问题

每次 run 起步时，`session::begin_run` → `load_history` 把 **reset 之后的所有
transcript** 全量喂给模型，预算上限由

```
history_token_budget = context_window − DEFAULT_RESERVE_TOKENS(16_384)
```

派生（`oc-cli/src/provider_setup.rs`）。对 `claude-opus-4-8`（200K 窗口）意味着
每轮历史预算约 **183K token**——聊到后面，每发一句就把前面几乎整段对话重发一遍，
既多耗 token 又拖慢首 token。

现状的压缩能力：

- **运行中剪枝** `run.rs::apply_compaction`：拿同一个预算做剪枝/丢早，但不产生摘要。
- **加载截断** `session.rs::load_history`：拿同一个预算从后往前截断。
- **摘要压缩** `session.rs::compact_session`：结构化 checkpoint 落库 + 推进 reset_at，
  已经是完整可用的路径，但**只有手动 `/compact` 触发**，`enable_summary` 默认关。

结论：摘要压缩的核心逻辑无需新写，缺的是「收紧预算」和「自动触发」。

## 目标

1. 每轮发给模型的历史 token 默认上限收紧到 **16K**（可配置），替代 `window − 16384`。
2. 每轮 run 正常结束后，历史超出预算水位时**自动**做滚动摘要压缩，落库为结构化
   checkpoint，后续轮次只读 checkpoint + 最近几轮。
3. 不改动已有手动 `/compact` 语义，不引入新的会话间竞态或数据丢失风险。

## 设计

### 1. 新增 `[context]` 配置节

`oc-core/src/config.rs` 新增：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ContextConfig {
    /// 每轮发给模型的历史 token 上限（输入侧）。默认 16384。
    #[garde(range(min = 1024))]
    pub history_token_budget: u32,
    /// 每轮结束后历史超出预算水位时自动滚动摘要压缩。默认 true。
    #[garde(skip)]
    pub auto_compact: bool,
}
```

要点：

- 挂到顶层 `Config`，`#[serde(default)]` 保证老配置文件缺 `[context]` 节仍能加载。
- `Default` 实现给 `history_token_budget = 16_384`、`auto_compact = true`。
- `default_local()` 里补上这一节。
- 该值只表示**请求的**预算；生效值在 provider_setup 夹进窗口（见下）。

### 2. provider_setup 收紧预算

`oc-cli/src/provider_setup.rs` 的 `build`：

```rust
let requested = cfg.context.history_token_budget as i64;
let budget = requested
    .min(context_window - oc_core::compaction::DEFAULT_RESERVE_TOKENS)
    .max(oc_core::compaction::MIN_BUDGET_TOKENS);
```

替代原来的 `CompactCfg::from_window(...).budget`。这一个值同时收紧三条路径：
`load_history`（加载截断）、`apply_compaction`（运行中剪枝）、以及自动摘要的触发阈值。

`SessionConfig` 增加字段：

```rust
pub auto_compact: bool,
```

### 3. 每轮结束后自动滚动压缩

`session.rs` 的 `SessionCmd::Finished` 分支，在 run 正常结束（`RunOutcome::Completed`）
且 `cfg.auto_compact` 为真时，后台 spawn 一次压缩。新增函数：

```rust
async fn maybe_auto_compact(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    session_id: &str,
    in_flight: &Arc<AtomicBool>,
)
```

逻辑：

1. 读 transcript（`load_transcript(session_id, cfg.max_history_entries)`），
   求和 `tokens_est`；`<= 预算 × 0.8` 则跳过（短会话零开销）。
2. 尝试 `compare_exchange` 抢 `in_flight` 守卫；抢不到（已有压缩在途）直接返回。
3. 复用现有 `compact_session` 的内部步骤：`flush_episodic`（记忆沉淀，失败不阻塞）
   → 调模型生成结构化 checkpoint → `compact_with_summary`（事务内插摘要 entry +
   推进 reset_at）。
4. RAII 清除守卫（`AtomicBool` 置 false）。

触发水位固定为 `预算 × 0.8`（16K 预算 → 12.8K 触发），`keep_recent` 沿用
`CompactCfg::default().keep_recent = 6`。这两项**不进 config**，保持配置面最小。

**共享核心的重构边界**：现有 `compact_session` 承担三件事——「太少不压缩」判定、
`flush_episodic` → 调摘要模型 → `compact_with_summary` 的核心序列、以及 `notify_compact`
用户通知。自动压缩只复用核心序列（前两件），不想要用户通知。因此把核心序列抽成
`compact_core(store, provider, model, session_id) -> CompactOutcome`（枚举：Skipped /
TooShort / Summarized / Failed），`compact_session`（手动）与 `maybe_auto_compact`
（自动）各自决定是否 `notify_compact`。核心序列的「太少不压缩」判定一并收进
`compact_core`，两路径共享。

### 4. 触发时机与占用语义

压缩是 `tokio::spawn` 后台跑，**不占车道**：`Finished` 处理后 actor 立即回到
`recv()`，下一句照常立即开始。压缩摘要最多 120s（沿用 `SUMMARY_TIMEOUT`），失败或
空摘要降级为「不压缩」，不影响对话。

- 手动 `/compact`：行为不变，压缩后有「已压缩上下文」通知。
- 自动压缩：静默（只打日志），避免每 ~16K 对话就弹提醒骚扰用户。

### 5. 并发安全（关键不变式）

后台压缩与下一个 run 并发时不丢数据，理由是 SQLite WAL 快照 + reset_at 单调性：

- `compact_with_summary` 的 `up_to_seq` 来自一次读快照，事务内推进 `reset_at = up_to_seq`。
- 任何并发新 append 的消息 seq 一定 > `up_to_seq`，因此**绝不会**被 reset_at 切掉——
  最多是这份摘要没提最新那几条，而它们仍作为「最近」完整保留在下一次加载里。

要防的唯一叠加是「两次压缩同时跑」（auto 连着触发、或 auto 撞上手动 `/compact`）：

- `SessionActor` 持一个 `Arc<AtomicBool> in_flight` 在途守卫，auto 与手动共用同一把。
- 手动 `/compact` 走现有 `SessionCmd::Compact` → `compact_session`，同样先抢守卫；
  抢不到时回一个「正在压缩中」通知给客户端。

**触发判定与核心序列分离**：`maybe_auto_compact` 先做 token 水位判定（预算 × 0.8），
够格才去抢守卫 + 跑 `compact_core`；`compact_core` 内部再做一次「太少不压缩」的
条数判定（沿用 `keep_recent + 1` 阈值），两判定的职责不重叠——前者是「够不够格
自动触发」，后者是「值不值得压」（与手动 `/compact` 共享）。

## 影响面

| 文件 | 改动 |
|---|---|
| `oc-core/src/config.rs` | 新增 `ContextConfig` + `Default` + `default_local` + 测试 |
| `oc-cli/src/provider_setup.rs` | 预算改 `min(requested, window−reserve)`，透传 `auto_compact` |
| `oc-server/src/session.rs` | `SessionConfig` 加 `auto_compact`；抽出 `compact_core`；`Finished` 分支接自动压缩；在途守卫 |
| `config.example.toml` | 加 `[context]` 节示例 |

## 非目标（明确不做）

- 不实现「按需上下文」（新消息引用前文才加载全历史）——那是后续方向 B，本次只做
  滚动摘要 + 收紧预算。
- 不做 provider prompt-cache 显式标注（`cache_control` 断点）——本设计不依赖也不
  改变 provider 缓存行为。
- 不抽 `oc-core::context` 模块（REFACTOR-1 仍是独立 P3 项）。

## 验收标准

- 老配置文件（无 `[context]` 节）加载不报错，默认拿到 16K 预算 + auto_compact=true。
- `load_history` 加载的历史 token 总量不超过 16K（字符/4 估算口径）。
- 一轮超长对话 run 正常结束后，transcript 被自动摘要压缩，`reset_at` 推进，后续
  轮次历史包含 `【上下文摘要】` entry + 最近 keep_recent 条。
- 短会话（≤ 预算×0.8）不触发任何摘要调用（零额外模型调用）。
- 手动 `/compact` 与自动压缩并发时，只有一次压缩执行，另一次得到「正在压缩」。
- 全部现有单测通过，新增测试覆盖配置缺省、预算夹紧、触发判定。
