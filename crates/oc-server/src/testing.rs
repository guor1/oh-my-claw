//! 测试 harness（feature `test-support`）。
//!
//! 存在理由有两个，都和 Rust 的测试组织方式有关：
//!
//! 1. `tests/*.rs` 每个文件是独立 crate，彼此无法共享代码。本仓 18 个集成测试
//!    因此把 20 字段的 [`SessionConfig`] 字面量逐字重复了 36 次——每加一个字段
//!    要改 20 处，新测试写不动。
//! 2. `oc-http` 的端到端测试也要起真 daemon（真传输 + 真 store），跨 crate 复用
//!    只能走 feature 而非 `#[cfg(test)]`。
//!
//! 提供三样东西：
//! - [`test_cfg()`]：20 字段的合理默认，配 builder 覆盖常改项。
//! - [`TestDaemon`]：经**真实传输**（unix socket / 命名管道）起一个 daemon。
//! - [`TestClient`]：完成握手的客户端，会收发 NDJSON 帧、能等特定事件。
//!
//! 传输端点命名策略（`\\.\pipe\oc-test-<pid>-<tag>`）沿用 `roundtrip.rs` 的既有
//! 做法：`first_pipe_instance(true)` 下第二个 server 绑同名管道会失败，故同进程内
//! 并发跑的测试必须各用唯一 tag。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::provider::Provider;
use oc_proto::{
    ChatSendParams, ClientKind, ConnectParams, Event, Frame, LifecyclePhase, Method, Req, ReqId,
    ResResult, SessionId, PROTO_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::session::SessionConfig;
use crate::TransportKind;

/// 会话配置的测试基线。
///
/// 取值来自迁移前 18 个测试文件的公共交集：纯对话（无工具）、5s 空闲超时、
/// 队列 8、看门狗阈值宽松到不会误触发。需要改某一项时用下面的 builder，
/// 或直接改字段（都是 pub）。
pub fn test_cfg() -> SessionConfig {
    SessionConfig {
        model: "mock".into(),
        system_prompt: None,
        idle_timeout: Duration::from_secs(5),
        run_timeout: None,
        queue_cap: 8,
        tools: None,
        warn_secs: 60,
        abort_min_secs: 300,
        max_history_entries: 200,
        history_token_budget: 8000,
        auto_compact: false,
        soul: String::new(),
        skills: Vec::new(),
        trigger_threshold: 0.72,
        trigger_max_per_turn: 3,
        intent_defaults: Default::default(),
        soul_dir: None,
        default_tz: "UTC".into(),
        context_window: 65536,
        max_output_tokens: None,
    }
}

/// [`SessionConfig`] 的链式覆盖。
///
/// 只覆盖了各测试实际改过的字段；其余直接改 struct 字段即可。
pub trait SessionConfigExt: Sized {
    fn with_soul(self, soul: impl Into<String>) -> Self;
    fn with_idle_timeout(self, d: Duration) -> Self;
    fn with_queue_cap(self, cap: usize) -> Self;
    /// 卡死诊断阈值：`warn_secs` 警告、`abort_min_secs` 达标才释放车道。
    fn with_watchdog(self, warn_secs: u64, abort_min_secs: u64) -> Self;
    fn with_trigger_threshold(self, t: f64) -> Self;
    fn with_history(self, max_entries: i64, token_budget: i64) -> Self;
    fn with_soul_dir(self, dir: impl Into<std::path::PathBuf>) -> Self;
    /// 挂工具执行器（默认 `None` = 纯对话）。
    fn with_tools(self, tools: crate::tools_bridge::ToolExecutor) -> Self;
    fn with_context_window(self, window: u32) -> Self;
    fn with_auto_compact(self, on: bool) -> Self;
}

impl SessionConfigExt for SessionConfig {
    fn with_soul(mut self, soul: impl Into<String>) -> Self {
        self.soul = soul.into();
        self
    }
    fn with_idle_timeout(mut self, d: Duration) -> Self {
        self.idle_timeout = d;
        self
    }
    fn with_queue_cap(mut self, cap: usize) -> Self {
        self.queue_cap = cap;
        self
    }
    fn with_watchdog(mut self, warn_secs: u64, abort_min_secs: u64) -> Self {
        self.warn_secs = warn_secs;
        self.abort_min_secs = abort_min_secs;
        self
    }
    fn with_trigger_threshold(mut self, t: f64) -> Self {
        self.trigger_threshold = t;
        self
    }
    fn with_history(mut self, max_entries: i64, token_budget: i64) -> Self {
        self.max_history_entries = max_entries;
        self.history_token_budget = token_budget;
        self
    }
    fn with_soul_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.soul_dir = Some(dir.into());
        self
    }
    fn with_tools(mut self, tools: crate::tools_bridge::ToolExecutor) -> Self {
        self.tools = Some(tools);
        self
    }
    fn with_context_window(mut self, window: u32) -> Self {
        self.context_window = window;
        self
    }
    fn with_auto_compact(mut self, on: bool) -> Self {
        self.auto_compact = on;
        self
    }
}

/// 起一个进程内 [`ServerState`](crate::ServerState) + 其会话注册表，不经传输层。
///
/// 给「只测 state/registry 语义」的用例用（如 P2-3 的内存 GC）：`ServerState::new`
/// 有 9 个参数、其中 4 个是只为它存在的空注册表，逐个测试重复构造既啰嗦又容易在
/// 加字段时集体失修。返回 state 与 registry 两个句柄（registry 也在 state 里，
/// 但测试常要直接 `get_or_spawn`）。
pub fn test_state(
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    store: oc_store::Store,
) -> (Arc<crate::ServerState>, crate::registry::SessionRegistry, tokio::sync::broadcast::Receiver<Event>) {
    let (event_tx, event_rx) = tokio::sync::broadcast::channel(512);
    let diag = crate::diag::DiagRegistry::new();
    let runtime = crate::state::RuntimeInfo {
        provider: provider.id().to_string(),
        model: cfg.model.clone(),
        endpoint: provider.endpoint().map(str::to_string),
        context_window: cfg.context_window,
    };
    let intent_defaults = cfg.intent_defaults.clone();
    let registry = crate::registry::SessionRegistry::new(
        cfg,
        provider,
        event_tx.clone(),
        store.clone(),
        diag.clone(),
    );
    let state = Arc::new(crate::ServerState::new(
        event_tx.clone(),
        registry.clone(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
        crate::ledger::TaskLedger::new(event_tx),
        store,
        runtime,
        diag,
        intent_defaults,
    ));
    (state, registry, event_rx)
}

/// 本测试专用的唯一传输端点。
///
/// `tag` 区分同进程内并发运行的测试。Windows 命名管道在
/// `first_pipe_instance(true)` 下同名二次绑定会失败，故 tag 必须唯一。
pub fn test_transport(tag: &str) -> TransportKind {
    #[cfg(windows)]
    {
        TransportKind::Pipe(format!(r"\\.\pipe\oc-test-{}-{tag}", std::process::id()))
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::temp_dir().join(format!("oc-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        TransportKind::Unix(dir.join("oc.sock"))
    }
}

/// 经真实传输运行的 daemon。
///
/// `drop` 时 abort server 任务并清理端点，测试无需手动收尾。
pub struct TestDaemon {
    kind: TransportKind,
    handle: tokio::task::JoinHandle<()>,
}

/// [`TestDaemon`] 的构造器。
pub struct TestDaemonBuilder {
    tag: String,
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    heartbeat: Duration,
    store: Option<oc_store::Store>,
}

impl TestDaemonBuilder {
    /// 覆盖会话配置。
    pub fn cfg(mut self, cfg: SessionConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// 改会话配置的个别字段。
    pub fn map_cfg(mut self, f: impl FnOnce(SessionConfig) -> SessionConfig) -> Self {
        self.cfg = f(self.cfg);
        self
    }

    /// 心跳间隔。默认 60s（长到不会在测试期间自发触发主动性/dreaming）。
    /// 验证 cron / 主动性时调小。
    pub fn heartbeat(mut self, d: Duration) -> Self {
        self.heartbeat = d;
        self
    }

    /// 用指定 store（默认内存库）。需要跨重启持久化时传 `open_path`。
    pub fn store(mut self, store: oc_store::Store) -> Self {
        self.store = Some(store);
        self
    }

    /// 起 daemon 并等 listener 就绪。
    pub async fn start(self) -> TestDaemon {
        let kind = test_transport(&self.tag);
        let store = self
            .store
            .unwrap_or_else(|| oc_store::Store::open_memory().expect("开内存库"));

        let server_kind = kind.clone();
        let handle = tokio::spawn(async move {
            let _ = crate::serve_with(server_kind, self.provider, self.cfg, self.heartbeat, store)
                .await;
        });

        TestDaemon { kind, handle }.await_ready().await
    }
}

impl TestDaemon {
    /// 用给定 provider 起一个 daemon。`tag` 需在同进程内唯一。
    pub fn builder(tag: &str, provider: Arc<dyn Provider>) -> TestDaemonBuilder {
        TestDaemonBuilder {
            tag: tag.to_string(),
            provider,
            cfg: test_cfg(),
            heartbeat: Duration::from_secs(60),
            store: None,
        }
    }

    /// 最常见形态：默认配置 + 内存库。
    pub async fn start(tag: &str, provider: Arc<dyn Provider>) -> Self {
        Self::builder(tag, provider).start().await
    }

    /// 传输端点。给 `oc-http` 建 `ConnPool` 用。
    pub fn transport(&self) -> TransportKind {
        self.kind.clone()
    }

    /// 连一个新客户端并完成 `connect` 握手。
    ///
    /// oc-server 对协议版本不匹配是硬拒绝，故握手必须先于任何其它方法。
    pub async fn client(&self) -> TestClient {
        let mut c = self.client_no_handshake().await;
        c.handshake().await;
        c
    }

    /// 连一个客户端但**不**握手。用于验证握手本身的行为。
    pub async fn client_no_handshake(&self) -> TestClient {
        TestClient::connect(&self.kind).await
    }

    /// 轮询直到端点可连（避免固定 sleep 带来的偶发失败）。
    async fn await_ready(self) -> Self {
        for _ in 0..100 {
            if TestClient::try_connect(&self.kind).await.is_some() {
                return self;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon 未在 2s 内就绪");
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.handle.abort();
        #[cfg(not(windows))]
        if let TransportKind::Unix(path) = &self.kind {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 抽象读写端（unix stream / 命名管道 client 都实现 `AsyncRead + AsyncWrite`）。
trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}

type BoxWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
type BoxRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// 经真实传输连到 daemon 的客户端。
///
/// 收发 NDJSON 帧，并提供"等到某个事件"的高层断言辅助——手册用例里
/// 人眼盯屏幕做的判断，在这里表达为对帧流的断言。
pub struct TestClient {
    w: BoxWrite,
    r: BufReader<BoxRead>,
    seq: u32,
}

impl TestClient {
    async fn try_connect(kind: &TransportKind) -> Option<Box<dyn ClientStream>> {
        match kind {
            #[cfg(unix)]
            TransportKind::Unix(path) => tokio::net::UnixStream::connect(path)
                .await
                .ok()
                .map(|s| Box::new(s) as Box<dyn ClientStream>),
            #[cfg(windows)]
            TransportKind::Pipe(name) => {
                use tokio::net::windows::named_pipe::ClientOptions;
                ClientOptions::new()
                    .open(name.as_str())
                    .ok()
                    .map(|c| Box::new(c) as Box<dyn ClientStream>)
            }
            #[allow(unreachable_patterns)]
            _ => panic!("本平台不支持该传输"),
        }
    }

    async fn connect(kind: &TransportKind) -> Self {
        for _ in 0..100 {
            if let Some(stream) = Self::try_connect(kind).await {
                let (r, w) = tokio::io::split(stream);
                return Self {
                    w: Box::new(w) as BoxWrite,
                    r: BufReader::new(Box::new(r) as BoxRead),
                    seq: 0,
                };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("连不上 daemon");
    }

    /// 发一帧。
    pub async fn send(&mut self, frame: &Frame) {
        let mut s = serde_json::to_string(frame).expect("序列化帧");
        s.push('\n');
        self.w.write_all(s.as_bytes()).await.expect("写帧");
        self.w.flush().await.expect("flush");
    }

    /// 收一帧（跳过空行）。超时即 panic——测试里挂死比误判更该立刻暴露。
    pub async fn recv(&mut self) -> Frame {
        self.recv_within(Duration::from_secs(5)).await
    }

    /// 收一帧；超时返回 `None` 而不 panic。
    ///
    /// 用在"等某个事件出现"的轮询里：那种循环希望在超时后走到自己的断言，
    /// 报出「没等到 X」这样的具体原因，而不是让 harness 抛一句
    /// 「收帧超时」把失败位置指到 testing.rs。
    pub async fn try_recv_within(&mut self, timeout: Duration) -> Option<Frame> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = tokio::time::timeout(timeout, self.r.read_line(&mut line))
                .await
                .ok()?
                .expect("读帧");
            if n == 0 {
                return None; // 对端关闭
            }
            let t = line.trim_end();
            if t.is_empty() {
                continue;
            }
            return Some(
                serde_json::from_str(t).unwrap_or_else(|e| panic!("解析帧失败: {e}\n原文: {t}")),
            );
        }
    }

    /// 收一帧，自定超时。超时即 panic——多数用例里收不到帧就是失败。
    pub async fn recv_within(&mut self, timeout: Duration) -> Frame {
        let mut line = String::new();
        loop {
            line.clear();
            let n = tokio::time::timeout(timeout, self.r.read_line(&mut line))
                .await
                .expect("收帧超时")
                .expect("读帧");
            assert!(n > 0, "对端关闭连接");
            let t = line.trim_end();
            if t.is_empty() {
                continue;
            }
            return serde_json::from_str(t).unwrap_or_else(|e| panic!("解析帧失败: {e}\n原文: {t}"));
        }
    }

    /// 自增请求 id，免得每个测试自己编。
    fn next_id(&mut self) -> ReqId {
        self.seq += 1;
        ReqId::new(format!("t{}", self.seq))
    }

    /// 发一个方法请求，返回其 `ReqId`。
    pub async fn request(&mut self, method: Method) -> ReqId {
        let id = self.next_id();
        self.send(&Frame::Req(Req {
            id: id.clone(),
            method,
            idempotency_key: None,
        }))
        .await;
        id
    }

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

    /// 发一轮对话。`session` 为 `None` 时落默认会话。
    pub async fn chat(&mut self, text: impl Into<String>, session: Option<SessionId>) -> ReqId {
        self.request(Method::ChatSend(ChatSendParams {
            session,
            text: text.into(),
        }))
        .await
    }

    /// 收帧直到本轮 `Lifecycle::End`，聚合成 [`Turn`]。
    ///
    /// 这是把手册用例翻译成断言的主力：人眼"盯着屏幕看回复完不完整"，
    /// 在这里变成对 `turn.text()` 的字符串断言。
    ///
    /// 只认 `run` 匹配的事件，故同会话并发多轮时不会互相捡错——这正是
    /// `Event::Usage` 缺 `run_id` 所以无法归属的那类问题（见 `usage` 字段注释）。
    pub async fn collect_turn(&mut self, timeout: Duration) -> Turn {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut turn = Turn::default();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "等 Lifecycle::End 超时；已收 {turn:?}");

            match self.recv_within(remaining).await {
                Frame::Res(res) => {
                    if turn.run_id.is_none() {
                        if let ResResult::Ok(oc_proto::MethodOk::ChatSend { run_id }) = &res.result {
                            turn.run_id = Some(run_id.clone());
                        }
                    }
                    turn.responses.push(res);
                }
                Frame::Event(ev) => {
                    if let Event::Assistant { delta, .. } = &ev {
                        turn.deltas.push(delta.clone());
                    }
                    // End 与 Error 都是终态：Error 也会结束 run、释放车道。
                    // 只等 End 会在错误路径上白等到超时，把「正确地报错」误判成挂死。
                    let done = matches!(
                        &ev,
                        Event::Lifecycle {
                            phase: LifecyclePhase::End | LifecyclePhase::Error { .. },
                            ..
                        }
                    );
                    turn.events.push(ev);
                    if done {
                        return turn;
                    }
                }
                // server 不会主动向 client 发 Req。
                Frame::Req(req) => panic!("非预期的入站 Req: {req:?}"),
            }
        }
    }

    /// 发一轮并收到终态。最常用的形态。
    pub async fn chat_turn(&mut self, text: impl Into<String>) -> Turn {
        self.chat(text, None).await;
        self.collect_turn(Duration::from_secs(10)).await
    }

    /// 拉一次诊断快照。
    ///
    /// 手册里"跑 `oc debug` 看会话是否回到 idle"这类判断，在这里变成断言。
    pub async fn diagnostics(&mut self) -> oc_proto::DiagnosticsSnapshot {
        let id = self.request(Method::Diagnostics).await;
        loop {
            // 诊断应答可能夹在事件流中间，跳过不相干的帧。
            if let Frame::Res(res) = self.recv().await {
                if res.id.as_str() == id.as_str() {
                    match res.result {
                        ResResult::Ok(oc_proto::MethodOk::Diagnostics(d)) => return d,
                        other => panic!("期望 Diagnostics，得到 {other:?}"),
                    }
                }
            }
        }
    }
}

/// 一轮对话收集到的全部帧。
#[derive(Default, Debug)]
pub struct Turn {
    pub run_id: Option<oc_proto::RunId>,
    /// 按到达顺序的 `Assistant` delta。
    pub deltas: Vec<String>,
    pub events: Vec<Event>,
    pub responses: Vec<oc_proto::Res>,
}

impl Turn {
    /// 拼接后的完整回复文本。
    ///
    /// 长回复截断类缺陷（P0-1）的断言点：拼出来的文本应与预期逐字相等，
    /// 缺一段就是丢帧。
    pub fn text(&self) -> String {
        self.deltas.concat()
    }

    /// 本轮是否以**正常**终态结束。
    pub fn ended_ok(&self) -> bool {
        self.events.iter().any(|e| {
            matches!(
                e,
                Event::Lifecycle {
                    phase: LifecyclePhase::End,
                    ..
                }
            )
        })
    }

    /// 本轮的错误终态（若有）。
    ///
    /// 断言「abort 后 run 以 Aborted 收尾」「超时以 Timeout 收尾」用。
    pub fn error_kind(&self) -> Option<oc_proto::RunErrorKind> {
        self.events.iter().find_map(|e| match e {
            Event::Lifecycle {
                phase: LifecyclePhase::Error { kind, .. },
                ..
            } => Some(*kind),
            _ => None,
        })
    }

    /// 本轮的 `Usage` 事件（若有）。
    pub fn usage(&self) -> Option<(u32, u32)> {
        self.events.iter().find_map(|e| match e {
            Event::Usage {
                input_tokens,
                context_window,
                ..
            } => Some((*input_tokens, *context_window)),
            _ => None,
        })
    }

    /// 协议层错误应答（如队列满被拒）。
    pub fn proto_errors(&self) -> Vec<&oc_proto::ProtoError> {
        self.responses
            .iter()
            .filter_map(|r| match &r.result {
                ResResult::Err(e) => Some(e),
                _ => None,
            })
            .collect()
    }
}


