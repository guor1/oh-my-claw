# 刷新接续去重（方案 H：落库水位标记）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 刷新页面后 resume 只回放「客户端历史里还没有的」事件，消除工具卡重复、工具输出翻倍、assistant 正文重复。

**Architecture:** `RunLog` 除事件缓冲外再记一串**落库标记点** `(entry_seq, 该刻已 push 的事件总数)`；每次 `persist()` 成功后由 `RunSink::mark_persisted(seq)` 打点。客户端把 `loadHistory` 拿到的最大 seq 作为 `since_seq` 随 `chat.resume` 上报，服务端二分到「seq ≤ since_seq 的最后一个标记点」，从那个事件位置开始回放。重叠恒为 0、空洞恒为 0，且与 `loadHistory`/`resume` 两次往返的时序无关。

**Tech Stack:** Rust 2021（rust-version 1.90）workspace：`oc-store` / `oc-server` / `oc-proto` / `oc-http`；前端 Vue 3 + vite，单测用 `node:test`。

## Global Constraints

- 不得改动 `RunSink::Conn`（TUI/CLI 交互式路径）的行为与开销——本特性只影响 `Detached`。
- `crates/oc-http/ui/dist/` 是 git 追踪的构建产物，由 `assets.rs` 直接服务；**任何改动 `ui/src/` 的任务都必须重建并提交 dist**。
- 协议新增字段一律带 `#[serde(default)]`，保证旧客户端/旧 daemon 混用不炸（`since_seq` 是 `i64`，不加 `skip_serializing_if`）。
- 注释用中文，与周边风格一致；解释「为什么」而非「是什么」。
- 每个 Task 结束前跑 `cargo test --workspace` 必须全绿（当前基线 452 个测试通过）。

---

## 设计要点（实现前必读）

### 核心不变式

run 驱动器是它自己会话在 run 期间的唯一写入者，事件与落库严格交替：

```
seq=143  user entry            （session.rs:531，run 起步前）
         ├─ 事件组 A：Lifecycle::Start、Reasoning、Assistant deltas
seq=144  assistant dispatch    （run.rs:276）   ← 事件组 A 的内容落进这里
         ├─ 事件组 B：Tool start/update/end     （run.rs:301 exec_tool）
seq=145  tool result           （run.rs:307）   ← 事件组 B 的内容落进这里
         ├─ 事件组 C：下一轮 deltas
seq=146  ...
```

所以「客户端已有 entry ≤ H」⟺「落进 seq ≤ H 的那些事件组已在客户端历史里」。

### 为什么用**标记点**而不是给每个事件打 seq 标签

给事件打 `tag = 当时的水位`、回放 `tag >= H`，在 seq 连续时等价；但 `compact_with_summary`（`ops.rs:142`）会往会话尾部**插一条 system 摘要 entry**，若它与 run 交错，run 的下一次 persist 就不是 `tag+1` 而是 `tag+2`，`tag >= H` 会把一整组事件错误过滤掉 → **丢内容**。

标记点方案不依赖 seq 连续：它直接记录「落库那一刻事件缓冲的位置」，seq 只用来做查找键。外部插入 entry 只会让某个 H 落到更早的标记点上 → 多回放一组（重叠，可见但无害），**永不丢**。故障模式从「静默丢内容」降级为「极端情况下多渲染一组」。

### 半落库场景（用户特别问过的）

刷新恰好发生在 `run.rs:276` 落了 dispatch、`run.rs:307` 还没落 tool result 之间（窗口 = 整个工具执行时间，`pip install` 可达数分钟）：

- 客户端历史 H=144，渲染出一张 `status:'running'` 的半卡（`state.js:83`）。
- 服务端找 seq ≤ 144 的最后一个标记点 = (144, 12) → 从第 12 条事件开始回放 = 事件组 B（tool start/update/end）→ **半卡被补完**。✅

反过来若工具已落满库（H=145），标记点 (145, 20) 让事件组 B 整组跳过 → **输出不会翻倍**。✅

### 已知限制（本计划不解决，明确记录）

环形缓冲 `CAP = 1024` 溢出时，`skip` 被 `saturating_sub` 夹到 0，退化为「回放缓冲里剩下的全部」——可能与历史重叠。方案 H 的暴露面只有「上次落库到现在」这一组事件，1024 条足够，故不加 `overflowed` 标志、不调整 CAP。

---

## File Structure

| 文件 | 职责变化 |
|------|----------|
| `crates/oc-store/src/ops.rs` | `append_entry` 返回值从 rowid 改为 **seq**（调用方无人用 rowid） |
| `crates/oc-store/src/writer.rs` | 同步 `writer_call!` 的文档（签名 `-> i64` 不变） |
| `crates/oc-server/src/run_log.rs` | 新增 `pushed` 计数、`marks` 标记表、`mark_persisted()`、`subscribe_from()` |
| `crates/oc-server/src/sink.rs` | 新增 `RunSink::mark_persisted(seq)`，仅 `Detached` 分支转发 |
| `crates/oc-server/src/run.rs` | `persist()` 成功后调 `ctx.sink.mark_persisted(seq)` |
| `crates/oc-proto/src/method.rs` | `ChatResumeParams` 加 `since_seq: i64` |
| `crates/oc-server/src/dispatch.rs` | `handle_chat_resume` 透传 `since_seq` |
| `crates/oc-server/src/session.rs` | `SessionCmd::Resume` / `SessionHandle::resume` 加 `since_seq` |
| `crates/oc-http/src/native/chat.rs` | `ResumeQuery` 加 `since_seq`，透传给 `ChatResumeParams` |
| `crates/oc-http/ui/src/lib/state.js` | 新增 `historyMaxSeq()` + 每会话水位记录；`maybeResume` 带上 `sinceSeq` |
| `crates/oc-http/ui/src/lib/api.js` | `resumeChat` 接受并发送 `sinceSeq` |
| `crates/oc-http/ui/src/lib/state.test.js` | **新建**：`historyMaxSeq` 纯函数单测 |

---

### Task 1: `append_entry` 返回 seq

**Files:**
- Modify: `crates/oc-store/src/ops.rs:28-55`
- Modify: `crates/oc-store/src/writer.rs:244`
- Test: `crates/oc-store/src/lib.rs`（已有 `#[cfg(test)] mod tests`，追加一个）

**Interfaces:**
- Consumes: 无
- Produces: `ops::append_entry(&Connection, &NewEntry) -> StoreResult<i64>` 返回值语义变为 **该 entry 在会话内的 seq**；异步侧 `Writer::append_entry(NewEntry) -> StoreResult<i64>` 同。

- [ ] **Step 1: 确认没有调用方依赖 rowid**

Run: `git grep -n "append_entry" -- 'crates/**/*.rs'`

逐条确认返回值要么被 `?`/`if let Err` 丢弃、要么在测试里 `.await.unwrap()` 后不使用。若发现任何 `let id = ...append_entry(...)` 且后续使用了 `id`，**停下来向调用方报告**，不要继续。

- [ ] **Step 2: 写失败的测试**

加到 `crates/oc-store/src/lib.rs` 的 `mod tests` 里（建库样板照 `entry_roundtrip_and_reset`，`lib.rs:291`）：

```rust
/// `append_entry` 返回的是会话内 seq（不是 rowid）——刷新接续的水位标记
/// 靠它定位「事件落进了哪条 entry」，返回 rowid 会在多会话下完全错位。
#[tokio::test]
async fn append_entry_returns_session_seq() {
    use crate::types::{NewEntry, Role};
    let store = Store::open_memory().expect("open");
    let w = store.writer();
    w.ensure_session("a".into(), "main".into()).await.unwrap();
    w.ensure_session("b".into(), "main".into()).await.unwrap();

    let a1 = w.append_entry(NewEntry::text("a", Role::User, "一", 1)).await.unwrap();
    let a2 = w.append_entry(NewEntry::text("a", Role::Assistant, "二", 1)).await.unwrap();
    // 会话 b 的第一条也必须是 seq=1；返回 rowid 的话这里会是 3。
    let b1 = w.append_entry(NewEntry::text("b", Role::User, "三", 1)).await.unwrap();

    assert_eq!((a1, a2, b1), (1, 2, 1), "返回值应为会话内 seq，而非全局 rowid");
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p oc-store append_entry_returns_session_seq`
Expected: FAIL，`(1, 2, 3)` ≠ `(1, 2, 1)`（返回的是 rowid）

- [ ] **Step 4: 改实现**

`crates/oc-store/src/ops.rs`，把函数末尾的 `Ok(conn.last_insert_rowid())` 改为 `Ok(next_seq)`，并改文档注释：

```rust
/// 追加一条 entry，seq 在会话内单调递增。**返回该 entry 的 seq**（不是 rowid）。
///
/// 返回 seq 而非 rowid：唯一的消费者是刷新接续的水位标记（`RunSink::mark_persisted`），
/// 它要答的是「事件落进了哪条 entry」——rowid 是全局的，跨会话完全错位。
pub fn append_entry(conn: &Connection, e: &NewEntry) -> StoreResult<i64> {
```

同步更新 `crates/oc-store/src/writer.rs:244` 上方（若无文档则新增一行）：

```rust
    /// 追加 entry，返回其**会话内 seq**。
    writer_call!(append_entry(entry: NewEntry) -> i64 => AppendEntry { entry });
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oc-store`
Expected: 全绿

- [ ] **Step 6: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿（基线 452 通过）

- [ ] **Step 7: 提交**

```bash
git add crates/oc-store/src/ops.rs crates/oc-store/src/writer.rs crates/oc-store/src/lib.rs
git commit -m "refactor(store): append_entry 返回会话内 seq（接续水位标记的定位键）"
```

---

### Task 2: `RunLog` 落库标记点 + `subscribe_from`

**Files:**
- Modify: `crates/oc-server/src/run_log.rs`
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 无（纯内存结构）
- Produces:
  - `RunLog::mark_persisted(&self, seq: i64)`
  - `RunLog::subscribe_from(&self, since_seq: i64, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event>`
  - `RunLog::subscribe(&self, replay_reasoning: bool) -> ...` 保留，等价于 `subscribe_from(0, ..)`

- [ ] **Step 1: 写失败的测试**

在 `run_log.rs` 的 `mod tests` 里追加（`assistant` / `collect_immediate` helper 已存在）：

```rust
    fn deltas(evs: &[Event]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                Event::Assistant { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    /// 回放起点由「seq ≤ since_seq 的最后一个落库标记点」决定：
    /// 客户端已从历史里拿到的那些事件组不再重放（否则工具卡/正文渲染两遍）。
    #[tokio::test]
    async fn replay_starts_after_marked_seq() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.push(assistant("r", "乙"));
        log.mark_persisted(144); // 甲乙 的内容落进了 entry 144
        log.push(assistant("r", "丙"));
        log.mark_persisted(145); // 丙 落进 entry 145
        log.push(assistant("r", "丁"));

        let mut s = log.subscribe_from(144, false);
        assert_eq!(
            deltas(&collect_immediate(&mut s).await),
            vec!["丙", "丁"],
            "客户端已有 entry<=144，只该回放其后的事件"
        );

        let mut s = log.subscribe_from(145, false);
        assert_eq!(deltas(&collect_immediate(&mut s).await), vec!["丁"]);

        let mut s = log.subscribe_from(143, false);
        assert_eq!(
            deltas(&collect_immediate(&mut s).await),
            vec!["甲", "乙", "丙", "丁"],
            "早于任何标记点 → 全量回放（缓冲即全部真相）"
        );
    }

    /// 无标记点（run 刚起步、一条都没落库）时退化为全量回放。
    /// 这正是 `subscribe(r) == subscribe_from(0, r)` 的向后兼容依据。
    #[tokio::test]
    async fn no_marks_replays_everything() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.push(assistant("r", "乙"));

        let mut s = log.subscribe_from(999, false);
        assert_eq!(deltas(&collect_immediate(&mut s).await), vec!["甲", "乙"]);
    }

    /// 标记点之后的回放同样过滤 reasoning（「只续不补」语义不受影响）。
    #[tokio::test]
    async fn subscribe_from_still_filters_reasoning() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.mark_persisted(10);
        log.push(reasoning("r", "想"));
        log.push(assistant("r", "乙"));

        let mut s = log.subscribe_from(10, false);
        let got = collect_immediate(&mut s).await;
        assert_eq!(got.len(), 1, "回放应过滤 Reasoning");
        assert_eq!(deltas(&got), vec!["乙"]);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-server --lib run_log`
Expected: 编译失败 —— `no method named 'mark_persisted'` / `'subscribe_from'`

- [ ] **Step 3: 改实现**

把 `run_log.rs` 的 `Inner` / `push` / `subscribe` 替换为：

```rust
struct Inner {
    buf: VecDeque<Event>,
    /// 累计 push 过的事件总数，**单调不回退**（ring 淘汰旧事件也不减）。
    /// 有了它，标记点才能用一个稳定的全局序号定位，而不是会被淘汰打乱的下标。
    pushed: u64,
    /// 落库标记点 `(entry seq, 落库那一刻的 pushed)`。
    /// 语义：全局序号 < pushed 的事件，其内容已落进 seq ≤ entry seq 的 entry。
    ///
    /// 不给每个事件打 seq 标签、而是记标记点，是为了**不依赖 seq 连续**：
    /// `compact_with_summary` 会往会话尾插一条摘要 entry，若与 run 交错，
    /// 「事件 tag+1 == 下一条 entry seq」的算术就不成立，按 tag 过滤会整组丢内容。
    /// 标记点只把 seq 当查找键，外部插入最多让回放多覆盖一组（重叠，不丢）。
    marks: Vec<(i64, u64)>,
    subs: Vec<mpsc::UnboundedSender<Event>>,
}
```

`new()` 里补 `pushed: 0, marks: Vec::new(),`。

`push()` 里在 `inner.buf.push_back(ev.clone());` 之后加一行 `inner.pushed += 1;`。

新增 `mark_persisted`：

```rust
    /// 记一个落库标记点：此刻为止 push 过的事件，其内容都已落进 seq ≤ `seq` 的 entry。
    /// 由 `RunSink::mark_persisted` 在每次 `persist()` 成功后调用。
    pub fn mark_persisted(&self, seq: i64) {
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        let pushed = inner.pushed;
        inner.marks.push((seq, pushed));
    }
```

把 `subscribe` 改成薄壳、新增 `subscribe_from`：

```rust
    /// 订阅并从头回放。等价于 `subscribe_from(0, replay_reasoning)`。
    pub fn subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event> {
        self.subscribe_from(0, replay_reasoning)
    }

    /// 订阅：跳过「客户端已从落库历史拿到」的事件后回放，再登记订阅者。
    ///
    /// `since_seq` = 客户端 `loadHistory` 拿到的最大 entry seq。回放起点取
    /// 「seq ≤ since_seq 的最后一个标记点」——早于它的事件内容已在客户端历史里，
    /// 重放会让工具卡建两张、工具输出拼两遍、assistant 正文渲染两遍。
    ///
    /// 全程持锁——回放与登记订阅者之间不丢事件（沿用原有的不变式）。
    pub fn subscribe_from(
        &self,
        since_seq: i64,
        replay_reasoning: bool,
    ) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");

        // marks 按 seq 递增追加，倒着找到的第一个即「≤ since_seq 的最后一个」。
        let from = inner
            .marks
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= since_seq)
            .map(|(_, pushed)| *pushed)
            .unwrap_or(0);
        // ring 首元素的全局序号。标记点若已被淘汰，saturating_sub 夹到 0
        // → 退化为「回放缓冲里剩下的全部」（可能与历史重叠，但不丢）。
        let buf_start = inner.pushed - inner.buf.len() as u64;
        let skip = from.saturating_sub(buf_start) as usize;

        for ev in inner.buf.iter().skip(skip) {
            if !replay_reasoning && matches!(ev, Event::Reasoning { .. }) {
                continue;
            }
            let _ = tx.send(ev.clone());
        }
        inner.subs.push(tx);
        rx
    }
```

同时更新文件头 `//!` 注释，把「回放可能从中间开始，完整文本最终由落库历史兜底」改成描述标记点语义的一段：

```rust
//! 回放起点由**落库标记点**决定：客户端上报 `since_seq`（它 loadHistory 拿到的
//! 最大 entry seq），只回放落进更晚 entry 的那些事件——否则刷新后工具卡会建两张、
//! 工具输出拼两遍、assistant 正文渲染两遍（见 docs/superpowers/plans/
//! 2026-09-17-resume-watermark.md）。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oc-server --lib run_log`
Expected: PASS，含原有 3 个测试 + 新增 3 个

- [ ] **Step 5: 提交**

```bash
git add crates/oc-server/src/run_log.rs
git commit -m "feat(run_log): 落库标记点 + subscribe_from（按客户端历史水位裁剪回放）"
```

---

### Task 3: `RunSink::mark_persisted` 接线到 `persist()`

**Files:**
- Modify: `crates/oc-server/src/sink.rs`
- Modify: `crates/oc-server/src/run.rs:886-905`（`persist` 函数体）
- Test: `crates/oc-server/src/sink.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `RunLog::mark_persisted(i64)`、`RunLog::subscribe_from(i64, bool)`（Task 2）
- Produces: `RunSink::mark_persisted(&self, seq: i64)`（`Conn`/`Broadcast` 为 no-op）

- [ ] **Step 1: 写失败的测试**

加到 `crates/oc-server/src/sink.rs` 的 `mod tests`：

```rust
    /// sink 上打的落库标记必须落到它持有的 RunLog 上，否则 subscribe_from
    /// 找不到标记点、永远全量回放（等于本特性没生效）。
    #[tokio::test]
    async fn detached_mark_persisted_reaches_runlog() {
        let log = std::sync::Arc::new(RunLog::new());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(8);
        let sink = RunSink::Detached { tx, log: log.clone() };

        let ev = |d: &str| Event::Assistant {
            session: SessionId::main(),
            run_id: oc_proto::RunId::new("r"),
            delta: d.into(),
        };
        sink.send(ev("旧")).await;
        sink.mark_persisted(7);
        sink.send(ev("新")).await;

        let mut sub = log.subscribe_from(7, false);
        let first = sub.try_recv().expect("标记点之后应有一条事件");
        assert!(matches!(&first, Event::Assistant { delta, .. } if delta == "新"));
        assert!(sub.try_recv().is_err(), "标记点之前的「旧」不该被回放");
    }

    /// 非 Detached sink 上打标记是 no-op，且不得 panic——TUI/CLI 路径共用
    /// 同一个 persist()，不能因为没有 RunLog 就炸。
    #[tokio::test]
    async fn non_detached_mark_persisted_is_noop() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        RunSink::Conn(tx).mark_persisted(1);
        let (btx, _brx) = tokio::sync::broadcast::channel::<Event>(4);
        RunSink::Broadcast(btx).mark_persisted(1);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-server --lib sink`
Expected: 编译失败 —— `no method named 'mark_persisted'`

- [ ] **Step 3: 在 `sink.rs` 加方法**

加在 `impl RunSink` 里、`closed()` 之后：

```rust
    /// 打一个落库标记：此前发出的事件，其内容已落进 seq 为 `seq` 的 entry。
    ///
    /// 只有 `Detached` 需要——刷新接续靠它把回放裁到客户端历史水位之后。
    /// `Conn`（TUI/CLI）不接续，`Broadcast`（测试）无历史概念，均为 no-op。
    pub fn mark_persisted(&self, seq: i64) {
        if let RunSink::Detached { log, .. } = self {
            log.mark_persisted(seq);
        }
    }
```

- [ ] **Step 4: 在 `run.rs` 的 `persist()` 里调用**

`crates/oc-server/src/run.rs`，把 `persist` 末尾的写库块：

```rust
    if let Err(e) = ctx
        .store
        .writer()
        .append_entry(oc_store::NewEntry {
            session_id: ctx.session_id.to_string(),
            role,
            content: content.to_string(),
            tokens_est: est,
            tool_calls,
            tool_call_id,
        })
        .await
    {
        warn!(run_id = %ctx.run_id, error = %e, "落库消息失败");
    }
```

改成：

```rust
    match ctx
        .store
        .writer()
        .append_entry(oc_store::NewEntry {
            session_id: ctx.session_id.to_string(),
            role,
            content: content.to_string(),
            tokens_est: est,
            tool_calls,
            tool_call_id,
        })
        .await
    {
        // 打落库标记：此刻之前发出的事件，内容已进这条 entry。刷新接续据此
        // 跳过客户端已从历史拿到的那几组事件（否则工具卡/正文渲染两遍）。
        Ok(seq) => ctx.sink.mark_persisted(seq),
        Err(e) => warn!(run_id = %ctx.run_id, error = %e, "落库消息失败"),
    }
```

> 落库失败时**不打标记**是对的：那组事件没进历史，客户端只能靠回放拿到。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oc-server`
Expected: 全绿

- [ ] **Step 6: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 7: 提交**

```bash
git add crates/oc-server/src/sink.rs crates/oc-server/src/run.rs
git commit -m "feat(sink): RunSink::mark_persisted，persist 成功即打落库标记"
```

---

### Task 4: 协议与 session 透传 `since_seq`

**Files:**
- Modify: `crates/oc-proto/src/method.rs`（`ChatResumeParams`）
- Modify: `crates/oc-server/src/dispatch.rs`（`handle_chat_resume`）
- Modify: `crates/oc-server/src/session.rs:115-119, 176-182, 412-421`
- Test: `crates/oc-server/tests/resume_run.rs`（追加一个用例）

**Interfaces:**
- Consumes: `RunLog::subscribe_from(i64, bool)`（Task 2）、`RunSink::mark_persisted`（Task 3）
- Produces:
  - `ChatResumeParams { session: SessionId, run_id: RunId, since_seq: i64 }`
  - `SessionHandle::resume(&self, run_id: RunId, since_seq: i64, out_tx: mpsc::Sender<Frame>) -> bool`
  - `SessionCmd::Resume { run_id, since_seq, out_tx, reply }`

- [ ] **Step 1: 写失败的测试**

追加到 `crates/oc-server/tests/resume_run.rs`。先在文件顶部把 import 补齐（路径抄自 `crates/oc-server/tests/concurrent_submit.rs:15-25`，那个文件在做同一件事）：

```rust
use oc_core::tool::ApprovalMode;
use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;
```

（`Delta` / `FinishReason` / `Arc` / `Duration` 该文件已有。）

然后追加 helper 与用例：

```rust
/// 免审批 exec：`ApprovalMode::Allow` 让工具轮不卡在审批门上。
/// 与 concurrent_submit.rs:28-32 同款。
fn exec_tools() -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(
        ApprovalMode::Allow,
        Duration::from_secs(10),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));
    ToolExecutor::new(Arc::new(reg))
}

/// 带 since_seq 的接续：已落库轮次的事件不重放，只回放「客户端历史还没有的」。
///
/// 真机对应场景：模型先说一句话再调一次工具（这一整轮落进 seq=2 的 assistant
/// dispatch，工具结果落 seq=3），刷新时客户端 loadHistory 已拿到这两条。若 resume
/// 仍把这一轮的事件重放一遍，前端会建出第二张同 call_id 的工具卡、把工具输出拼
/// 两遍、把「我看一下。」渲染两遍——正是本计划要消除的三个症状。
#[tokio::test]
async fn resume_with_since_seq_skips_persisted_rounds() {
    // 第 1 轮：先吐一句正文，再请求工具调用；第 2 轮：慢慢吐 6 段文本。
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Text("我看一下。".into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "c1".into(),
                name: Some("exec".into()),
                args_chunk: r#"{"command":"echo hi"}"#.into(),
            }),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::ToolUse),
        },
    ];
    let provider = Arc::new(SequencedMock::new(vec![
        round1,
        streaming_reply(6, Duration::from_millis(120)),
    ]));

    let daemon = TestDaemon::builder("resume-since", provider)
        .map_cfg(|c| {
            c.with_idle_timeout(Duration::from_secs(120))
                .with_tools(exec_tools())
                // 关掉自动压缩：它会往会话尾插一条 system 摘要 entry，打乱
                // 下面写死的 seq=3。
                .with_auto_compact(false)
        })
        .start()
        .await;

    let mut first = daemon.client_no_handshake().await;
    first.handshake_as(ClientKind::Detached).await;
    first.chat("跑一下", None).await;

    // 等到工具结束 + 至少一段第 2 轮正文，确保 seq=2(dispatch) 与 seq=3(tool
    // result) 都已落库、标记点都已打上。
    let mut run_id = None;
    let mut tool_ended = false;
    let mut round2_deltas = 0usize;
    while !(tool_ended && round2_deltas >= 1) {
        match tokio::time::timeout(Duration::from_secs(10), first.recv())
            .await
            .expect("等工具结束 + 第 2 轮首段")
        {
            Frame::Res(oc_proto::Res {
                result: ResResult::Ok(MethodOk::ChatSend { run_id: rid }),
                ..
            }) => run_id = Some(rid),
            Frame::Event(Event::Tool {
                phase: oc_proto::ToolPhase::End { .. },
                ..
            }) => tool_ended = true,
            Frame::Event(Event::Assistant { .. }) if tool_ended => round2_deltas += 1,
            _ => {}
        }
    }
    let run_id = run_id.expect("应从 Res(ChatSend) 拿到 run_id");
    drop(first);

    // 新连接接续，声明「我历史里已有到 seq=3」。
    // seq 排布：1=user「跑一下」，2=assistant dispatch（含「我看一下。」+ tool_calls），
    // 3=tool result。第 2 轮正文要到 run 收尾才落 seq=4。
    let mut second = daemon.client_no_handshake().await;
    second.handshake_as(ClientKind::Detached).await;
    second.resume(SessionId::main(), run_id.clone(), 3).await;

    let mut tool_events = 0usize;
    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), second.recv())
            .await
            .expect("等接续流到终态")
        {
            Frame::Event(Event::Tool { .. }) => tool_events += 1,
            Frame::Event(Event::Assistant { delta, .. }) => text.push_str(&delta),
            Frame::Event(Event::Lifecycle {
                phase: oc_proto::LifecyclePhase::End,
                ..
            }) => break,
            _ => {}
        }
    }

    assert_eq!(
        tool_events, 0,
        "工具轮已落库（seq<=3），接续不得重放其事件——否则前端建出第二张工具卡、输出拼两遍"
    );
    assert!(
        !text.contains("我看一下。"),
        "第 1 轮正文已落进 seq=2，不得重放，实际收到: {text}"
    );
    assert!(
        text.contains("第5段。"),
        "第 2 轮剩余正文仍须完整续上，实际收到: {text}"
    );
}
```

`TestDaemon` 的客户端还没有 `resume` 方法，在 `crates/oc-server/src/testing.rs` 的 `impl TestClient` 里、`chat()`（`testing.rs:454`）旁边加一个同款薄壳：

```rust
    /// 发一条 chat.resume（接续在途 Detached run）。
    pub async fn resume(
        &mut self,
        session: SessionId,
        run_id: RunId,
        since_seq: i64,
    ) -> ReqId {
        self.request(Method::ChatResume(ChatResumeParams {
            session,
            run_id,
            since_seq,
        }))
        .await
    }
```

`ChatResumeParams` / `RunId` 若不在 `testing.rs` 的 use 列表里，补进去。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-server --test resume_run`
Expected: 编译失败 —— `ChatResumeParams` 无 `since_seq` 字段

- [ ] **Step 3: 改 proto**

`crates/oc-proto/src/method.rs` 的 `ChatResumeParams`：

```rust
pub struct ChatResumeParams {
    pub session: SessionId,
    pub run_id: RunId,
    /// 客户端已从落库历史拿到的最大 entry seq。服务端据此裁剪回放起点：
    /// 落进 seq ≤ 本值的事件不重放（否则前端工具卡建两张、正文渲染两遍）。
    /// `0` = 什么都没有，全量回放。
    #[serde(default)]
    pub since_seq: i64,
}
```

- [ ] **Step 4: 改 session.rs**

`SessionCmd::Resume` 加字段：

```rust
    Resume {
        run_id: RunId,
        /// 客户端历史水位，见 `ChatResumeParams::since_seq`。
        since_seq: i64,
        out_tx: mpsc::Sender<Frame>,
        reply: oneshot::Sender<bool>,
    },
```

`SessionHandle::resume`：

```rust
    pub async fn resume(&self, run_id: RunId, since_seq: i64, out_tx: mpsc::Sender<Frame>) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionCmd::Resume { run_id, since_seq, out_tx, reply })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
```

actor 分支（`session.rs:412`）：

```rust
            SessionCmd::Resume { run_id, since_seq, out_tx, reply } => {
                let hit = if let Some(log) = run_logs.get(run_id.as_str()) {
                    // 回放过滤 reasoning（只续不补）；起点按客户端历史水位裁剪。
                    let sub = log.subscribe_from(since_seq, false);
                    tokio::spawn(forward_run_log(sub, out_tx));
                    true
                } else {
                    false
                };
                let _ = reply.send(hit);
            }
```

- [ ] **Step 5: 改 dispatch.rs**

`handle_chat_resume` 里把 `handle.resume(p.run_id.clone(), out_tx.clone())` 改为 `handle.resume(p.run_id.clone(), p.since_seq, out_tx.clone())`。

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test -p oc-server --test resume_run`
Expected: 两个既有用例 + 新用例全绿

- [ ] **Step 7: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 8: 提交**

```bash
git add crates/oc-proto/src/method.rs crates/oc-server/src/session.rs \
        crates/oc-server/src/dispatch.rs crates/oc-server/src/testing.rs \
        crates/oc-server/tests/resume_run.rs
git commit -m "feat(proto): chat.resume 带 since_seq，按客户端历史水位裁剪回放"
```

---

### Task 5: HTTP 层透传 `since_seq`

**Files:**
- Modify: `crates/oc-http/src/native/chat.rs:37-41`（`ResumeQuery`）、`:229-260`（`resume` handler）
- Test: `crates/oc-http/tests/native.rs`

**Interfaces:**
- Consumes: `ChatResumeParams::since_seq`（Task 4）
- Produces: `GET /api/v1/chat/resume?run_id=..&session=..&since_seq=..`（`since_seq` 可省，缺省 0）

- [ ] **Step 1: 写失败的测试**

追加到 `crates/oc-http/tests/native.rs`。骨架照 `resume_endpoint_streams_remainder`（`native.rs:541`）——同样的 `spawn_gateway` + `bytes_stream` 逐行消费 + `read_sse`——但 provider 换成「先调工具再吐文本」的 `SequencedMock`，因为**纯文本 run 中途一次都不落库**（只在收尾落），标记点为空时 `subscribe_from` 退化为全量回放，`since_seq` 传什么都看不出差别。

```rust
/// `since_seq` 一路透传到 daemon：声明已有到 seq=3 后，已落库的工具轮不再回放。
///
/// 这条守的是 HTTP 层——query 参数漏填或拼错时，服务端只会收到默认值 0、
/// 悄悄退回全量回放，前端刷新后重复渲染的 bug 会无声复发。
#[tokio::test]
async fn resume_endpoint_forwards_since_seq() {
    use oc_server::testing::SessionConfigExt;

    // 第 1 轮：一句正文 + 工具调用；第 2 轮：6 段文本。
    let round1 = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("我看一下。".into()) },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "c1".into(),
                name: Some("exec".into()),
                args_chunk: r#"{"command":"echo hi"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ];
    let provider = Arc::new(SequencedMock::new(vec![
        round1,
        streaming_reply(6, Duration::from_millis(150)),
    ]));

    let daemon = TestDaemon::builder("nat-resume-since", provider)
        .map_cfg(|c| {
            c.with_idle_timeout(Duration::from_secs(120))
                .with_tools(exec_tools())
                .with_auto_compact(false)
        })
        .start()
        .await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    let mut send_stream = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "main", "text": "跑一下" }))
        .send()
        .await
        .expect("send 应建立")
        .bytes_stream();

    // 消费到「工具已结束 + 第 2 轮已出至少一段」，确保 seq=2/3 都已落库打标。
    let mut linebuf = String::new();
    let mut run_id = None;
    let mut tool_ended = false;
    let mut round2_deltas = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), send_stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                linebuf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = linebuf.find('\n') {
                    let line: String = linebuf.drain(..=pos).collect();
                    let Some(p) = line.trim_end().strip_prefix("data: ") else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(p.trim()) else { continue };
                    if let Some(id) = v.get("run_id").and_then(|i| i.as_str()) {
                        run_id = Some(id.to_string());
                    }
                    match v.get("event").and_then(|e| e.as_str()) {
                        Some("tool") => {
                            if v.pointer("/phase/phase").and_then(|s| s.as_str()) == Some("end") {
                                tool_ended = true;
                            }
                        }
                        Some("assistant") if tool_ended => round2_deltas += 1,
                        _ => {}
                    }
                }
                if run_id.is_some() && tool_ended && round2_deltas >= 1 {
                    break;
                }
            }
            _ => break,
        }
    }
    let run_id = run_id.unwrap_or_else(|| panic!("未能取到 run_id：{linebuf}"));
    assert!(tool_ended, "断连前工具轮应已结束并落库：{linebuf}");

    drop(send_stream);

    // seq 排布：1=user，2=assistant dispatch（含「我看一下。」+ tool_calls），3=tool result。
    let resp = client
        .get(format!(
            "{base}/api/v1/chat/resume?run_id={run_id}&session=main&since_seq=3"
        ))
        .send()
        .await
        .expect("resume 应建立");
    assert_eq!(resp.status(), 200);
    let sse = read_sse(resp, Duration::from_secs(15)).await;

    assert!(sse.contains(r#""phase":"end""#), "resume 流应以 lifecycle end 终止：{sse}");
    assert!(
        !sse.contains("event: tool"),
        "since_seq=3 之前的工具轮不该回放（参数没透传时会回放）：{sse}"
    );
    assert!(
        !sse.contains("我看一下。"),
        "seq=2 已含第 1 轮正文，不该回放：{sse}"
    );
    assert!(sse.contains("第5段。"), "第 2 轮剩余正文仍须续上：{sse}");
}
```

> `exec_tools()` 与 Task 4 里的同名 helper 相同（免审批 exec）。`native.rs` 里没有它，**在本文件内再定义一份**——跨 crate 的 `tests/` 目标之间不能共享 helper，复制是这里的正解。所需 import（`native.rs` 已有的不用重复加）：
>
> ```rust
> use oc_core::tool::ApprovalMode;
> use oc_llm::mock::{ScriptStep, SequencedMock};
> use oc_llm::types::ToolCallDelta;
> use oc_llm::{Delta, FinishReason};
> use oc_server::tools_bridge::ToolExecutor;
> use oc_tools::exec::ExecTool;
> use oc_tools::shell::Shell;
> use oc_tools::ToolRegistry;
> ```
>
> `oc-http` 的 `[dev-dependencies]` 已含 `oc-tools` 与 `oc-core`，无需改 `Cargo.toml`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-http --test native resume_endpoint_forwards_since_seq`
Expected: FAIL（`since_seq` 被忽略，回放未裁剪）

- [ ] **Step 3: 改 `ResumeQuery`**

```rust
pub struct ResumeQuery {
    pub run_id: String,
    #[serde(default)]
    pub session: Option<String>,
    /// 客户端已渲染的历史水位（最大 entry seq）。缺省 0 = 全量回放。
    #[serde(default)]
    pub since_seq: i64,
}
```

- [ ] **Step 4: 改 handler**

`resume` 里构造参数处：

```rust
        Method::ChatResume(ChatResumeParams {
            session: session.clone(),
            run_id: run_id.clone(),
            since_seq: q.since_seq,
        }),
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oc-http`
Expected: 全绿

- [ ] **Step 6: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 7: 提交**

```bash
git add crates/oc-http/src/native/chat.rs crates/oc-http/tests/native.rs
git commit -m "feat(http): /chat/resume 透传 since_seq"
```

---

### Task 6: 前端上报历史水位

**Files:**
- Modify: `crates/oc-http/ui/src/lib/state.js:52-104`（`loadHistory`）、`:316-350`（`maybeResume`）
- Modify: `crates/oc-http/ui/src/lib/api.js:261-267`（`resumeChat`）
- Create: `crates/oc-http/ui/src/lib/state.test.js`
- Modify: `crates/oc-http/ui/dist/`（重建产物）

**Interfaces:**
- Consumes: `GET /api/v1/chat/resume?...&since_seq=N`（Task 5）
- Produces: `historyMaxSeq(entries) -> number`；`resumeChat({ session, runId, sinceSeq, ...callbacks })`

- [ ] **Step 1: 写失败的测试**

新建 `crates/oc-http/ui/src/lib/state.test.js`（样式照 `activity.test.js`）：

```js
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { historyMaxSeq } from './state.js'

test('historyMaxSeq 取最大 seq', () => {
  assert.equal(historyMaxSeq([{ seq: 1 }, { seq: 3 }, { seq: 2 }]), 3)
})

test('historyMaxSeq 空历史为 0（=全量回放）', () => {
  assert.equal(historyMaxSeq([]), 0)
})

test('historyMaxSeq 忽略缺 seq 的条目', () => {
  // 防御性：少一条 seq 不该让水位变成 NaN——那会让 since_seq 序列化成 null，
  // 服务端 serde(default) 兜成 0，悄悄退回全量回放（重复渲染复发）。
  assert.equal(historyMaxSeq([{ seq: 5 }, {}]), 5)
})
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd crates/oc-http/ui && node --test src/lib/`
Expected: FAIL —— `historyMaxSeq is not a function`（未导出）

- [ ] **Step 3: 改 `state.js`**

在 `loadHistory` 上方加：

```js
// 每会话的历史水位：`loadHistory` 拿到的最大 entry seq。
// resume 时上报给服务端，让它跳过「已落库、客户端已渲染」的那几组事件——
// 不报的话工具卡会建两张、工具输出拼两遍、assistant 正文渲染两遍。
const historySeq = {}

/** 历史条目里的最大 seq；空历史 / 缺字段返回 0（= 全量回放）。 */
export function historyMaxSeq(entries) {
  let max = 0
  for (const e of entries ?? []) {
    if (typeof e?.seq === 'number' && e.seq > max) max = e.seq
  }
  return max
}
```

在 `loadHistory` 里、`messageMap[sessionId] = msgs` 之前加一行：

```js
  historySeq[sessionId] = historyMaxSeq(entries)
```

> 放在 `catch` 的 `handleAuthFailure` 早返回**之后**：历史没加载成功就不该有水位，让 `maybeResume` 去等。

`maybeResume` 顶部加守卫、调用处带上 `sinceSeq`：

```js
export function maybeResume(sessionId) {
  if (sessionId !== 'main') return
  const rid = status.active_run
  if (!rid) return
  if (activeChats.has(sessionId)) return   // 已有流在跑，不重复挂

  // 历史还没加载完就 resume，等于声明「我什么都没有」→ 服务端全量回放，
  // 随后 loadHistory 再渲染一遍同样内容 → 正是本次要修的重复。宁可不接。
  const sinceSeq = historySeq[sessionId]
  if (sinceSeq === undefined) return

  const target = sessionId
  const act = activityFor(target)
  act.arm(Date.now())

  const ctrl = resumeChat({
    session: target,
    runId: rid,
    sinceSeq,
    // …以下回调保持原样不动…
```

- [ ] **Step 4: 改 `api.js`**

```js
export function resumeChat({ session, runId, sinceSeq = 0, onDelta, onReasoning, onTool, onEnd, onError }) {
```

以及 URL：

```js
        `/api/v1/chat/resume?run_id=${encodeURIComponent(runId)}&session=${encodeURIComponent(session)}&since_seq=${encodeURIComponent(sinceSeq)}`,
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd crates/oc-http/ui && node --test src/lib/`
Expected: PASS（新增 3 个 + `activity.test.js` 既有的全绿）

- [ ] **Step 6: 重建 dist**

```bash
cd crates/oc-http/ui && npm run build
```
Expected: 构建成功，`dist/` 有改动

- [ ] **Step 7: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 8: 提交**

```bash
git add crates/oc-http/ui/src/lib/state.js crates/oc-http/ui/src/lib/api.js \
        crates/oc-http/ui/src/lib/state.test.js crates/oc-http/ui/dist
git commit -m "feat(ui): resume 上报历史水位 since_seq，消除刷新后重复渲染"
```

---

## 验收（全部任务完成后）

- [ ] `cargo test --workspace` 全绿
- [ ] `cd crates/oc-http/ui && node --test src/lib/` 全绿
- [ ] `git status` 干净（dist 已提交）
- [ ] 真机复验：`main` 会话发一条会触发多轮工具的消息，输出过程中连续刷新 3 次，观察：
  - 工具卡数量 == 数据库里该轮 `tool_calls` 条数（不多不少）
  - 工具输出不重复、不残留永久转圈的卡
  - assistant 正文不重复
  - 刷新恰好卡在工具执行中时，那张 `running` 卡最终被补完
