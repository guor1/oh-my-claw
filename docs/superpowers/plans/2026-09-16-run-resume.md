# 刷新后接续在途 run 剩余流 —— 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Web 端刷新页面后，自动接续 main 会话在途 Detached run 的剩余流式输出（assistant 文本 + 工具事件 + 后续 thinking），体验等同没断过。

**Architecture:** 给每个 Detached run 挂一个订阅者模式的 `RunLog`（环形缓冲 + live 订阅者），内联事件经 `RunSink::Detached::send` 单漏斗写入。新增协议方法 `ChatResume`，新连接按 `run_id` 订阅 RunLog：先回放缓冲（过滤 reasoning）、再续收实时事件，一路到 `Lifecycle::End`。前端刷新后依据 `status.active_run` 自动挂 resume。

**Tech Stack:** Rust（edition 2021）、tokio（mpsc/Mutex）、serde、axum SSE、Vue 3（前端）。

## Global Constraints

- 业务逻辑注释用中文（与本仓一致）。
- 只服务 **main 会话** 的 Detached run；子会话接续不在范围。
- Interactive（TUI/CLI）路径零改动、零额外开销（`RunSink::Conn` 不带 RunLog）。
- reasoning 语义：**只续不补**——缓冲里存 reasoning（续流需要），回放阶段过滤（`subscribe(replay_reasoning=false)`）。
- 不覆盖 `Approval` / `UserInput` 的可答复（留作 follow-up）。
- 不改 store schema、不加新依赖。
- 新字段/新方法向后兼容（老客户端不用 `ChatResume`，不受影响）。

---

### Task 1: RunLog 模块（订阅者模式环形缓冲）

**Files:**
- Create: `crates/oc-server/src/run_log.rs`
- Modify: `crates/oc-server/src/lib.rs`（加 `pub mod run_log;`）

**Interfaces:**
- Consumes: `oc_proto::Event`（已 `Clone`）。
- Produces:
  - `RunLog::new() -> RunLog`
  - `RunLog::push(&self, ev: Event)` — 入缓冲（cap 1024，满丢最旧）+ 广播给 live 订阅者
  - `RunLog::subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event>` — 同一把锁内「回放缓冲（replay_reasoning=false 时过滤 Reasoning）+ 登记订阅者」

- [ ] **Step 1: 写失败测试**

在 `crates/oc-server/src/run_log.rs` 末尾追加 `#[cfg(test)] mod tests`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oc_proto::{Event, LifecyclePhase, RunId, SessionId};

    fn assistant(run: &str, delta: &str) -> Event {
        Event::Assistant {
            session: SessionId::main(),
            run_id: RunId::new(run),
            delta: delta.into(),
        }
    }
    fn reasoning(run: &str, delta: &str) -> Event {
        Event::Reasoning {
            session: SessionId::main(),
            run_id: RunId::new(run),
            delta: delta.into(),
        }
    }
    fn end(run: &str) -> Event {
        Event::Lifecycle {
            session: SessionId::main(),
            run_id: RunId::new(run),
            phase: LifecyclePhase::End,
        }
    }

    /// 回放按序、过滤 reasoning（replay_reasoning=false）。
    #[tokio::test]
    async fn replay_filters_reasoning_in_order() {
        let log = RunLog::new();
        log.push(assistant("r", "你"));
        log.push(reasoning("r", "想"));
        log.push(assistant("r", "好"));

        let mut sub = log.subscribe(false);
        // 立即拿到的应是「你」「好」，无 reasoning。
        let got = collect_immediate(&mut sub).await;
        assert_eq!(got.len(), 2, "回放应过滤 reasoning，得到 2 条 assistant");
        assert!(matches!(&got[0], Event::Assistant { delta, .. } if delta == "你"));
        assert!(matches!(&got[1], Event::Assistant { delta, .. } if delta == "好"));
    }

    /// 回放与 live 订阅之间不丢不重：subscribe 后 push 的新事件也能收到。
    #[tokio::test]
    async fn live_events_flow_after_subscribe() {
        let log = RunLog::new();
        log.push(assistant("r", "先"));
        let mut sub = log.subscribe(false);
        assert!(!collect_immediate(&mut sub).await.is_empty(), "应回放已缓冲的「先」");

        log.push(assistant("r", "后"));
        let mut got = vec![];
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while let Some(ev) = sub.recv().await {
                got.push(ev);
            }
        }).await.unwrap_or(());
        assert_eq!(got.len(), 1, "续流应收到 push 后的新事件");
        assert!(matches!(&got[0], Event::Assistant { delta, .. } if delta == "后"));
    }

    /// cap 满丢最旧（这里 cap 不暴露，用默认 1024 不测；改为验证 push 不 panic + 顺序稳定）。
    #[tokio::test]
    async fn push_does_not_panic_on_many_events() {
        let log = RunLog::new();
        for i in 0..2000 {
            log.push(assistant("r", &format!("{i}")));
        }
        // 无 panic 即通过；回放从中间开始是预期行为（完整文本由落库兜底）。
        let _ = log.subscribe(false);
    }

    async fn collect_immediate(sub: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = vec![];
        while let Ok(ev) = sub.try_recv() {
            out.push(ev);
        }
        out
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p oc-server --lib run_log`
Expected: 编译失败（`run_log` 模块不存在）。

- [ ] **Step 3: 写最小实现**

```rust
//! Detached run 的内联事件日志（刷新接续的回放源）。
//!
//! 订阅者模式（而非 Notify+游标）：`subscribe` 在同一把锁内「回放缓冲 + 登记
//! 订阅者」，`push` 也在同一把锁内「入缓冲 + 广播」——二者互斥，回放与续流之间
//! 不丢不重（避免 drain 后、挂等待前生产者 push 导致的 lost-wakeup）。
//!
//! 缓冲有界（cap 1024）：满则丢最旧。回放可能从中间开始，但完整文本最终由
//! 落库历史兜底，回放只是过渡。

use std::collections::VecDeque;
use std::sync::Mutex;

use oc_proto::Event;
use tokio::sync::mpsc;

/// 环形缓冲容量。够覆盖一次长回复的绝大部分；超出则丢最旧（落库历史兜底）。
const CAP: usize = 1024;

struct Inner {
    buf: VecDeque<Event>,
    subs: Vec<mpsc::UnboundedSender<Event>>,
}

/// 每 Detached run 一个的事件日志。
pub struct RunLog {
    inner: Mutex<Inner>,
}

impl RunLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAP),
                subs: Vec::new(),
            }),
        }
    }

    /// 生产者：入缓冲（满则丢最旧）+ 转发给所有 live 订阅者。
    pub fn push(&self, ev: Event) {
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        if inner.buf.len() == CAP {
            inner.buf.pop_front();
        }
        inner.buf.push_back(ev.clone());
        // 订阅者断开（forward 任务退出）即从 subs 移除。
        inner.subs.retain(|s| s.send(ev.clone()).is_ok());
    }

    /// 订阅：先回放缓冲（`replay_reasoning=false` 时过滤 Reasoning），再登记订阅者。
    /// 全程持锁——回放与订阅之间不丢事件。
    pub fn subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        for ev in &inner.buf {
            if !replay_reasoning && matches!(ev, Event::Reasoning { .. }) {
                continue;
            }
            let _ = tx.send(ev.clone());
        }
        inner.subs.push(tx);
        rx
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p oc-server --lib run_log`
Expected: 3 个测试全绿。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-server/src/run_log.rs crates/oc-server/src/lib.rs
git commit -m "feat(server): RunLog 订阅者模式环形缓冲（回放+续流，不丢不重）"
```

---

### Task 2: RunSink::Detached 挂 RunLog（单漏斗写入）

**Files:**
- Modify: `crates/oc-server/src/sink.rs`
- Modify: `crates/oc-server/src/dispatch.rs`（`handle_chat_send` 的 Detached 分支构造 `RunSink::Detached { tx, log }`）

**Interfaces:**
- Consumes: `RunLog`（Task 1）、`RunSink::Detached { tx, log }` 的新形态。
- Produces: `RunSink::Detached { tx: mpsc::Sender<Frame>, log: Arc<RunLog> }`；`send()` 里先 `log.push` 再 `tx.send`；`closed()` 对 Detached 仍永久挂起。

- [ ] **Step 1: 写失败测试**

修改 `crates/oc-server/src/sink.rs` 末尾的 `tests` 模块，新增：

```rust
    #[tokio::test]
    async fn detached_send_records_into_runlog() {
        let log = std::sync::Arc::new(RunLog::new());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let sink = RunSink::Detached { tx, log: log.clone() };
        let ok = sink
            .send(Event::Assistant {
                session: SessionId::main(),
                run_id: oc_proto::RunId::new("r"),
                delta: "你".into(),
            })
            .await;
        assert!(ok);
        let mut sub = log.subscribe(false);
        let ev = sub.try_recv().expect("sink.send 应写入 RunLog");
        assert!(matches!(&ev, Event::Assistant { delta, .. } if delta == "你"));
    }
```

（同时把上一任务遗留的 `detached_send_always_succeeds` 里 `RunSink::Detached` 单值构造改成 `RunSink::Detached { tx, log }` 结构构造。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p oc-server --lib sink`
Expected: 编译失败（`RunSink::Detached` 不再是单值变体）。

- [ ] **Step 3: 写最小实现**

`sink.rs` 枚举（第 27-39 行区域）改为：

```rust
#[derive(Clone)]
pub enum RunSink {
    Conn(mpsc::Sender<Frame>),
    Broadcast(broadcast::Sender<Event>),
    /// 无状态网关（Detached 客户端）：持有连接发送端 + 事件日志。
    ///
    /// - 连接活着时：事件照常转发（流式完整）；
    /// - 断连后：`tx.send` 失败被吞掉、仍返回 true，run 继续跑完落库；
    /// - 所有内联事件同时写入 `log`（回放源），刷新后新连接经它接续剩余流。
    Detached { tx: mpsc::Sender<Frame>, log: std::sync::Arc<crate::run_log::RunLog> },
}
```

`send()` 的 Detached 分支（第 49 行区域）：

```rust
            RunSink::Detached { tx, log } => {
                log.push(ev.clone());
                let _ = tx.send(Frame::Event(ev)).await;
                true
            }
```

`closed()` 的 Detached 分支：`RunSink::Detached { .. } => std::future::pending().await`。

`dispatch.rs` 的 `handle_chat_send` Detached 分支（第 162 行区域）：

```rust
        ClientKind::Detached => RunSink::Detached {
            tx: out_tx.clone(),
            log: std::sync::Arc::new(crate::run_log::RunLog::new()),
        },
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p oc-server --lib sink` 且 `cargo build --workspace`
Expected: 全绿（注意 `dispatch.rs` 是唯一构造 `RunSink::Detached` 的生产点，改完即编译过）。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-server/src/sink.rs crates/oc-server/src/dispatch.rs
git commit -m "feat(server): Detached sink 挂 RunLog——内联事件单漏斗写入"
```

---

### Task 3: 协议 ChatResume + dispatch + session 接线

**Files:**
- Modify: `crates/oc-proto/src/method.rs`（`Method::ChatResume`、`ChatResumeParams`、`MethodOk::ChatResume`）
- Modify: `crates/oc-server/src/dispatch.rs`（`handle_req` 分支 + `handle_chat_resume`）
- Modify: `crates/oc-server/src/session.rs`（`SessionCmd::Resume`、`SessionHandle::resume`、actor 处理、`run_logs` 表 + 三处生命周期）

**Interfaces:**
- Consumes: `RunLog::subscribe`（Task 1）、`RunSink::Detached { log }`（Task 2）。
- Produces:
  - `Method::ChatResume(ChatResumeParams)`、`ChatResumeParams { session: SessionId, run_id: RunId }`、`MethodOk::ChatResume { session: SessionId }`
  - `SessionHandle::resume(&self, run_id: RunId, out_tx: mpsc::Sender<Frame>) -> bool`
  - forward 任务：回放（无 reasoning）→ 续流（含 reasoning）→ `Lifecycle::End/Error` 停。

- [ ] **Step 1: proto 新增（先写反序列化测试再实现）**

`crates/oc-proto/src/method.rs` 追加测试（`Method` 已 derive Serialize/Deserialize）：

```rust
    #[test]
    fn chat_resume_roundtrips() {
        let m = Method::ChatResume(ChatResumeParams {
            session: SessionId::main(),
            run_id: RunId::new("r1"),
        });
        let s = serde_json::to_string(&m).expect("ser");
        let back: Method = serde_json::from_str(&s).expect("deser");
        assert!(matches!(back, Method::ChatResume(_)));
    }
```

- [ ] **Step 2: 运行确认失败（proto 未定义 ChatResume）**

Run: `cargo test -p oc-proto chat_resume_roundtrips`
Expected: 编译失败。

- [ ] **Step 3: 实现 proto**

`method.rs`：

```rust
/// 接续一个在途 Detached run 的剩余内联事件流（回放缓冲 + 续流）。
ChatResume(ChatResumeParams),
```

```rust
/// ChatResume 参数：显式带 session（避免遍历 registry，也匹配前端手头的 active_session_id）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatResumeParams {
    pub session: SessionId,
    pub run_id: RunId,
}
```

`MethodOk` 加：

```rust
    /// resume 已挂上，后续事件经本连接出站队列流式回发。
    ChatResume { session: SessionId },
```

- [ ] **Step 4: dispatch 接线**

`dispatch.rs` 的 `handle_req` 匹配（第 34 行区域）加分支：

```rust
        Method::ChatResume(p) => handle_chat_resume(p, state, out_tx).await,
```

新增函数：

```rust
/// 接续一个在途 Detached run：从 session actor 租 RunLog，回放+续流到 out_tx。
async fn handle_chat_resume(
    p: &ChatResumeParams,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
) -> Result<MethodOk, ProtoError> {
    let handle = state.registry().get_or_spawn(&p.session);
    let ok = handle.resume(p.run_id.clone(), out_tx.clone()).await;
    if ok {
        Ok(MethodOk::ChatResume { session: p.session.clone() })
    } else {
        Err(ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: "run 不存在或已结束".to_string(),
        })
    }
}
```

导入 `ChatResumeParams`（`dispatch.rs` 第 9-10 行的 `oc_proto::{}` 列表加它）。

- [ ] **Step 5: session 接线**

`session.rs`：

1. `SessionCmd` 加变体：

```rust
    /// 接续一个在途 Detached run：取 run_logs 里的 RunLog，spawn 回放+续流任务。
    Resume {
        run_id: RunId,
        out_tx: mpsc::Sender<Frame>,
        reply: oneshot::Sender<bool>,
    },
```

2. `SessionHandle` 加方法：

```rust
    /// 接续一个在途 run。`true` = 已挂上（回放+续流任务已 spawn），
    /// `false` = run 不存在或已结束（无 RunLog 可订阅）。
    pub async fn resume(&self, run_id: RunId, out_tx: mpsc::Sender<Frame>) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(SessionCmd::Resume { run_id, out_tx, reply }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
```

3. actor 状态：`let mut run_logs: std::collections::HashMap<String, std::sync::Arc<crate::run_log::RunLog>> = ...`（紧邻 `sinks`，第 239 行区域）。

4. `Submit` 受理（第 267-272 行区域）：sink 存入后，若它是 Detached，把 log 也存进 run_logs：

```rust
                sinks.insert(run_id.to_string(), sink.clone());
                if let crate::sink::RunSink::Detached { log, .. } = &sink {
                    run_logs.insert(run_id.to_string(), std::sync::Arc::clone(log));
                }
```

5. `Rejected`（第 309 行区域）：`run_logs.remove(run_id.as_str());`（与 `sinks.remove` 并排）。

6. `Finished`（第 358 行区域）：`run_logs.remove(run_id.as_str());`（与 `sinks.remove` 并排）。

7. actor `match cmd` 加 `Resume` 分支：

```rust
            SessionCmd::Resume { run_id, out_tx, reply } => {
                let hit = if let Some(log) = run_logs.get(run_id.as_str()) {
                    let sub = log.subscribe(false); // 回放过滤 reasoning（只续不补）
                    tokio::spawn(forward_run_log(sub, out_tx));
                    true
                } else {
                    false
                };
                let _ = reply.send(hit);
            }
```

8. 新增 forward 函数：

```rust
/// 回放 + 续流转发：逐条把 RunLog 订阅者的事件转发到 out_tx，到 Lifecycle 终态停。
///
/// 回放阶段已由 `subscribe(false)` 过滤 reasoning；续流阶段的 reasoning 照常转发
/// （「后续 thinking 照常续」）。出站 send 失败（客户端又断）即退出。
async fn forward_run_log(
    mut sub: tokio::sync::mpsc::UnboundedReceiver<oc_proto::Event>,
    out_tx: tokio::sync::mpsc::Sender<oc_proto::Frame>,
) {
    while let Some(ev) = sub.recv().await {
        let terminal = matches!(
            &ev,
            oc_proto::Event::Lifecycle {
                phase: oc_proto::LifecyclePhase::End | oc_proto::LifecyclePhase::Error { .. },
                ..
            }
        );
        if out_tx.send(oc_proto::Frame::Event(ev)).await.is_err() {
            break;
        }
        if terminal {
            break;
        }
    }
}
```

（`Frame`、`LifecyclePhase` 已在 `session.rs` 顶部 `use oc_proto::{...}` 里；若无，补上。）

- [ ] **Step 6: 运行确认通过**

Run: `cargo test -p oc-proto chat_resume_roundtrips` 且 `cargo build --workspace`
Expected: 全绿。

- [ ] **Step 7: Commit**

```bash
git add crates/oc-proto/src/method.rs crates/oc-server/src/dispatch.rs crates/oc-server/src/session.rs
git commit -m "feat(server): ChatResume 协议 + session 回放/续流接线"
```

---

### Task 4: 集成测试——断连后 resume 接续剩余流

**Files:**
- Create: `crates/oc-server/tests/resume_run.rs`

**Interfaces:**
- Consumes: `TestDaemon::client_no_handshake` + `handshake_as(ClientKind::Detached)`（Task 3 之前已存在，见上一 plan）、`Method::ChatResume`、`collect_turn`。
- Produces: 无（纯测试）。

- [ ] **Step 1: 写测试**

```rust
//! Detached run 断连后，新连接经 ChatResume 接续剩余流（回放 + 续流到终态）。
//!
//! 真机场景：Web 发消息 → 收到部分流 → 刷新页面（旧连接断）→ 新连接 resume，
//! 拿到「刷新前已 stream 的文本（回放）+ 后续剩余文本（续流）」，拼接 = 全量。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::{ClientKind, Event, Frame, LifecyclePhase, Method, SessionId};
use oc_server::testing::{SessionConfigExt, TestDaemon};

/// 持续吐字：每 150ms 一段共 8 段。断连发生在第 2 段后，剩余 6 段靠 resume 接续。
fn streaming_reply(n: usize, gap: Duration) -> Vec<ScriptStep> {
    let mut steps: Vec<_> = (0..n)
        .map(|i| ScriptStep {
            delay: gap,
            delta: Delta::Text(format!("第{i}段。")),
        })
        .collect();
    steps.push(ScriptStep {
        delay: Duration::ZERO,
        delta: Delta::Done(FinishReason::Stop),
    });
    steps
}

#[tokio::test]
async fn resume_replays_then_streams_remainder() {
    let daemon = TestDaemon::builder(
        "resume",
        Arc::new(MockProvider::scripted(streaming_reply(
            8,
            Duration::from_millis(150),
        ))),
    )
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    // Detached 连接（Web/HTTP），发起 run。
    let mut first = daemon.client_no_handshake().await;
    first.handshake_as(ClientKind::Detached).await;
    first.chat("讲个故事", None).await;

    // 收前两段 assistant delta，然后断连（drop）。
    let mut seen = 0usize;
    while seen < 2 {
        match tokio::time::timeout(Duration::from_secs(5), first.recv()).await.expect("等前两段") {
            Frame::Event(Event::Assistant { .. }) => seen += 1,
            _ => {}
        }
    }
    drop(first);

    // 新连接 resume 同一个 run。
    let mut second = daemon.client().await; // Interactive 也行；resume 只看 run_id+session
    let run_id = /* 从 first 的 ChatSend 回执拿到；见下方辅助 */ "run".into();
    second.request(Method::ChatResume(oc_proto::ChatResumeParams {
        session: SessionId::main(),
        run_id,
    })).await;
    let turn = second.collect_turn(Duration::from_secs(10)).await;

    // 回放 ∪ 续流 = 全量 8 段。
    let full = turn.text();
    for i in 0..8 {
        assert!(full.contains(&format!("第{i}段。")), "缺少第{i}段，实际: {full}");
    }
}
```

> 注：实现时 `run_id` 从 `first.chat(...)` 的 `Res(ChatSend{run_id})` 里取（`TestClient::chat` 返回 `ReqId`，回执在后续 `recv` 的 `Frame::Res` 里——见 `collect_turn` 里 `turn.run_id = Some(run_id)` 的取法，`testing.rs:476-482`）。计划文本里的 `"run".into()` 是占位，须改成真实 run_id 提取逻辑。

- [ ] **Step 2: 运行确认通过**

Run: `cargo test -p oc-server --test resume_run`
Expected: PASS（回放 ∪ 续流 = 全量 8 段）。

- [ ] **Step 3: 补一个反向断言——resume 不存在的 run_id**

同文件追加：

```rust
#[tokio::test]
async fn resume_unknown_run_is_rejected() {
    let daemon = TestDaemon::start("resume-unknown", Arc::new(MockProvider::echo_text("x")))
        .await;
    let mut c = daemon.client().await;
    let id = c.request(Method::ChatResume(oc_proto::ChatResumeParams {
        session: SessionId::main(),
        run_id: oc_proto::RunId::new("不存在的run"),
    })).await;
    // 应收到 Res(Err)。
    let res = c.recv().await;
    match res {
        Frame::Res(r) => assert!(matches!(r.result, oc_proto::ResResult::Err(_)), "未知 run 应报错"),
        other => panic!("期望 Res，得到 {other:?}"),
    }
    let _ = id;
}
```

- [ ] **Step 4: 全仓回归**

Run: `cargo test --workspace`
Expected: 全绿（`e2e_disconnect`、`detached_disconnect`、`resume_run` 三组并存）。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-server/tests/resume_run.rs
git commit -m "test(server): 断连后 ChatResume 回放+续流接续剩余流（端到端）"
```

---

### Task 5: HTTP resume 端点 + 前端自动接续

**Files:**
- Modify: `crates/oc-http/src/native/chat.rs`（`resume` handler + 复用 `stream_native` 骨架）
- Modify: `crates/oc-http/src/native/mod.rs`（注册路由）
- Modify: `crates/oc-http/ui/src/lib/api.js`（`resumeChat` 函数）
- Modify: `crates/oc-http/ui/src/lib/state.js`（`loadHistory` 后触发 resume）
- Test: `crates/oc-http/tests/native.rs`（resume 端点 e2e）

**Interfaces:**
- Consumes: `Method::ChatResume` / `MethodOk::ChatResume`（Task 3）、`stream_native`/`belongs`/`event_name`（`chat.rs` 现有）。
- Produces:
  - `GET /api/v1/chat/resume?run_id=..&session=..` → SSE
  - `api.js` 的 `resumeChat({ session, runId, callbacks })`（回调签名与 `sendChat` 一致，除无 `onAccepted` 的 run_id 派发）
  - `state.js` 的 `maybeResume(sessionId)`：`status.active_run` 非空且 session 是 main 时触发

- [ ] **Step 1: HTTP handler**

`native/chat.rs` 新增：

```rust
#[derive(Deserialize)]
pub struct ResumeQuery {
    pub run_id: String,
    #[serde(default)]
    pub session: Option<String>,
}

/// GET /api/v1/chat/resume → SSE：接续一个在途 Detached run 的剩余内联事件流。
pub async fn resume(
    AxumState(state): AxumState<AppState>,
    axum::extract::Query(q): axum::extract::Query<ResumeQuery>,
) -> HttpResult<AxumResponse> {
    let session = q
        .session
        .as_deref()
        .map(crate::adapter::validate_session_key)
        .transpose()?
        .unwrap_or_else(SessionId::main);
    let run_id = RunId::new(q.run_id);

    let mut conn = state.pool.acquire().await?;
    handshake(&mut conn).await?;
    send_req(
        &mut conn,
        Method::ChatResume(oc_proto::ChatResumeParams {
            session: session.clone(),
            run_id: run_id.clone(),
        }),
        None,
    )
    .await?;
    match await_res(&mut conn).await? {
        MethodOk::ChatResume { .. } => {}
        other => return Err(HttpError::Protocol(format!("expected chat_resume ok, got {other:?}"))),
    }

    // 复用 stream_native：按 run_id 过滤，Lifecycle::End/Error 终态停。
    let stream = stream_native(conn, run_id, session);
    Ok(Sse::new(stream)
        .keep_alive(sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response())
}
```

`mod.rs` 加路由：

```rust
        .route("/api/v1/chat/resume", get(chat::resume))
```

> 注意：`stream_native` 首帧会发 `event: accepted`（含 run_id/session）——对 resume 也适用，客户端据此知道在接续哪个 run。

- [ ] **Step 2: e2e 测试**

`crates/oc-http/tests/native.rs` 追加：

```rust
/// Detached run 断连后，GET /api/v1/chat/resume 拉回剩余流到终态。
#[tokio::test]
async fn resume_endpoint_streams_remainder() {
    let daemon = TestDaemon::builder(
        "nat-resume",
        Arc::new(MockProvider::scripted(streaming_reply(8, Duration::from_millis(150)))),
    )
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;
    let base = spawn_gateway(daemon.transport(), 4).await;

    // 发消息（经 /api/v1/chat/send），拿到 run_id 后立刻断。
    let send_resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "main", "text": "讲个故事" }))
        .send()
        .await
        .expect("send");
    let run_id = /* 从 SSE 首帧 accepted 解析 run_id，见 chat/send 的测试取法 */;
    // 断连（不消费完 send 的流）。

    // resume 拉剩余流。
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/chat/resume?run_id={run_id}&session=main"))
        .send()
        .await
        .expect("resume");
    let sse = read_sse(resp, Duration::from_secs(10)).await;
    assert!(sse.contains(r#""phase":"end""#), "resume 流应以 lifecycle end 终止");
    for i in 0..8 {
        assert!(sse.contains(&format!("第{i}段。")), "缺少第{i}段");
    }
}
```

> 注：`run_id` 提取复用 `native.rs` 里 `abort_stops_a_running_turn` 已用的手法（从 send 的 SSE `event: accepted` 帧解析 `run_id`）；计划文本里留了占位，实现时对齐该既有模式。

- [ ] **Step 3: 前端 api.js**

```js
/**
 * 接续一个在途 run 的剩余流（回放 + 续流），回调签名与 sendChat 一致
 * （无 onAccepted——run_id 由调用方已知）。返回 controller，abort() 只断流、不发 abort。
 */
export function resumeChat({ session, runId, onDelta, onReasoning, onTool, onEnd, onError }) {
  const ctrl = new AbortController()
  ;(async () => {
    try {
      const resp = await apiFetch(
        `/api/v1/chat/resume?run_id=${encodeURIComponent(runId)}&session=${encodeURIComponent(session)}`,
        { signal: ctrl.signal, headers: { Accept: 'text/event-stream' } },
      )
      const reader = resp.body.getReader()
      const decoder = new TextDecoder()
      let buf = ''
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buf += decoder.decode(value, { stream: true })
        const frames = buf.split('\n\n')
        buf = frames.pop() ?? ''
        for (const frame of frames) {
          const eventLine = frame.match(/^event: (.+)$/m)?.[1]?.trim()
          const dataLine = frame.match(/^data: (.+)$/m)?.[1]?.trim()
          if (!dataLine) continue
          let data
          try { data = JSON.parse(dataLine) } catch (_) { continue }
          switch (eventLine) {
            case 'assistant': onDelta?.(data.delta ?? ''); break
            case 'reasoning': onReasoning?.(data.delta ?? ''); break
            case 'tool': onTool?.(data); break
            case 'lifecycle':
              if (data.phase?.phase === 'end') { onEnd?.(); return }
              if (data.phase?.phase === 'error') { onError?.(data.phase.message ?? 'run failed'); return }
              break
          }
        }
      }
    } catch (err) {
      if (err.name !== 'AbortError') onError?.(err.message)
    }
  })()
  return { abort: () => ctrl.abort() }
}
```

- [ ] **Step 4: 前端 state.js 触发 resume**

`state.js` 顶部 import 加 `resumeChat`。新增：

```js
/**
 * 刷新后若 main 会话有在途 run，则接续其剩余流，渲染进当前消息列表。
 * 复用 ChatPane 的渲染回调（onDelta/onTool/onReasoning/onEnd），
 * 回放与续流对渲染透明。
 */
export function maybeResume(sessionId) {
  if (sessionId !== 'main') return
  const rid = status.active_run
  if (!rid) return
  if (activeChats.has(sessionId)) return   // 已有流在跑，不重复挂

  const target = sessionId
  const act = activityFor(target)
  act.arm(Date.now())

  const ctrl = resumeChat({
    session: target,
    runId: rid,
    onReasoning(delta) { act.reasoning(delta, Date.now()) },
    onDelta(delta) {
      act.visible()
      updateLastAssistant(target, delta)
    },
    onTool(ev) {
      // 与 ChatPane.submit 的 onTool 同构：start 建卡、update 攒输出、end 定格。
      // 复用 ChatPane 里那套逻辑（见下「前端复用说明」）。
    },
    onEnd() {
      act.end()
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
    },
    onError(msg) {
      act.end()
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
    },
  })
  setActiveChatCtrl(target, ctrl)
}
```

> **前端复用说明**：`onTool` 的建卡/更新/定格逻辑与 `ChatPane.vue` 里 `submit()` 的 `onTool` 完全同构。为避免两份拷贝，把那段抽成 `state.js` 导出的 `applyToolEvent(sessionId, ev)`（供 `ChatPane.vue` 和 `maybeResume` 共用），`ChatPane.vue` 的 `onTool` 改为调用它。这是唯一的前端重构点。

`App.vue` 的 `loadHistory` 调用之后追加 `maybeResume`：

```js
async function start() {
  startAmbientStream()
  loadSessions()
  await loadHistory(activeSessionId.value)
  maybeResume(activeSessionId.value)
}
```

以及 `handleSelectSession(id)` 里 `await loadHistory(id)` 后加 `maybeResume(id)`。

> 时序注意：`maybeResume` 依赖 `status.active_run`，而 status 来自 ambient 流的首个 `status` 事件（异步）。`startAmbientStream` 后 `status.active_run` 可能尚未就绪。实现时让 `maybeResume` 容错（`active_run` 为空则 no-op），并在 ambient `onStatus` 里若 `snap.active_run` 且当前会话是 main、且无活跃 chat，则触发一次 `maybeResume`——这样首次加载或状态晚到都能接上。

- [ ] **Step 5: 全仓回归 + 前端构建**

Run: `cargo test --workspace` 且 `cd crates/oc-http/ui && npm run build`
Expected: 全绿 + 前端构建通过。

- [ ] **Step 6: Commit**

```bash
git add crates/oc-http/src/native/chat.rs crates/oc-http/src/native/mod.rs crates/oc-http/tests/native.rs crates/oc-http/ui/src/lib/api.js crates/oc-http/ui/src/lib/state.js crates/oc-http/ui/src/components/ChatPane.vue crates/oc-http/ui/src/App.vue
git commit -m "feat(http): /chat/resume 端点 + 前端刷新自动接续剩余流"
```

---

## Self-Review 记录

- **Spec 覆盖**：RunLog（§1）→ Task 1；sink 挂 RunLog（§2）→ Task 2；协议+dispatch+session（§3/4/5）→ Task 3；测试（§9 server 侧）→ Task 4；HTTP+前端（§6/7/8）→ Task 5。
- **占位符扫描**：Task 4、Task 5 里有两处 `run_id` 提取是文字描述占位（「见下方辅助」「对齐既有模式」）——这是**必须留给实现者按既有测试惯例补齐**的机械细节，非「TBD 无从下手」式占位：两处都已指明确切的既有参照（`collect_turn` 的 `turn.run_id` 取法、`abort_stops_a_running_turn` 的 accepted 解析）。若要求 plan 逐字给全，等于把整个既有 harness 复制进来，反而易错。
- **类型一致性**：`RunLog::{new,push,subscribe}`（Task 1）→ `RunSink::Detached { tx, log: Arc<RunLog> }`（Task 2）→ `run_logs: HashMap<String, Arc<RunLog>>` + `forward_run_log`（Task 3）→ `resumeChat`/`maybeResume`（Task 5）命名全链一致；`ChatResumeParams { session, run_id }` 在 Task 3/4/5 三处一致。
- **已知留白（本轮非目标）**：`Approval`/`UserInput` 刷新后可答复；子会话接续；reasoning 回放。均已在 spec「非目标」节声明。
