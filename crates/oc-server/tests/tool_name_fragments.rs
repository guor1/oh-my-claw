//! 回归：工具名不得被后续空分片覆盖。
//!
//! 真机现象（deepseek-v4-flash-0731）：一轮工具调用里 `id` 和 `args` 都完好，
//! 唯独 `name` 是空串，于是落库成哨兵 `unknown_tool`、白白浪费一轮；模型重试时
//! 吐出**字节数完全相同**的参数，第二次却成功——说明模型产出没变，是我们解析侧
//! 把名字弄丢了。
//!
//! 根因：OpenAI 流式协议里 `id`/`name` 只在工具调用的第一个 chunk 出现，后续
//! chunk 只带 `arguments` 分片。但 provider 会在后续分片里把 `function.name`
//! 一并带上且置空，`as_str()` 得到 `Some("")`——累加处 `if let Some(name) = tc.name`
//! 不加判空就赋值，于是**最后一个分片把好名字清成了空串**。
//! 同一处的 `call_id` 早有 `!is_empty()` 保护，`name` 漏了，这就是那个不对称。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session;
use oc_server::testing::{test_cfg, SessionConfigExt};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

async fn wait_terminal(rx: &mut broadcast::Receiver<Event>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        ) {
            return;
        }
    }
}

/// 首个分片带 name，后续 args 分片带 `name:""` —— 名字必须保住。
#[tokio::test]
async fn empty_name_fragment_must_not_clobber_tool_name() {
    // 真机分片形状：第 1 片给 id+name+空 args，第 2/3 片只续 args 但把 name 带成空串。
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "call_real".into(),
                name: Some("web_search".into()),
                args_chunk: String::new(),
            }),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: String::new(),
                name: Some(String::new()), // ← 毒分片
                args_chunk: r#"{"query":"#.into(),
            }),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: String::new(),
                name: Some(String::new()), // ← 毒分片
                args_chunk: r#""珠峰"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ];
    let round2 = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("好的。".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(SequencedMock::new(vec![round1, round2]));

    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    // 空注册表：工具必然「未知」，但我们断言的是**名字**，不是执行结果。
    let tools = ToolExecutor::new(Arc::new(ToolRegistry::new()));
    let (tx, mut rx) = broadcast::channel(512);
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        test_cfg().with_tools(tools),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("搜一下".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let dispatch = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Assistant && e.tool_calls.is_some())
        .expect("应落库一条带 tool_calls 的 assistant");
    let tc = dispatch.tool_calls.as_deref().unwrap();

    assert!(
        tc.contains(r#""name":"web_search""#),
        "首片已给出 web_search，后续空 name 分片不得覆盖它（覆盖后会落成 unknown_tool、白跑一轮）: {tc}"
    );
    // args 仍须完整拼接，修名字不能伤到参数累加。
    assert!(
        tc.contains(r#"{\"query\":\"珠峰\"}"#) || tc.contains(r#"{"query":"珠峰"}"#),
        "参数分片仍须完整拼接: {tc}"
    );
    // call_id 用首片的真值，不退化成兜底 uuid。
    assert!(tc.contains("call_real"), "call_id 应取首片真值: {tc}");
}

/// 真·空名（从头到尾就没给过名字）仍走哨兵——修复不得把兜底一起削掉。
#[tokio::test]
async fn genuinely_absent_name_still_falls_back_to_sentinel() {
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "call_x".into(),
                name: Some(String::new()),
                args_chunk: r#"{"command":"ls"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ];
    let round2 = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("好的。".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(SequencedMock::new(vec![round1, round2]));

    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();
    let tools = ToolExecutor::new(Arc::new(ToolRegistry::new()));
    let (tx, mut rx) = broadcast::channel(512);
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        test_cfg().with_tools(tools),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("跑一下".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let dispatch = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Assistant && e.tool_calls.is_some())
        .expect("应落库一条带 tool_calls 的 assistant");
    let tc = dispatch.tool_calls.as_deref().unwrap();
    assert!(
        tc.contains(r#""name":"unknown_tool""#),
        "从未给过名字时仍须落哨兵（否则回喂 400）: {tc}"
    );
}
