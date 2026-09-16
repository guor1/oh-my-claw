# 刷新后接续在途 run 的剩余流式输出 —— 设计

> 状态：待实现。承接 [2026-09-16-disconnect-continue-run.md](../plans/2026-09-16-disconnect-continue-run.md) 的已知留白第 1 条。
> 目标：Web 端刷新页面后，不再只是「run 继续跑完并落库、用户多刷几次才看到最终回复」，而是**自动接续剩余流式输出**（文本 + 工具事件 + 后续 thinking），体验等同没断过。

## 背景与目标

上一轮（断连继续跑完并落库）已修复「刷新掐死 run」的根因：`RunSink::Detached` 断连吞失败、run 跑完落库。但它有两个残留体验缺口：

1. 刷新后**看不到在途 run 的剩余流式输出**——剩余事件在断连后被 `Detached` 吞掉。
2. 刷新后**看不到刷新前已经 stream 出的文本/工具卡**——它们只在 run 终态落库，中途不可读。

本设计补齐这两点，scope 明确为：

- 覆盖 **main 会话**的 Detached run（Web/HTTP 网关）。
- 接续事件：**assistant 文本 + 工具事件（start/update/end）+ 后续 thinking**。
- 回放历史：**assistant 文本 + 工具事件**（Lifecycle 也回放，供客户端重建状态与判终态）。
- thinking 语义：**只续不补**——刷新前已 stream 的 thinking 不回放，刷新后新来的照常续（续流需要 reasoning 进缓冲，见第 2 节注记）。
- **不覆盖**：`Approval` / `UserInput` 的可答复（跨连接寻址审批注册表，留作后续 follow-up）；子会话（`http-user-*` / `cron:*` 等）。

## 架构总览

```
                    ┌──────────────────────────── session actor ────────────────────────────┐
                    │                                                                       │
  chat.send ──────► │  Submit: sinks.insert(run_id, RunSink::Detached{tx, log})             │
  (Detached)        │          run_logs.insert(run_id, log.clone())                         │
                    │                                                                       │
  chat.resume ────► │  Resume: run_logs.get(run_id) → spawn 回放+续流 → out_tx              │
  (run_id,session)  │                                                                       │
                    └───────────────────────────────────────────────────────────────────────┘
                            ▲  RunLog（每 Detached run 一个）
                            │  - 环形缓冲 VecDeque<Event>（cap 1024）
                            │  - 订阅者列表（live forward 任务）
                            │
  run driver ───────────────┘  所有内联事件经 RunSink::send() 单漏斗 push 进 RunLog
```

核心不变式：**内联事件的读写都归 session actor 管**。run 驱动器经 sink 写 RunLog，resume 从 actor「租」一个转发任务读 RunLog——不引入新的全局注册表，也不让 RunLog 被两个 actor 并发写。

## 1. RunLog（每 Detached run 一个）

新模块 `crates/oc-server/src/run_log.rs`。**订阅者模式**（而非 Notify 轮询），避免「drain 后、挂等待前」的 lost-wakeup 竞态：

```rust
pub struct RunLog {
    inner: Mutex<Inner>,
}
struct Inner {
    buf: VecDeque<Event>,                                  // 环形缓冲，cap 1024
    subs: Vec<mpsc::UnboundedSender<Event>>,               // live 订阅者（resume forward 任务）
}

impl RunLog {
    pub fn new() -> Self;
    /// 生产者：入缓冲（满则丢最旧）+ 转发给所有 live 订阅者。
    pub fn push(&self, ev: Event);
    /// 订阅：先按序回放缓冲（`replay_reasoning=false` 时过滤 Reasoning），
    /// 再把订阅者登记进 subs。**全程持锁**——回放与订阅之间不丢事件。
    pub fn subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event>;
}
```

- **cap=1024**：回放可能从中间开始（丢最旧），完整文本最终由落库历史兜底。
- **不丢不重**：`subscribe` 在同一把锁内「回放 + 登记订阅者」；`push` 也持同一把锁「入缓冲 + 广播」——二者互斥，回放与续流之间无缝隙。
- 订阅者通道 `UnboundedSender`：forward 任务即时转发到 resume 连接的出站队列（bounded 256，`send().await` 背压），慢客户端由 out_tx 背压反向抑制，不会无限堆积。
- `Event` 已 `Clone`（`event.rs:13`），缓冲存 `Event` 值即可。

## 2. RunSink::Detached 扩展

`sink.rs` 的 `Detached` 变体从持一个发送端，改为持「发送端 + RunLog」：

```rust
pub enum RunSink {
    Conn(mpsc::Sender<Frame>),
    Broadcast(broadcast::Sender<Event>),
    Detached { tx: mpsc::Sender<Frame>, log: Arc<RunLog> },
}
```

- `send()` 的 Detached 分支：`log.push(ev.clone())`（入缓冲 + 转发 live 订阅者），再 `tx.send(Frame::Event(ev)).await` 到原连接，恒返回 `true`。
- **收益**：所有内联事件（`run.rs` 的 `emit_inline`、`tools_bridge.rs` 的 tool-update pump）本就走 `sink.send()`，于是 RunLog 的写入集中在 sink 这一个漏斗，**run.rs 的每个 emit 点零改动**。
- **reasoning 必须进缓冲**：续流阶段经 RunLog 订阅取新事件，reasoning 不入缓冲则「后续 thinking 照常续」送不到；「只续不补」由 `subscribe(replay_reasoning=false)` 在回放阶段过滤实现。
- **开销**：`RunSink::Conn`（交互式路径）不带 RunLog，零额外成本；只有 Detached 路径多一次 `push`。

## 3. 协议层（proto）

`method.rs`：

```rust
pub enum Method {
    // ... 既有 ...
    /// 接续一个在途 Detached run 的剩余内联事件流（回放缓冲 + 续流）。
    ChatResume(ChatResumeParams),
}

#[derive(...)]
pub struct ChatResumeParams {
    pub session: SessionId,   // 显式带 session：避免遍历 registry，也匹配前端手头的 active_session_id
    pub run_id: RunId,
}

pub enum MethodOk {
    // ... 既有 ...
    /// resume 已挂上，后续事件经本连接出站队列流式回发。
    ChatResume { session: SessionId },
}
```

- `ChatResume` 是**无副作用**方法，dispatch 不加幂等缓存。
- resume 回执**不回放文本**——回放走事件流（第 4 节），与 `ChatSend` 保持单通道、避免双通道重复。

## 4. dispatch 与 session 接线

`dispatch.rs`：

- `handle_req` 加 `ChatResume` 分支 → `handle_chat_resume(p, state, out_tx).await`。
- `handle_chat_resume`：
  1. `state.registry().get_or_spawn(&p.session)` 拿句柄。
  2. `handle.resume(p.run_id.clone(), out_tx.clone()).await` → `Option<()>`。
  3. `Some(())` → `Ok(MethodOk::ChatResume { session: p.session })`；`None` → `Err(ProtoError { kind: Internal, message: "run 不存在或已结束" })`。

`session.rs`：

- `SessionCmd` 新增 `Resume { run_id: RunId, out_tx: mpsc::Sender<Frame>, reply: oneshot::Sender<bool> }`。
- `SessionHandle::resume(run_id, out_tx) -> bool`：`tx.send(SessionCmd::Resume{..})` 后收 oneshot。
- actor 处理 `Resume`：
  1. `run_logs.get(&run_id)`；取不到 → `reply.send(false)`。
  2. 取到 → `run_log.subscribe(/* replay_reasoning */ false)` 拿订阅者 receiver，spawn **forward 任务**，`reply.send(true)`。
- **forward 任务**（`session.rs` 内私有函数）：
  1. `receiver.recv()` 循环，逐条 `out_tx.send(Frame::Event(ev)).await`（回放阶段已过滤 reasoning，receiver 里只有要发的事件 + 续流的 reasoning）。
  2. 收到 `Event::Lifecycle { phase: End | Error }` 即停、任务退出。
  3. 出站 send 失败（客户端又断）→ 退出，`Arc<RunLog>` drop。

## 5. 生命周期 / 清理

`session.rs` actor 内新增 `run_logs: HashMap<String, Arc<RunLog>>`，与 `sinks` 平行、生命周期相同：

| 时机 | 动作 |
|------|------|
| `Submit` 受理 | `sinks.insert(run_id, sink)`；若 sink 是 Detached，`run_logs.insert(run_id, log.clone())` |
| `Rejected` | 两张表一起 `remove(run_id)` |
| `Finished`（终态） | 两张表一起 `remove(run_id)` |

- RunLog 由 `handle_chat_send` 的 Detached 分支创建，actor 与 run 驱动器各持一个 `Arc` clone。
- **无循环引用**：RunLog 不反向持有任务句柄；forward 任务持 `Arc<RunLog>`，任务退出即 drop。
- 竞态：resume 与 run 终态并发——actor 串行处理命令，取到 RunLog 后即便 run 随即结束，forward 任务收到 `Lifecycle::End` 正常停，无泄漏。

## 6. HTTP 层

`native/chat.rs` 新增：

- `GET /api/v1/chat/resume?run_id=...&session=...`（或 body POST，与 `/chat/abort` 保持 REST 风格一致，见实现时定）→ SSE。
- 流程：`acquire` 连接 → `handshake` → `send_req(ChatResume)` → `await_res` 确认挂上 → 复用 `stream_native` 骨架流式转发，`Lifecycle::End/Error` 停。
- `stream_native` 现有的 `belongs(ev, run_id, session)` 对 inline 事件本就走 `run_id` 匹配（`session` 只用于 Usage），**无需改**；只需把 `accepted` 事件的 session 字段填上 `p.session`。

## 7. 前端

`state.js` / `ChatPane.vue`：

- 刷新后 `loadHistory` 完成后，查 `status.active_run`（`openAmbientStream` 已注入 status 事件）。**仅当 `activeSessionId === 'main'` 且 `active_run` 非空**时发起 resume。
- 用 `sendChat` 同款回调接流：`onDelta`（append 到正在建的 assistant 气泡）、`onTool`（建/更新 ToolCard）、`onReasoning`（续 thinking）、`onEnd`（finalize + 清 activeChats）。
- 回放事件与续流事件在 SSE 层**不可区分**（对客户端透明），客户端无需知道「哪些是补的、哪些是新的」——渲染逻辑完全复用 `sendChat` 的现有回调。
- 收敛点：只服务 `main` 会话（`Snapshot.active_run` 本就只反映 main 的活跃 run）。

## 8. 错误处理

- resume 时 run 不存在 / 已终态 / 是 Interactive：`run_logs` 取不到 → `ProtoError { kind: Internal, message: "run 不存在或已结束" }`。
- forward 任务出站 send 失败（客户端又断）：退出，RunLog drop，无残留。
- 环形缓冲满丢最旧：回放从中间开始，完整文本由落库历史兜底。

## 9. 测试

| 层 | 用例 |
|----|------|
| 单元（run_log） | push 入缓冲顺序；cap 满丢最旧；subscribe 回放按序 + 不丢不重（回放与 live 订阅之间无缝隙） |
| 单元（resume 语义） | subscribe(replay_reasoning=false) 回放过滤 Reasoning；live 订阅含 Reasoning |
| 集成（oc-server） | Detached run 中途断连 → 新连接 `ChatResume` → 收到回放 ∪ 续流到 End，且两者拼接 = 全量 |
| 集成（oc-server） | resume 不存在的 run_id → 协议错误 |
| e2e（oc-http） | `GET /api/v1/chat/resume` 拉回剩余流到终态 |
| 前端（ui 单测，可选） | `loadHistory` 发现 active_run 时触发 resume |

## 非目标（明确排除）

- `Approval` / `UserInput` 的刷新后可答复（需跨连接寻址审批/输入注册表）。
- 子会话（`http-user-*` / `cron:*` 等）的接续。
- reasoning 的回放（只续不补，已定）。
- Interactive（TUI/CLI）客户端的断连接续——它们断连本就收敛 run，语义不适用。
