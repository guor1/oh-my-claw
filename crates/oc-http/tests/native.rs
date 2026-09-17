//! 原生 `/api/v1` 端到端：真 axum + 真 `ConnPool` + 真 daemon。
//!
//! 与 `gateway.rs` 同源（真传输 + 真 store + 脚本化模型），验的是原生 API 的契约：
//! REST 方法映射、SSE 事件名与终止、会话隔离。
//!
//! # 为什么 chat/send 必须自己流回复
//!
//! oc-server 把 run 的内联事件（Assistant / Lifecycle / Tool / Approval）经
//! `RunSink::Conn` 定向到**发起 chat.send 的那条连接**，不进广播（见 oc-server
//! `sink.rs`）。所以另开一条 `GET /events` 收不到 delta —— 这不是实现选择，
//! 是协议决定的。`ambient_stream_excludes_inline_run_events` 就是这条的回归。

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::tool::ApprovalMode;
use oc_llm::mock::{MockProvider, ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_server::testing::TestDaemon;
use oc_server::tools_bridge::ToolExecutor;
use oc_server::TransportKind;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;

async fn spawn_gateway(transport: TransportKind, max_conns: usize) -> String {
    let pool = oc_http::conn_pool::ConnPool::new(transport, max_conns, max_conns);
    let app = oc_http::create_app(
        pool,
        "mock".into(),
        oc_http::AppConfig { token: None, web_ui: false },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑端口");
    let addr = listener.local_addr().expect("取端口");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// 读一条 SSE 流到终止，返回原始文本。
async fn read_sse(resp: reqwest::Response, max_wait: Duration) -> String {
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    let deadline = tokio::time::Instant::now() + max_wait;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // lifecycle end/error 是终止事件。
                if buf.contains(r#""phase":"end""#) || buf.contains(r#""phase":"error""#) {
                    break;
                }
            }
            _ => break,
        }
    }
    buf
}

/// 解析 SSE 文本里出现过的 `event:` 名，按顺序。
fn event_names(sse: &str) -> Vec<String> {
    sse.lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .map(|s| s.trim().to_string())
        .collect()
}

/// 持续吐字：每 `gap` 一段共 `n` 段，末段 Done(Stop)。断连发生在中途时，
/// 剩余段证明 run 仍在推进、且 resume 能接续回来。
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

/// 免审批 exec：`ApprovalMode::Allow` 让工具轮不卡在审批门上。
/// 与 oc-server 的 `resume_run.rs` / `concurrent_submit.rs` 同款。
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

/// chat/send 应流式返回，首事件给 run_id，末事件是 lifecycle end。
#[tokio::test]
async fn chat_send_streams_accepted_then_deltas_then_end() {
    let script = vec![
        ScriptStep { delay: Duration::from_millis(30), delta: Delta::Text("第一段".into()) },
        ScriptStep { delay: Duration::from_millis(30), delta: Delta::Text("第二段".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let daemon = TestDaemon::start("nat-send", Arc::new(MockProvider::scripted(script))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-a", "text": "hi" }))
        .send()
        .await
        .expect("应有应答");
    assert_eq!(resp.status(), 200);

    let body = read_sse(resp, Duration::from_secs(20)).await;
    let names = event_names(&body);

    assert_eq!(names.first().map(String::as_str), Some("accepted"), "首事件应为 accepted：{body}");
    assert!(body.contains("run_id"), "accepted 应带 run_id：{body}");
    assert!(names.iter().any(|n| n == "assistant"), "应有 assistant 事件：{body}");
    assert!(body.contains("第一段") && body.contains("第二段"), "两段文本都应送达：{body}");
    assert_eq!(names.last().map(String::as_str), Some("lifecycle"), "末事件应为 lifecycle：{body}");
    assert!(body.contains(r#""phase":"end""#), "应以 end 收尾：{body}");

    // 每个 data 值必须是合法 JSON（单层封帧）。
    for line in body.lines().filter(|l| l.starts_with("data: ")) {
        let v = line.strip_prefix("data: ").unwrap();
        serde_json::from_str::<serde_json::Value>(v)
            .unwrap_or_else(|e| panic!("data 应是合法 JSON（{e}）：{line}"));
    }
}

/// sessions / history / status 的 REST 映射。
#[tokio::test]
async fn rest_endpoints_map_to_protocol() {
    let daemon = TestDaemon::start("nat-rest", Arc::new(MockProvider::echo_text("你好"))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    // 先发一轮，好让会话与 transcript 有内容。
    let resp = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-r", "text": "问题" }))
        .send()
        .await
        .expect("send 应有应答");
    read_sse(resp, Duration::from_secs(20)).await;

    // sessions 应含刚用过的会话。
    let sessions: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions"))
        .send()
        .await
        .expect("sessions 应有应答")
        .json()
        .await
        .expect("应是 JSON");
    let ids: Vec<&str> = sessions
        .as_array()
        .expect("应是数组")
        .iter()
        .filter_map(|s| s.get("id").and_then(|i| i.as_str()))
        .collect();
    assert!(ids.contains(&"nat-r"), "应含 nat-r 会话：{ids:?}");

    // history 应含用户那句与模型回复。
    let history: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/nat-r/history?limit=50"))
        .send()
        .await
        .expect("history 应有应答")
        .json()
        .await
        .expect("应是 JSON");
    let entries = history.as_array().expect("应是数组");
    assert!(!entries.is_empty(), "history 不应为空");
    let all_text: String = entries
        .iter()
        .filter_map(|e| e.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("|");
    assert!(all_text.contains("问题"), "history 应含用户输入：{all_text}");

    // status 应返回 Snapshot（含 model / context_window）。
    let status: serde_json::Value = client
        .get(format!("{base}/api/v1/status"))
        .send()
        .await
        .expect("status 应有应答")
        .json()
        .await
        .expect("应是 JSON");
    assert!(status.get("context_window").is_some(), "Snapshot 应含 context_window：{status}");
    assert!(status.get("session").is_some(), "Snapshot 应含 session：{status}");

    // reset / compact 应 200。
    for path in ["reset", "compact"] {
        let code = client
            .post(format!("{base}/api/v1/sessions/nat-r/{path}"))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .expect("应有应答")
            .status()
            .as_u16();
        assert_eq!(code, 200, "{path} 应成功");
    }
}

/// 空 text 与保留前缀 session 应 400（而非 500 或静默接受）。
#[tokio::test]
async fn send_validates_input() {
    let daemon = TestDaemon::start("nat-valid", Arc::new(MockProvider::echo_text("ok"))).await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    let post = |body: serde_json::Value| {
        let client = client.clone();
        let url = format!("{base}/api/v1/chat/send");
        async move {
            client
                .post(&url)
                .json(&body)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .expect("应有应答")
                .status()
                .as_u16()
        }
    };

    assert_eq!(post(serde_json::json!({ "text": "   " })).await, 400, "空 text 应被拒");
    assert_eq!(
        post(serde_json::json!({ "session": "cron:x", "text": "hi" })).await,
        400,
        "保留前缀应被拒"
    );
}

/// abort 应能打断正在跑的轮，且车道真的释放。
#[tokio::test]
async fn abort_stops_a_running_turn() {
    let daemon = TestDaemon::start(
        "nat-abort",
        // 占道 30s：给足时间在跑着的时候 abort。
        Arc::new(MockProvider::scripted(vec![
            ScriptStep { delay: Duration::from_secs(30), delta: Delta::Text("ok".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ])),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    // 流式发起，从首事件取 run_id。
    let mut stream = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-ab", "text": "hi" }))
        .send()
        .await
        .expect("send 应建立")
        .bytes_stream();

    let mut buf = String::new();
    let mut run_id = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while run_id.is_none() && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                for line in buf.lines() {
                    let Some(p) = line.strip_prefix("data: ") else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(p.trim()) else { continue };
                    if let Some(id) = v.get("run_id").and_then(|i| i.as_str()) {
                        run_id = Some(id.to_string());
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    let run_id = run_id.unwrap_or_else(|| panic!("未能取到 run_id：{buf}"));

    let code = client
        .post(format!("{base}/api/v1/chat/abort"))
        .json(&serde_json::json!({ "run_id": run_id, "hard": true }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("abort 应有应答")
        .status()
        .as_u16();
    assert_eq!(code, 200, "abort 应成功");

    // 车道应释放——不验这步，一个什么都不做的端点也能过上面的断言。
    let mut released = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let mut probe = daemon.client().await;
        let diag = probe.diagnostics().await;
        let busy = diag
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == "nat-ab")
            .map(|s| s.active.is_some())
            .unwrap_or(false);
        if !busy {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(released, "abort 后 run 应停止占道（模型脚本要跑 30s，不可能自然结束）");
}

/// `GET /events` 只带 ambient 事件，**不含** run 的内联 delta。
///
/// 这条是架构约束的回归：若哪天有人把 `Assistant` 挪回广播、或让 /events 放行
/// 内联事件，Web UI 就会出现「同一段回复显示两遍」（POST 流一份 + /events 一份）。
#[tokio::test]
async fn ambient_stream_excludes_inline_run_events() {
    let daemon = TestDaemon::start(
        "nat-amb",
        Arc::new(MockProvider::scripted(vec![
            ScriptStep { delay: Duration::from_millis(50), delta: Delta::Reasoning("内联思考".into()) },
            ScriptStep { delay: Duration::from_millis(50), delta: Delta::Text("内联文本".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ])),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 8).await;
    let client = reqwest::Client::new();

    // 先开 ambient 流。
    let ambient = client
        .get(format!("{base}/api/v1/events"))
        .send()
        .await
        .expect("events 应建立");
    assert_eq!(ambient.status(), 200);
    let mut ambient_stream = ambient.bytes_stream();

    // 首事件应是 status 快照。
    let mut ambient_buf = String::new();
    if let Ok(Some(Ok(chunk))) = tokio::time::timeout(Duration::from_secs(5), ambient_stream.next()).await {
        ambient_buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(
        ambient_buf.contains("event: status"),
        "events 首事件应为 status 快照：{ambient_buf}"
    );

    // 另一条连接发一轮。
    let resp = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-amb-s", "text": "hi" }))
        .send()
        .await
        .expect("send 应有应答");
    let post_body = read_sse(resp, Duration::from_secs(20)).await;
    assert!(post_body.contains("内联文本"), "POST 流应含回复文本：{post_body}");

    // 再读 ambient 流一小会儿：应能收到 usage，但绝不该有 assistant delta。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(800), ambient_stream.next()).await {
            Ok(Some(Ok(chunk))) => ambient_buf.push_str(&String::from_utf8_lossy(&chunk)),
            _ => break,
        }
    }

    assert!(
        !ambient_buf.contains("event: assistant"),
        "ambient 流不该出现 assistant 内联事件（会导致回复显示两遍）：{ambient_buf}"
    );
    assert!(
        !ambient_buf.contains("内联文本"),
        "ambient 流不该出现回复正文：{ambient_buf}"
    );
    assert!(
        !ambient_buf.contains("reasoning"),
        "reasoning 是内联事件，绝不该出现在 ambient 流：{ambient_buf}"
    );
    assert!(
        !ambient_buf.contains("内联思考"),
        "ambient 流不该出现 reasoning 正文：{ambient_buf}"
    );
}

/// 两个会话并发发送，各自的流不串台。
#[tokio::test]
async fn concurrent_sessions_do_not_cross_talk() {
    let daemon = TestDaemon::start(
        "nat-cross",
        Arc::new(MockProvider::scripted(vec![
            ScriptStep { delay: Duration::from_millis(200), delta: Delta::Text("回复".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ])),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 8).await;

    let mut tasks = Vec::new();
    for i in 0..3 {
        let base = base.clone();
        tasks.push(tokio::spawn(async move {
            let resp = reqwest::Client::new()
                .post(format!("{base}/api/v1/chat/send"))
                .json(&serde_json::json!({ "session": format!("cross-{i}"), "text": format!("q{i}") }))
                .send()
                .await
                .expect("应有应答");
            let body = read_sse(resp, Duration::from_secs(25)).await;
            (i, body)
        }));
    }

    for t in tasks {
        let (i, body) = t.await.expect("任务不应 panic");
        assert!(body.contains(r#""phase":"end""#), "会话 {i} 应正常收尾：{body}");
        // 每条流只应含自己会话的事件。
        let own = format!("cross-{i}");
        for other in 0..3 {
            if other == i {
                continue;
            }
            let foreign = format!("cross-{other}");
            assert!(
                !body.contains(&foreign),
                "会话 {own} 的流混入了 {foreign} 的事件：{body}"
            );
        }
    }
}

/// history 应透传工具结构：assistant 条目带 `tool_calls`，tool 条目带 `tool_call_id`。
///
/// 回归「历史接口丢工具字段」——Web UI 靠这些字段把「空气泡 + 乱码气泡」重建成
/// 结构化工具卡片。这里用脚本化模型（首轮请求 exec，次轮收尾）真跑一轮工具调用。
#[tokio::test]
async fn history_exposes_tool_call_structure() {
    use oc_core::tool::ApprovalMode;
    use oc_llm::mock::SequencedMock;
    use oc_llm::types::ToolCallDelta;
    use oc_server::testing::SessionConfigExt;
    use oc_server::tools_bridge::ToolExecutor;
    use oc_tools::exec::ExecTool;
    use oc_tools::shell::Shell;
    use oc_tools::ToolRegistry;

    let scripts = vec![
        vec![
            ScriptStep {
                delay: Duration::ZERO,
                delta: Delta::ToolCall(ToolCallDelta {
                    call_id: "call-hist".into(),
                    name: Some("exec".into()),
                    args_chunk: r#"{"command": "echo hi"}"#.into(),
                }),
            },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
        ],
        vec![
            ScriptStep { delay: Duration::ZERO, delta: Delta::Text("跑完了".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ],
    ];

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(
        ApprovalMode::Allow,
        Duration::from_secs(10),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));

    let daemon = TestDaemon::builder("nat-toolstruct", Arc::new(SequencedMock::new(scripts)))
        .map_cfg(|c| c.with_tools(ToolExecutor::new(Arc::new(reg))))
        .start()
        .await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-ts", "text": "跑个命令" }))
        .send()
        .await
        .expect("send 应有应答");
    read_sse(resp, Duration::from_secs(20)).await;

    let history: serde_json::Value = client
        .get(format!("{base}/api/v1/sessions/nat-ts/history?limit=50"))
        .send()
        .await
        .expect("history 应有应答")
        .json()
        .await
        .expect("应是 JSON");
    let entries = history.as_array().expect("应是数组");

    let dispatch = entries
        .iter()
        .find(|e| e.get("role").and_then(|r| r.as_str()) == Some("assistant") && e.get("tool_calls").is_some())
        .unwrap_or_else(|| panic!("assistant 应带 tool_calls：{history}"));
    let tool_calls = dispatch.get("tool_calls").and_then(|t| t.as_array()).expect("tool_calls 应是数组");
    assert_eq!(tool_calls[0].get("name").and_then(|n| n.as_str()), Some("exec"));
    assert_eq!(tool_calls[0].get("id").and_then(|n| n.as_str()), Some("call-hist"));
    assert!(tool_calls[0].get("args").and_then(|a| a.as_str()).unwrap_or("").contains("echo hi"));

    let result = entries
        .iter()
        .find(|e| e.get("role").and_then(|r| r.as_str()) == Some("tool") && e.get("tool_call_id").is_some())
        .unwrap_or_else(|| panic!("tool 条目应带 tool_call_id：{history}"));
    assert_eq!(result.get("tool_call_id").and_then(|n| n.as_str()), Some("call-hist"));
}

/// reasoning 应作为内联事件出现在 chat/send 的 SSE 流上，事件名是 `reasoning`。
#[tokio::test]
async fn chat_send_streams_reasoning_events() {
    use oc_llm::mock::{MockProvider, ScriptStep};
    let daemon = TestDaemon::start(
        "nat-reas",
        Arc::new(MockProvider::scripted(vec![
            ScriptStep { delay: Duration::from_millis(20), delta: Delta::Reasoning("先想想".into()) },
            ScriptStep { delay: Duration::from_millis(20), delta: Delta::Text("正文".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ])),
    )
    .await;
    let base = spawn_gateway(daemon.transport(), 8).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "nat-reas-s", "text": "hi" }))
        .send()
        .await
        .expect("send 应有应答");
    let body = read_sse(resp, Duration::from_secs(20)).await;

    let names = event_names(&body);
    assert!(names.iter().any(|n| n == "reasoning"), "SSE 应含 reasoning 事件，实际：{names:?}");
    assert!(body.contains("先想想"), "reasoning 正文应透传，实际：{body}");
    assert!(body.contains(r#""event":"reasoning""#), "data 里应带 event 标签：{body}");
}

/// Detached run 断连后，GET /api/v1/chat/resume 拉回剩余流到终态。
///
/// 关键回归（Fix 1）：断连前必须消费一个**非空**前缀，使 RunLog 回放缓冲非空。
/// resume 得把「断连前已 stream 的前缀（回放）+ 断连后的剩余段（续流）」都吐出来。
/// 旧实现直接 await_res 丢掉交错事件，导致前缀在 resume 流里消失。
#[tokio::test]
async fn resume_endpoint_streams_remainder() {
    use oc_server::testing::SessionConfigExt;

    let daemon = TestDaemon::builder(
        "nat-resume",
        Arc::new(MockProvider::scripted(streaming_reply(
            8,
            Duration::from_millis(150),
        ))),
    )
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;
    let base = spawn_gateway(daemon.transport(), 4).await;
    let client = reqwest::Client::new();

    // 发消息（经 /api/v1/chat/send），边消费边收集：run_id + 若干 assistant 前缀段。
    let mut send_stream = client
        .post(format!("{base}/api/v1/chat/send"))
        .json(&serde_json::json!({ "session": "main", "text": "讲个故事" }))
        .send()
        .await
        .expect("send 应建立")
        .bytes_stream();

    let mut linebuf = String::new();
    let mut run_id = None;
    let mut prefix: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), send_stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                linebuf.push_str(&String::from_utf8_lossy(&chunk));
                // 按完整行消费，跨 chunk 的半行留在 linebuf。
                while let Some(pos) = linebuf.find('\n') {
                    let line: String = linebuf.drain(..=pos).collect();
                    let line = line.trim_end();
                    let Some(p) = line.strip_prefix("data: ") else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(p.trim()) else { continue };
                    if let Some(id) = v.get("run_id").and_then(|i| i.as_str()) {
                        run_id = Some(id.to_string());
                    }
                    if v.get("event").and_then(|e| e.as_str()) == Some("assistant") {
                        if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                            prefix.push(d.to_string());
                        }
                    }
                }
                if run_id.is_some() && prefix.len() >= 2 {
                    break;
                }
            }
            _ => break,
        }
    }
    let run_id = run_id.unwrap_or_else(|| panic!("未能取到 run_id：{linebuf}"));
    assert!(
        prefix.len() >= 2,
        "断连前应已 stream 至少 2 段（使回放缓冲非空），实际 {prefix:?}：{linebuf}"
    );

    // 断连（不消费完 send 的流）——run 仍在推进（idle_timeout 拉到 120s）。
    drop(send_stream);

    // resume 拉剩余流。
    let resp = client
        .get(format!("{base}/api/v1/chat/resume?run_id={run_id}&session=main"))
        .send()
        .await
        .expect("resume 应建立");
    assert_eq!(resp.status(), 200);
    let sse = read_sse(resp, Duration::from_secs(10)).await;
    assert!(sse.contains(r#""phase":"end""#), "resume 流应以 lifecycle end 终止：{sse}");
    // 回放 ∪ 续流 = 全量 8 段。
    for i in 0..8 {
        assert!(sse.contains(&format!("第{i}段。")), "缺少第{i}段：{sse}");
    }
    // Fix 1 的负载断言：断连前已 stream 的前缀必须出现在 resume 流里。
    for d in &prefix {
        assert!(sse.contains(d), "回放前缀「{d}」未出现在 resume 流：{sse}");
    }
}

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
