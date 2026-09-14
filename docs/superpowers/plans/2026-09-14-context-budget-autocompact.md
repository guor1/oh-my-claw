# 上下文优化：滚动摘要 + 收紧预算 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把每轮发给模型的历史 token 上限从「窗口−16384」收紧到可配置的 16K，并在每轮结束后对超水位的长历史自动做滚动摘要压缩。

**Architecture:** 新增 `[context]` 配置节提供 `history_token_budget` 与 `auto_compact`；`provider_setup` 把请求值夹进窗口后填进 `SessionConfig`；`session.rs` 抽出 `compact_core`（共享的摘要压缩核心序列），手动 `/compact` 与新增的 `maybe_auto_compact`（每轮 `Finished` 后台触发）各自包裹它，用一把 `AtomicBool` 在途守卫防叠加。

**Tech Stack:** Rust / tokio / SQLite(WAL) / garde + serde（配置校验与解析）。

## Global Constraints

- 预算下限 `MIN_BUDGET_TOKENS = 8_000`、预留 `DEFAULT_RESERVE_TOKENS = 16_384`（`oc-core/src/compaction.rs`，均为 pub const，勿改值）。
- `ContextConfig.history_token_budget` 校验下限 `1024`（`#[garde(range(min = 1024))]`）。
- 默认值：`history_token_budget = 16_384`、`auto_compact = true`。
- 触发水位 = `预算 × 0.8`（用整数 `budget * 4 / 5` 算，避免浮点）。
- `keep_recent` 沿用 `CompactCfg::default().keep_recent = 6`，不进 config。
- 测试 harness 默认 `auto_compact = false`（避免干扰既有断言），自动压缩测试显式 `.with_auto_compact(true)`。
- 自动压缩静默（只打日志）；手动 `/compact` 保留现有通知文案（含「无需压缩」「正在压缩中」）。
- 提交信息末尾带 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。

---

### Task 1: 新增 `ContextConfig` 配置节

**Files:**
- Modify: `crates/oc-core/src/config.rs`
- Test: `crates/oc-core/src/config.rs`（`#[cfg(test)] mod tests` 内追加）

**Interfaces:**
- Produces: `oc_core::config::ContextConfig { pub history_token_budget: u32, pub auto_compact: bool }`，实现 `Default`（16_384 / true）+ `Validate`；`Config` 增加 `pub context: ContextConfig`（`#[garde(dive)]` + `#[serde(default)]`）。

- [ ] **Step 1: 写失败测试**

在 `crates/oc-core/src/config.rs` 的 `mod tests` 末尾（`skills_section_defaults_when_absent` 测试之后）追加：

```rust
    /// 现网 config.toml 没有 [context] 节，缺省必须拿到默认预算 + 自动压缩开。
    #[test]
    fn context_section_defaults_when_absent() {
        let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
[memory]
vec = true
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3
[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#).expect("无 [context] 节也应能加载");
        assert_eq!(cfg.context.history_token_budget, 16_384, "缺省预算应为 16K");
        assert!(cfg.context.auto_compact, "缺省应开启自动压缩");
        assert!(cfg.validate_shape().is_ok());
    }

    #[test]
    fn context_explicit_wins() {
        let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
[context]
history_token_budget = 8192
auto_compact = false
[memory]
vec = true
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3
[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#).expect("显式 [context] 应能解析");
        assert_eq!(cfg.context.history_token_budget, 8192);
        assert!(!cfg.context.auto_compact);
    }

    #[test]
    fn context_budget_below_min_rejected() {
        let mut cfg = Config::default_local();
        cfg.context.history_token_budget = 512;
        assert!(cfg.validate_shape().is_err(), "预算低于 1024 应校验失败");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-core config::tests::context_section_defaults_when_absent`
Expected: 编译失败 —— `Config` 没有 `context` 字段。

- [ ] **Step 3: 实现最小代码**

在 `crates/oc-core/src/config.rs`：

(a) 在 `struct Config`（约第 14 行）的 `models` 之后、`memory` 之前插入字段：

```rust
    #[garde(dive)]
    #[serde(default)]
    pub context: ContextConfig,
```

(b) 在 `ModelConfig` 定义之后（约第 95 行 `}` 之后）新增类型 + Default：

```rust
/// 上下文预算与自动压缩配置（`[context]` 节）。
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ContextConfig {
    /// 每轮发给模型的历史 token 上限（输入侧）。默认 16384。
    #[garde(range(min = 1024))]
    pub history_token_budget: u32,
    /// 每轮结束后历史超出预算水位时自动滚动摘要压缩。默认 true。
    #[garde(skip)]
    pub auto_compact: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            history_token_budget: 16_384,
            auto_compact: true,
        }
    }
}
```

(c) 在 `default_local()`（约第 315 行 `models: vec![...]` 之后、`memory:` 之前）插入：

```rust
            context: ContextConfig::default(),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-core`
Expected: 全部通过，含新加的 3 个测试与既有 `default_local_is_valid`、`skills_section_defaults_when_absent`。

- [ ] **Step 5: 提交**

```bash
git add crates/oc-core/src/config.rs
git commit -m "feat(config): 新增 [context] 节（history_token_budget + auto_compact）

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 2: 收紧预算 + SessionConfig 透传 `auto_compact`

**Files:**
- Modify: `crates/oc-cli/src/provider_setup.rs:35-79`
- Modify: `crates/oc-server/src/session.rs:25-65`（`SessionConfig` 加字段）
- Modify: `crates/oc-server/src/testing.rs:38-118`（`test_cfg` 加字段 + builder）
- Test: `crates/oc-cli/src/config_loader.rs`（追加解析断言）

**Interfaces:**
- Consumes: `oc_core::config::ContextConfig`（Task 1）。
- Produces: `SessionConfig.auto_compact: bool`；`SessionConfigExt::with_auto_compact(self, on: bool) -> Self`。

- [ ] **Step 1: 写失败测试**

在 `crates/oc-cli/src/config_loader.rs` 的 `mod tests` 里追加（参考既有 `model_output_limit_keys_parse` 的 TOML 结构）：

```rust
    #[test]
    fn context_section_parses() {
        let toml_text = r#"proto_version = 1

[server]
transport = "pipe"

[[models]]
alias = "default"
provider = "openai"
model = "deepseek-chat"
hosting = "cloud"
base_url = "https://api.deepseek.com"
api_key = { env = "DEEPSEEK_API_KEY" }

[context]
history_token_budget = 16384
auto_compact = true

[memory]
vec = false
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3

[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90

[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
timeout_secs = 120

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#;
        let cfg: Config = toml::from_str(toml_text).expect("含 [context] 的配置应可解析");
        assert_eq!(cfg.context.history_token_budget, 16_384);
        assert!(cfg.context.auto_compact);
        assert!(cfg.validate_shape().is_ok());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-cli config_loader::tests::context_section_parses`
Expected: 编译失败 —— `Config` 尚无 `context` 字段（若 Task 1 未先合入，此步是同一失败点；顺序执行时此处应已存在 `context`，则失败点变为 `SessionConfig` 缺 `auto_compact` 字段，因为 `build` 编译不过）。

- [ ] **Step 3: 实现最小代码**

(a) `crates/oc-server/src/session.rs`：在 `SessionConfig` 的 `history_token_budget` 字段之后插入：

```rust
    /// 每轮结束后历史超预算水位时自动滚动摘要压缩（[context] 节）。
    pub auto_compact: bool,
```

(b) `crates/oc-cli/src/provider_setup.rs`：把预算派生（原第 35-38 行）替换为：

```rust
    // 上下文窗口 → 历史预算：请求值夹进 (MIN, window−reserve) 区间。
    let context_window = model.effective_context_window() as i64;
    let budget = (cfg.context.history_token_budget as i64)
        .min(context_window - oc_core::compaction::DEFAULT_RESERVE_TOKENS)
        .max(oc_core::compaction::MIN_BUDGET_TOKENS);
```

并在 `SessionConfig` 字面量（原第 43-79 行）内、`history_token_budget: budget,` 之后插入：

```rust
        auto_compact: cfg.context.auto_compact,
```

(c) `crates/oc-server/src/testing.rs`：`test_cfg()` 的 `SessionConfig` 字面量（`history_token_budget: 8000,` 之后）插入：

```rust
        auto_compact: false,
```

并在 `SessionConfigExt` trait（第 70-77 行附近）加声明、`impl SessionConfigExt`（第 79 行起）加实现：

```rust
    fn with_auto_compact(self, on: bool) -> Self;
```

```rust
    fn with_auto_compact(mut self, on: bool) -> Self {
        self.auto_compact = on;
        self
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-core -p oc-cli -p oc-server`
Expected: 全部通过（Task 2 无行为变化，仅编译透传）。

- [ ] **Step 5: 提交**

```bash
git add crates/oc-cli/src/provider_setup.rs crates/oc-cli/src/config_loader.rs crates/oc-server/src/session.rs crates/oc-server/src/testing.rs
git commit -m "feat(context): 收紧历史预算并透传 auto_compact 到 SessionConfig

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 3: 抽出 `compact_core`（纯重构，行为不变）

**Files:**
- Modify: `crates/oc-server/src/session.rs:703-792`（`compact_session` → `compact_core` + 薄壳 `compact_session`）

**Interfaces:**
- Produces: `enum CompactOutcome { Summarized { older_count: usize }, TooShort { entries: usize }, Failed(&'static str) }`（derive `Debug`）；`async fn compact_core(store: &oc_store::Store, cfg: &SessionConfig, provider: &Arc<dyn Provider>, session_id: &str) -> CompactOutcome`。
- Consumes: 既有 `flush_episodic`、`crate::summarize::summarize`、`notify_compact`（均已在 session.rs / summarize.rs，签名不变）。

- [ ] **Step 1: 重构（本步无独立失败测试，靠既有 `compact_cmd.rs` 守护）**

把 `crates/oc-server/src/session.rs` 的 `compact_session`（原 703-792 行）替换为下面三段。核心序列原样搬移，`notify_compact` 调用移到薄壳里，返回值换成 `CompactOutcome`。

在 `compact_session` 原位置前插入枚举定义：

```rust
/// 一次摘要压缩的结果分类（手动与自动路径共用）。
#[derive(Debug)]
enum CompactOutcome {
    /// 压缩完成，落库 checkpoint。`older_count` = 被摘要的历史条数。
    Summarized { older_count: usize },
    /// 历史太短，不值得压缩。`entries` = 当前条数。
    TooShort { entries: usize },
    /// 加载/摘要/写入失败，降级不压缩。`&'static str` = 具体环节（拼"压缩失败："前缀）。
    Failed(&'static str),
}
```

核心序列（无 notify）：

```rust
/// 摘要压缩的核心序列：加载 → 沉淀 episodic → 调摘要模型 → 落库 checkpoint。
///
/// 手动 `/compact` 与自动压缩共用。失败绝不 panic，返回 [`CompactOutcome::Failed`]
/// 由调用方决定是否通知用户（手动要通知，自动静默打日志）。
async fn compact_core(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    session_id: &str,
) -> CompactOutcome {
    let keep_recent = oc_core::compaction::CompactCfg::default().keep_recent;

    let entries = match store
        .writer()
        .load_transcript(session_id.into(), cfg.max_history_entries)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "compact：加载历史失败");
            return CompactOutcome::Failed("加载历史出错");
        }
    };
    if entries.len() <= keep_recent + 1 {
        return CompactOutcome::TooShort { entries: entries.len() };
    }

    let cut = entries.len() - keep_recent;
    let older = &entries[..cut];
    let up_to_seq = older.last().map(|e| e.seq).unwrap_or(0);

    let msgs: Vec<oc_llm::Message> = older
        .iter()
        .map(|e| oc_llm::Message {
            role: match e.role {
                oc_store::Role::Assistant => oc_llm::MsgRole::Assistant,
                oc_store::Role::System => oc_llm::MsgRole::System,
                oc_store::Role::Tool => oc_llm::MsgRole::User,
                oc_store::Role::User => oc_llm::MsgRole::User,
            },
            content: e.content.clone(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        })
        .collect();

    // 摘要之前先沉淀 episodic 候选（设计 §11.5，P1-6）。顺序刻意在调模型之前。
    flush_episodic(store, session_id, older).await;

    let older_count = older.len();
    let input_chars: usize = msgs.iter().map(|m| m.content.chars().count()).sum();
    info!(session = %session_id, msgs = older_count, input_chars, "compact：调摘要模型");
    let Some(summary) = crate::summarize::summarize(provider, &cfg.model, &msgs).await else {
        warn!(session = %session_id, msgs = older_count, "compact：摘要为空/失败，跳过");
        return CompactOutcome::Failed("摘要生成为空或超时");
    };

    if let Err(e) = store
        .writer()
        .compact_with_summary(session_id.into(), up_to_seq, summary)
        .await
    {
        warn!(error = %e, "compact：落库失败");
        return CompactOutcome::Failed("写入出错");
    }

    CompactOutcome::Summarized { older_count }
}
```

薄壳（手动路径，保留通知文案）：

```rust
/// 手动压缩指定会话：调用核心序列后，按结果回发用户通知。
async fn compact_session(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    session_id: &str,
) {
    match compact_core(store, cfg, provider, session_id).await {
        CompactOutcome::Summarized { older_count } => notify_compact(
            events,
            session_id,
            &format!("已压缩上下文：{older_count} 条历史消息总结为摘要"),
        ),
        CompactOutcome::TooShort { entries } => notify_compact(
            events,
            session_id,
            &format!("当前上下文较短（{entries} 条），无需压缩"),
        ),
        CompactOutcome::Failed(step) => {
            notify_compact(events, session_id, &format!("压缩失败：{step}"));
        }
    }
}
```

`notify_compact` 函数体（原 888-895 行）保持不变。

- [ ] **Step 2: 跑测试确认通过**

Run: `cargo test -p oc-server --test compact_cmd --test compaction`
Expected: 全部通过。`compact_summarizes_old_history` 仍见「上下文摘要」+「结构化摘要文本」；`compact_short_history_notifies_instead_of_silent` 仍见「无需压缩」。

- [ ] **Step 3: 提交**

```bash
git add crates/oc-server/src/session.rs
git commit -m "refactor(session): 抽出 compact_core，手动/自动压缩共用核心序列

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 4: 每轮结束后自动滚动压缩 + 在途守卫

**Files:**
- Modify: `crates/oc-server/src/session.rs`（`use` 引入 AtomicBool/Ordering；`actor_loop` 建守卫 + `Finished` 分支接线；`compact_session` 加守卫参数；新增 `maybe_auto_compact`）
- Test: Create `crates/oc-server/tests/auto_compact.rs`

**Interfaces:**
- Consumes: `CompactOutcome` + `compact_core`（Task 3）、`SessionConfig.auto_compact`（Task 2）。
- Produces: `async fn maybe_auto_compact(store, cfg, provider, session_id, guard: &Arc<AtomicBool>)`；`compact_session` 签名增参 `guard: &Arc<AtomicBool>`。

- [ ] **Step 1: 写失败测试**

新建 `crates/oc-server/tests/auto_compact.rs`：

```rust
//! 自动滚动摘要压缩：每轮结束后历史超水位时后台压缩落库 checkpoint。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_server::session::{self, SessionConfig};
use oc_server::testing::{test_cfg, SessionConfigExt};
use tokio::sync::broadcast;

fn cfg() -> SessionConfig {
    // 默认 test_cfg 的 history_token_budget = 8000 → 水位 6400。
    test_cfg().with_auto_compact(true).with_history(500, 8000)
}

async fn wait_terminal(rx: &mut broadcast::Receiver<oc_proto::Event>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            oc_proto::Event::Lifecycle { phase: oc_proto::LifecyclePhase::End, .. }
                | oc_proto::Event::Lifecycle { phase: oc_proto::LifecyclePhase::Error { .. }, .. }
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn auto_compact_triggers_on_long_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 40 条 × 500 token_est = 20000，远超水位 6400。
    let big = "字".repeat(400);
    for i in 0..40 {
        let role = if i % 2 == 0 { oc_store::Role::User } else { oc_store::Role::Assistant };
        w.append_entry(oc_store::NewEntry::text("main", role, format!("{big}#{i}"), 500))
            .await
            .unwrap();
    }

    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("这是压缩后的摘要文本"));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    handle.submit("最新一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // 自动压缩在后台执行，轮询等落库生效（摘要 entry 出现）。
    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let hist = w.load_transcript("main".into(), 500).await.unwrap();
        if hist.iter().any(|e| e.content.contains("上下文摘要")) {
            assert!(hist.iter().any(|e| e.content.contains("这是压缩后的摘要文本")), "应含摘要文本");
            assert!(!hist.iter().any(|e| e.content.contains("历史消息 0")), "最早消息应被排除");
            ok = true;
            break;
        }
    }
    assert!(ok, "自动压缩应在超时内完成落库");
}

#[tokio::test]
async fn auto_compact_skips_short_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 3 条短历史，远低于水位。
    for (role, text) in [
        (oc_store::Role::User, "hi"),
        (oc_store::Role::Assistant, "在"),
        (oc_store::Role::User, "就一句"),
    ] {
        w.append_entry(oc_store::NewEntry::text("main", role, text, 1))
            .await
            .unwrap();
    }

    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    handle.submit("就一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;
    // 给足时间让「若有误触发的压缩」跑出来，再断言没有。
    tokio::time::sleep(Duration::from_millis(300)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "短历史不应触发摘要模型调用（captures 应只有主 run 一次）");
    let hist = w.load_transcript("main".into(), 500).await.unwrap();
    assert!(!hist.iter().any(|e| e.content.contains("上下文摘要")), "短历史不应产生摘要 entry");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-server --test auto_compact`
Expected: 编译失败 —— `with_auto_compact` 不存在（若 Task 2 未合入）或 `maybe_auto_compact` 不存在。

- [ ] **Step 3: 实现最小代码**

(a) `crates/oc-server/src/session.rs` 顶部 `use` 块（现有 `use std::sync::Arc;` 之后）加：

```rust
use std::sync::atomic::{AtomicBool, Ordering};
```

(b) `compact_session` 签名改为带守卫，并在函数开头加 claim：

```rust
async fn compact_session(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    events: &broadcast::Sender<Event>,
    session_id: &str,
    guard: &Arc<AtomicBool>,
) {
    // 抢在途守卫：自动压缩可能在后台跑。抢不到说明已有压缩进行中，提示而非叠加。
    if guard.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        notify_compact(events, session_id, "正在压缩中，请稍后再试");
        return;
    }
    let outcome = compact_core(store, cfg, provider, session_id).await;
    guard.store(false, Ordering::Release);
    match outcome {
        // ……（原 CompactOutcome 三分支 notify，保持不变）
    }
}
```

(c) `actor_loop` 函数体（约第 232 行 `let mut last_activity = ...` 之后）新增守卫：

```rust
    // 压缩在途守卫：手动 /compact 与每轮结束后的自动压缩共用，防两个摘要并发叠加。
    let compact_in_flight = Arc::new(AtomicBool::new(false));
```

(d) `SessionCmd::Compact` 分支（原第 329 行 `compact_session(&store, &cfg, &provider, &events, &sid).await;`）改传守卫：

```rust
                compact_session(&store, &cfg, &provider, &events, &sid, &compact_in_flight).await;
```

(e) `SessionCmd::Finished` 分支：在 `if active.as_ref().map(|a| &a.run_id) == Some(&run_id) {` 块内、最顶部先记 `completed`，并在块末（`queue.complete_active()` 的 `if let Some(next)` 之后、`}` 之前）加自动压缩 spawn。

先在块首（`let elapsed = ...` 之前）插入：

```rust
                    let completed = matches!(outcome, RunOutcome::Completed);
```

在块末（该 `if active...` 的收尾 `}` 之前，即 `if let Some(next) = queue.complete_active() { ... }` 块之后）插入：

```rust
                    // 每轮结束后自动滚动压缩（后台，不占车道）。仅正常结束且开启
                    // auto_compact 时触发；水位不足或已有压缩在途时内部自会跳过。
                    if completed && cfg.auto_compact {
                        let store_c = store.clone();
                        let cfg_c = cfg.clone();
                        let provider_c = Arc::clone(&provider);
                        let sid_c = sid.clone();
                        let guard_c = Arc::clone(&compact_in_flight);
                        tokio::spawn(async move {
                            maybe_auto_compact(&store_c, &cfg_c, &provider_c, &sid_c, &guard_c).await;
                        });
                    }
```

(f) 新增 `maybe_auto_compact`（放在 `compact_session` 之后）：

```rust
/// 每轮结束后的自动滚动摘要压缩：水位不足或已有压缩在途则跳过（静默）。
async fn maybe_auto_compact(
    store: &oc_store::Store,
    cfg: &SessionConfig,
    provider: &Arc<dyn Provider>,
    session_id: &str,
    guard: &Arc<AtomicBool>,
) {
    // 水位判定：transcript 总 token 估算 ≤ 预算 × 0.8 则不压。
    let entries = match store
        .writer()
        .load_transcript(session_id.into(), cfg.max_history_entries)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "auto compact：加载历史失败，跳过");
            return;
        }
    };
    let total: i64 = entries.iter().map(|e| e.tokens_est.max(1)).sum();
    if total <= cfg.history_token_budget * 4 / 5 {
        return;
    }

    // 抢在途守卫：与手动 /compact 互斥。抢不到说明已有压缩进行中，直接返回。
    if guard.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    let outcome = compact_core(store, cfg, provider, session_id).await;
    guard.store(false, Ordering::Release);
    // 自动压缩静默：只打日志，不向用户弹通知（每 ~16K 对话弹一次会骚扰）。
    info!(session = %session_id, ?outcome, "auto compact 完成");
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-server`
Expected: 全部通过，含新 `auto_compact` 两个测试，以及既有 `compact_cmd` / `compaction` / 其余集成测试（`test_cfg` 默认 `auto_compact=false`，不干扰）。

- [ ] **Step 5: 提交**

```bash
git add crates/oc-server/src/session.rs crates/oc-server/tests/auto_compact.rs
git commit -m "feat(context): 每轮结束后自动滚动摘要压缩（后台 + 在途守卫）

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 5: 更新示例配置文档

**Files:**
- Modify: `config.example.toml`

**Interfaces:**
- 无代码接口。纯文档，与 Task 1 的配置节对齐。

- [ ] **Step 1: 加 `[context]` 节示例**

在 `config.example.toml` 的 `[[models]]` 块之后（约第 49 行 `api_key = ...` 之后、`[memory]` 之前）插入：

```toml
# ── 上下文 ─────────────────────────────────────────────────────
# 每轮发给模型的历史 token 上限（输入侧）。默认 16384。
# 聊得越久，超出的旧对话会被自动滚动摘要压缩成结构化 checkpoint，
# 后续轮次只读摘要 + 最近几轮，省 token、也更快。
# 比旧行为（budget = 窗口 − 16384，几乎整段重发）收敛得多。
[context]
history_token_budget = 16384   # 每轮历史 token 上限（>= 1024）
auto_compact = true            # 每轮结束后自动滚动摘要压缩
```

- [ ] **Step 2: 提交**

```bash
git add config.example.toml
git commit -m "docs(config): 示例配置加 [context] 节说明

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- `[context]` 节 + serde default + default_local → Task 1。✅
- 预算收紧 `min(requested, window−reserve)` 替代 `window−16384` → Task 2。✅
- 每轮结束后自动压缩（水位 0.8、后台、静默）→ Task 4。✅
- 手动 `/compact` 语义保留 → Task 3 薄壳 + Task 4 守卫。✅
- 并发安全（WAL 快照 + reset_at 单调性不丢数据；AtomicBool 防叠加）→ Task 4 守卫 + 文档注释。✅
- `config.example.toml` 示例 → Task 5。✅
- 验收「老配置加载不报错」→ Task 1 `context_section_defaults_when_absent` + 既有 `skills_section_defaults_when_absent`。✅
- 验收「短会话零摘要调用」→ Task 4 `auto_compact_skips_short_history`（captures.len()==1）。✅

**Placeholder scan:** 无 TBD/TODO；所有代码块为完整内容。

**Type consistency:**
- `ContextConfig.history_token_budget: u32` 在 config、provider_setup（`as i64`）、SessionConfig（`i64`）间一致。✅
- `CompactOutcome` 三个变体在 `compact_core`（返回）、`compact_session`（match）、`maybe_auto_compact`（只 Debug 打印）一致。✅
- `with_auto_compact(self, on: bool)` 声明与实现一致。✅
- `maybe_auto_compact(store, cfg, provider, session_id, guard: &Arc<AtomicBool>)` 调用点参数类型与定义一致。✅
- `compact_session(..., guard: &Arc<AtomicBool>)` 调用点（`SessionCmd::Compact` 分支）已同步改传守卫。✅

**已知边界（诚实说明）:** 在途守卫的并发互斥（auto 与手动同抢一把 `AtomicBool`）是「由构造保证」，未写确定性的并发单测——两路径分属「后台 spawn」与「串行占道」，时序依赖真实调度，确定性测试需引入可控 stalling mock，收益低且易 flaky，故用日志 + 既有 `compact_cmd` 测试守护。守卫本身逻辑简单（`compare_exchange` + RAII 式 `store(false)`），风险可接受。
