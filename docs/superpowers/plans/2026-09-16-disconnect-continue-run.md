# 断连继续跑完并落库（Detached 客户端）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复「Web 端发出消息 → 收到 thinking 后立即刷新页面 → run 被断连掐死、回复既不继续也不落库」的缺陷。

**Architecture:** 给连接引入「客户端类型」（`ClientKind`），区分交互式客户端（TUI/CLI，断连即中止 run）与无状态网关（Web/HTTP，run 归属会话而非连接）。无状态网关的 run 使用一个新的 `RunSink::Detached`（`send` 恒成功、`closed` 永不完成），使断连不再触发 `sink.closed()` → cancel → Aborted，run 得以跑完并正常落库。这是 OpenClaw「run 与连接解耦、只与 delivery key 绑定」模型的最小对齐。

**Tech Stack:** Rust（edition 2021，rust-version 1.90）、tokio、serde、tracing、sqlite（oc-store）。

## Global Constraints

- 业务逻辑注释用中文（与本仓既有代码一致）。
- 所有新字段 `#[serde(default)]`，保证旧客户端/旧握手帧向后兼容（缺省 → 旧语义）。
- 默认客户端类型一律是 `Interactive`，只有 oc-http 网关显式发 `Detached`——交互式客户端的既有断连语义（断连即释放车道）**不得改变**。
- 不改 store schema、不加依赖、不改前端 JS（本轮只做后端根因修复）。
- `RunOutcome::Aborted` 依旧不落库——本计划的修复是「Detached 连接不再走到 Aborted」，而不是「Aborted 也落库」。

---

### Task 1: proto 新增 `ClientKind` 与 `ConnectParams.client_kind`

**Files:**
- Modify: `crates/oc-proto/src/method.rs`（`ConnectParams` 附近）

**Interfaces:**
- Consumes: 无（本仓首个改动）。
- Produces:
  - `pub enum ClientKind { Interactive, Detached }`，派生 `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema`，`#[serde(rename_all = "snake_case")]`，并实现 `Default`（默认 `Interactive`）。
  - `ConnectParams` 新增字段 `pub client_kind: ClientKind`，标注 `#[serde(default, skip_serializing_if = "ClientKind::is_interactive")]`。
  - `impl ClientKind { fn is_interactive(&self) -> bool }`。

- [ ] **Step 1: 写失败测试**

在 `crates/oc-proto/src/method.rs` 末尾追加测试模块（若已有 `#[cfg(test)] mod tests` 则并入）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 旧客户端（无 client_kind 字段）的握手帧反序列化后应为 Interactive——
    /// 这是向后兼容红线：老 CLI/TUI 不得因此被当成 Detached。
    #[test]
    fn connect_params_defaults_client_kind_to_interactive() {
        let p: ConnectParams =
            serde_json::from_str(r#"{"proto_version":1,"token":null}"#).expect("deser");
        assert_eq!(p.client_kind, ClientKind::Interactive);
    }

    #[test]
    fn client_kind_roundtrips() {
        let p = ConnectParams {
            proto_version: 1,
            token: None,
            client_kind: ClientKind::Detached,
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: ConnectParams = serde_json::from_str(&s).expect("deser");
        assert_eq!(back.client_kind, ClientKind::Detached);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p oc-proto connect_params_defaults_client_kind_to_interactive`
Expected: 编译失败（`ConnectParams` 无 `client_kind` 字段、`ClientKind` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `crates/oc-proto/src/method.rs` 的 `ConnectParams` 定义（当前第 81-86 行）**之前**插入枚举，并给 `ConnectParams` 加字段：

```rust
/// 连接的客户端类型：决定 run 是否随连接断开而中止。
///
/// - `Interactive`：TUI / CLI 等驻留客户端。断连即中止 run、立即释放车道
///   （原有语义，对应 `RunSink::Conn` 的 `closed()` 探测）。
/// - `Detached`：Web / HTTP 无状态网关。run 归属会话而非连接，断连不中止——
///   客户端刷新页面后 run 继续跑完并落库（对齐 OpenClaw 的 delivery-key 解耦）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Interactive,
    Detached,
}

impl Default for ClientKind {
    fn default() -> Self {
        ClientKind::Interactive
    }
}

impl ClientKind {
    fn is_interactive(&self) -> bool {
        matches!(self, ClientKind::Interactive)
    }
}
```

给 `ConnectParams`（第 81-86 行）加字段：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectParams {
    pub proto_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// 客户端类型（见 [`ClientKind`]）。旧客户端缺省为 Interactive。
    #[serde(default, skip_serializing_if = "ClientKind::is_interactive")]
    pub client_kind: ClientKind,
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p oc-proto`
Expected: 全绿（含新加的两个测试）。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-proto/src/method.rs
git commit -m "feat(proto): ConnectParams 增加 client_kind 区分交互式/无状态网关"
```

---

### Task 2: `RunSink` 新增 `Detached` 变体

**Files:**
- Modify: `crates/oc-server/src/sink.rs`

**Interfaces:**
- Consumes: 无（`oc_proto::{Event, Frame}`、`tokio::sync::{broadcast, mpsc}` 已在文件内）。
- Produces: `RunSink::Detached(mpsc::Sender<Frame>)` 变体。语义：
  - `send()` → **尽力转发**到连接（连接活着时事件照常送达，流式可用）；断连后 `tx.send()` 返回 `Err`，**吞掉并恒返回 `true`**（不触发「send 失败 → 断连」探测，run 不中止）。
  - `closed()` → 永久挂起（`std::future::pending().await`，断连感知永不触发）。

> **修正（2026-09-16 执行中）**：原设计 `Detached` 是无载荷单元变体、`send` 直接丢光事件——这会让 oc-http 的流式路径（`accumulate_response`/`stream_sse`/`stream_native`）永远收不到 `LifecyclePhase::End`，导致 oc-http 集成测试全部超时失败。改为**持有连接发送端**：连接活着时事件照常转发（流式完整），断连后吞掉失败并继续跑完落库。

- [ ] **Step 1: 写失败测试**

在 `crates/oc-server/src/sink.rs` 末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oc_proto::{Event, LifecyclePhase, SessionId};

    #[tokio::test]
    async fn detached_send_always_succeeds() {
        let sink = RunSink::Detached;
        let ok = sink
            .send(Event::Lifecycle {
                session: SessionId::main(),
                run_id: oc_proto::RunId::new("run"),
                phase: LifecyclePhase::Start,
            })
            .await;
        assert!(ok, "Detached sink 的 send 必须恒成功，才不会触发断连收敛");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p oc-server --lib sink::tests::detached_send_always_succeeds`
Expected: 编译失败（`RunSink::Detached` 未定义）。

- [ ] **Step 3: 写最小实现**

修改 `crates/oc-server/src/sink.rs`：在枚举（当前第 27-33 行）加变体，在 `send`（第 41-50 行）与 `closed`（第 58-63 行）各加一支：

```rust
pub enum RunSink {
    /// 定向到单条连接的出站队列（有界背压，不丢）。
    Conn(mpsc::Sender<Frame>),
    /// 广播（测试断言 / 沿用旧语义）。
    Broadcast(broadcast::Sender<Event>),
    /// 无连接归属（Detached 客户端）：事件静默丢弃、`closed()` 永不触发。
    ///
    /// 供 Web/HTTP 网关使用——run 归属会话而非连接，客户端刷新/断连不中止 run。
    /// `send` 恒真所以「send 失败 → 断连」探测失效，`closed` 永久挂起所以
    /// 静默等待期（等模型/审批/ask_user）也不会因断连收敛。
    Detached,
}
```

`send` 匹配（在 `RunSink::Conn` 分支前加）：

```rust
            RunSink::Detached => true,
```

`closed` 匹配（在 `RunSink::Broadcast` 分支旁加）：

```rust
            RunSink::Detached => std::future::pending().await,
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p oc-server --lib sink`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-server/src/sink.rs
git commit -m "feat(server): RunSink 新增 Detached 变体（断连不收敛）"
```

---

### Task 3: 把 `client_kind` 接到 dispatch 的 sink 选择

**Files:**
- Modify: `crates/oc-server/src/conn.rs`（`read_loop` 传 kind）
- Modify: `crates/oc-server/src/dispatch.rs`（`handle_req` / `handle_chat_send` 签名与 sink 选择）
- Modify: `crates/oc-http/src/proto.rs`（handshake 发 `Detached`）
- Modify: `crates/oc-cli/src/cli_client.rs`（发 `Interactive`）
- Modify: `crates/oc-tui/src/app.rs`（发 `Interactive`）
- Modify: `crates/oc-server/src/testing.rs`（发 `Interactive`，并加 `handshake_as`）

**Interfaces:**
- Consumes: `oc_proto::ClientKind`（Task 1）、`RunSink::Detached`（Task 2）。
- Produces:
  - `dispatch::handle_req(req: &Req, state: &Arc<ServerState>, out_tx: &mpsc::Sender<Frame>, client_kind: ClientKind) -> ResResult`（新增第 4 参）。
  - `dispatch::handle_chat_send(p, state, out_tx, client_kind: ClientKind)`：`Detached` → `RunSink::Detached`，否则 `RunSink::Conn(out_tx.clone())`。
  - `TestClient::handshake_as(client_kind: ClientKind)`；`handshake()` 委托为 `handshake_as(ClientKind::Interactive)`。

- [ ] **Step 1: 改 `dispatch.rs` 签名与 sink 选择**

`crates/oc-server/src/dispatch.rs`：

导入（当前第 8-10 行的 `oc_proto::{}` 列表）加 `ClientKind`：

```rust
use oc_proto::{
    ChatAbortParams, ChatSendParams, ClientKind, ConnectParams, Features, Frame, Method, MethodOk,
    ProtoError, Req, ResResult, SessionId, Snapshot, PROTO_VERSION,
};
```

`handle_req` 签名（当前第 21 行）加参数，并把 `ChatSend` 分发传下去：

```rust
pub async fn handle_req(
    req: &Req,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
    client_kind: ClientKind,
) -> ResResult {
```

分发处（当前第 32 行）改为：

```rust
        Method::ChatSend(p) => handle_chat_send(p, state, out_tx, client_kind).await,
```

`handle_chat_send` 签名（当前第 146-150 行）加参数，sink 构造（当前第 154 行）改为：

```rust
async fn handle_chat_send(
    p: &ChatSendParams,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
    client_kind: ClientKind,
) -> Result<MethodOk, ProtoError> {
    // 缺省路由到 main；未知 id 由 registry 懒创建（隐式建会话）。
    let session = p.session.clone().unwrap_or_else(SessionId::main);
    let handle = state.registry().get_or_spawn(&session);
    // Detached 网关（Web/HTTP）：run 归属会话而非连接，断连不中止；
    // Interactive 客户端：本轮内联事件定向回发到这条连接（背压不丢，P0-1）。
    let sink = match client_kind {
        ClientKind::Detached => RunSink::Detached,
        ClientKind::Interactive => RunSink::Conn(out_tx.clone()),
    };
    let mut result = handle.submit(p.text.clone(), sink.clone()).await;
```

（后续 `if result.is_none() && handle.is_closed()` 的重试分支不动，仍用同一个 `sink`。）

- [ ] **Step 2: 改 `conn.rs` 捕获并传递 kind**

`crates/oc-server/src/conn.rs` 的 `read_loop`（当前第 79-107 行）：

```rust
async fn read_loop<R>(
    reader: &mut FrameReader<R>,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
) -> ServerResult<()>
where
    R: AsyncReadExt + Unpin,
{
    // 连接级客户端类型：Connect 握手帧确定，此后本连接所有 chat.send 沿用。
    // 缺省 Interactive——旧客户端或漏发字段时保持原有断连语义。
    let mut client_kind = oc_proto::ClientKind::Interactive;
    while let Some(frame) = reader.read_frame().await? {
        match frame {
            Frame::Req(req) => {
                if let Method::Connect(p) = &req.method {
                    client_kind = p.client_kind;
                }
                let result =
                    dispatch::handle_req(&req, state, out_tx, client_kind).await;
                let res = Res {
                    id: req.id,
                    result,
                };
                if out_tx.send(Frame::Res(res)).await.is_err() {
                    break;
                }
            }
            Frame::Res(_) | Frame::Event(_) => {
                debug!("忽略来自 client 的非请求帧");
            }
        }
    }
    Ok(())
}
```

注意：`Method` 需在 `conn.rs` 导入（当前文件只 `use oc_proto::{Frame, Res}`，第 12 行补 `Method`）。

- [ ] **Step 3: 改四处握手字面量**

`crates/oc-http/src/proto.rs`（导入第 3 行加 `ClientKind`；第 10 行 handshake 改）：

```rust
use oc_proto::{ClientKind, ConnectParams, Frame, IdemKey, Method, MethodOk, Req, ReqId, ResResult, PROTO_VERSION};
```
```rust
    send_req(conn, Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Detached }), None).await?;
```

`crates/oc-cli/src/cli_client.rs`（导入第 8 行加 `ClientKind`；第 26 行字面量加字段）：

```rust
            method: Method::Connect(ConnectParams { proto_version: PROTO_VERSION, token: None, client_kind: ClientKind::Interactive }),
```

`crates/oc-tui/src/app.rs`（导入第 11 行 `oc_proto::{}` 加 `ClientKind`；第 77-80 行字面量加字段）：

```rust
            method: Method::Connect(ConnectParams {
                proto_version: PROTO_VERSION,
                token: None,
                client_kind: ClientKind::Interactive,
            }),
```

`crates/oc-server/src/testing.rs`：

导入（第 23-26 行的 `oc_proto::{}`）加 `ClientKind`。把现有 `handshake`（第 431-447 行）拆成两个方法：

```rust
    /// `connect` 握手，断言 hello 成功（默认 Interactive，保持既有测试语义）。
    pub async fn handshake(&mut self) {
        self.handshake_as(ClientKind::Interactive).await;
    }

    /// `connect` 握手，指定客户端类型。Detached 用例用它模拟 Web/HTTP 网关。
    pub async fn handshake_as(&mut self, client_kind: ClientKind) {
        let id = self
            .request(Method::Connect(ConnectParams {
                proto_version: PROTO_VERSION,
                token: None,
                client_kind,
            }))
            .await;
        match self.recv().await {
            Frame::Res(res) => {
                assert_eq!(res.id.as_str(), id.as_str(), "hello 应答 id 应匹配");
                assert!(matches!(res.result, ResResult::Ok(_)), "connect 应成功");
            }
            other => panic!("期望 Res(hello)，得到 {other:?}"),
        }
    }
```

- [ ] **Step 4: 全仓编译 + 既有测试回归**

Run: `cargo test --workspace`
Expected: 编译通过，既有测试全绿（`e2e_disconnect` 的断连收敛用例默认 Interactive，不受影响；`aborted_request_returns_permit`、`abort_stops_a_running_turn` 同样）。

- [ ] **Step 5: Commit**

```bash
git add crates/oc-server/src/conn.rs crates/oc-server/src/dispatch.rs crates/oc-http/src/proto.rs crates/oc-cli/src/cli_client.rs crates/oc-tui/src/app.rs crates/oc-server/src/testing.rs
git commit -m "feat(server): Detached 网关的 run 断连不中止（sink 选择接 client_kind）"
```

---

### Task 4: 端到端回归——Detached 断连后 run 继续跑完并落库

**Files:**
- Create: `crates/oc-server/tests/detached_disconnect.rs`

**Interfaces:**
- Consumes: `oc_proto::ClientKind`（Task 1）、`TestDaemon::client_no_handshake` + `TestClient::handshake_as`（Task 3）、`oc_store::Store::open_memory` + `writer().load_transcript`。
- Produces: 无（纯测试）。

- [ ] **Step 1: 写测试**

```rust
//! Detached 客户端断连回归：Web/HTTP 网关发起的 run 不随连接断开而中止。
//!
//! 真机缺陷：Web 端发出消息 → 收到 thinking 后立即刷新页面 → run 被断连
//! 掐死（日志 `等模型期间客户端断开，收敛 run`），回复既不再继续，也未落库。
//!
//! 根因：`chat.send` 内联事件经 `RunSink::Conn` 定向回发到发起连接；断连 →
//! 出站队列关闭 → `sink.closed()` → run Aborted（Aborted 不落库）。
//!
//! 修复：Web/HTTP 网关连接标记 `ClientKind::Detached`，其 run 用
//! `RunSink::Detached`（send 恒成功、closed 永不完成），run 归属会话而非连接。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::ClientKind;
use oc_server::testing::{SessionConfigExt, TestDaemon};

/// 持续吐字：每 100ms 一段共 10 段。断连发生在中途，剩余段证明 run 仍在推进。
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
async fn detached_client_disconnect_does_not_abort_run_and_persists() {
    let store = oc_store::Store::open_memory().expect("store");
    let daemon = TestDaemon::builder(
        "detached-disc",
        Arc::new(MockProvider::scripted(streaming_reply(
            10,
            Duration::from_millis(100),
        ))),
    )
    .store(store.clone())
    // idle_timeout 给足，确保「跑完并落库」只可能来自 Detached 语义，而非看门狗。
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    // Detached 连接（模拟 Web/HTTP 网关）。
    let mut client = daemon.client_no_handshake().await;
    client.handshake_as(ClientKind::Detached).await;
    client.chat("讲个故事", None).await;

    // 收到首个事件（证明 run 已起步、连接存活）后立即断连。
    let first = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("应在断连前收到首个事件");
    assert!(matches!(first, oc_proto::Frame::Event(_)), "首个帧应为事件");
    drop(client);

    // run 应在断连后继续跑完并落库 assistant。轮询 transcript 直至出现 assistant。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut assistant: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        let hist = store
            .writer()
            .load_transcript("main".into(), 100)
            .await
            .expect("load");
        assistant = hist
            .iter()
            .find(|e| e.role == oc_store::Role::Assistant)
            .map(|e| e.content.clone());
        if assistant.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = assistant.expect("断连后 run 应跑完并落库 assistant 回复");
    assert!(
        text.contains("第9段"),
        "应含完整回复（断连后仍继续产出），实际: {text}"
    );
}
```

- [ ] **Step 2: 运行测试确认通过**

Run: `cargo test -p oc-server --test detached_disconnect`
Expected: PASS。反向验证（可选，验证用例有效性）：临时把 `dispatch.rs` 里 `ClientKind::Detached => RunSink::Detached` 改回 `RunSink::Conn(out_tx.clone())`，此测试应转红——证明它真在测断连语义，而非走过场。

- [ ] **Step 3: 全仓回归 + 既有断连语义复核**

Run: `cargo test --workspace`
Expected: 全绿。特别关注 `e2e_disconnect`（Interactive 断连仍收敛）与 `detached_disconnect`（Detached 断连不收敛）**两条并存且都绿**——这正是「按客户端类型分流」的正确性证据。

- [ ] **Step 4: Commit**

```bash
git add crates/oc-server/tests/detached_disconnect.rs
git commit -m "test(server): Detached 网关断连后 run 继续跑完并落库（端到端）"
```

---

## Self-Review 记录

- **Spec 覆盖**：根因（`RunSink::Conn` 断连→`sink.closed()`→Abort 不落库）由 Task 3 改 sink 选择 + Task 2 的 `Detached` 语义修复；向后兼容由 Task 1 的 `#[serde(default)]` + 单测保证；「不继续回复 + 不落库」两症状由 Task 4 端到端断言（跑完 + assistant 落库）覆盖。
- **占位符扫描**：无 TBD/TODO；每个代码步骤含可落地的具体代码。
- **类型一致性**：`ClientKind`（proto，Task 1）→ `handle_req`/`handle_chat_send` 参数与 `RunSink::Detached`（Task 3）→ `handshake_as`/`client_no_handshake`（Task 3 产物，Task 4 消费）命名全链一致。
- **已知留白（本轮非目标，需用户确认再做）**：
  1. 刷新后「实时接续在途 run 的剩余事件流」——OpenClaw 的 `resolveInFlightRunSnapshot` + delivery-key 重挂。本计划只做到「run 继续跑完 + 落库」，用户需再刷一次历史才能看到完整回复。
  2. `Detached` 轮若调 `exec`/`ask_user` 触发审批/提问，无人应答时依赖既有的超时 fail-closed（`tools.approval.timeout_secs`、ask_user 600s）。是否对 Detached 轮强制关工具留待后续。
