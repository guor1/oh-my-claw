//! 协议回归：thinking 模型的 reasoning 增量要推成 `Event::Reasoning`，
//! 同时同轮回喂的 `Message.reasoning` 必须保留（DeepSeek 400 回归保护）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self};
use oc_server::testing::{test_cfg, SessionConfigExt};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

fn reasoning_step(r: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Reasoning(r.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("回复".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

/// 带工具调用的轮：reasoning 必须绑在带 tool_calls 的 assistant 消息上回喂。
fn reasoning_tool_step(r: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Reasoning(r.into()) },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "call-1".into(),
                name: Some("exec".into()),
                args_chunk: r#"{"command":"echo hi"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ]
}

fn text_step(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

async fn collect_until_terminal(
    rx: &mut broadcast::Receiver<Event>,
    timeout: Duration,
) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let terminal = matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        );
        out.push(ev);
        if terminal {
            break;
        }
    }
    out
}

#[tokio::test]
async fn reasoning_delta_emits_reasoning_event() {
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(SequencedMock::new(vec![reasoning_step("我需要先确认路径")]));
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        test_cfg(),
        provider,
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("写文件".into(), handle.broadcast_sink()).await.expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    let reasoning: String = evs
        .iter()
        .filter_map(|e| match e {
            Event::Reasoning { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "我需要先确认路径", "reasoning 应逐 delta 透出，事件流：{evs:?}");
}

#[tokio::test]
async fn reasoning_is_carried_back_on_tool_turn() {
    // 工具轮：reasoning 必须绑在带 tool_calls 的 assistant 消息上回喂，
    // 否则 DeepSeek 400。这条锁住 run.rs:264 那段的回归。
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(
        oc_core::tool::ApprovalMode::Allow,
        Duration::from_secs(10),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));
    let tools = ToolExecutor::new(Arc::new(reg));

    let provider = Arc::new(SequencedMock::new(vec![
        reasoning_tool_step("我先跑个命令看看"),
        text_step("跑完了"),
    ]));
    let sid = oc_proto::SessionId::main();
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(512);
    let handle = session::spawn(
        sid.clone(),
        test_cfg().with_tools(tools),
        provider.clone(),
        tx,
        store,
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("跑个命令".into(), handle.broadcast_sink()).await.expect("run");
    collect_until_terminal(&mut rx, Duration::from_secs(10)).await;

    let caps = provider.captures();
    let cap = caps.lock().unwrap();
    // 工具轮 = 两轮请求：第一轮模型请求工具（reasoning 尚未回喂），
    // 第二轮才是「带 tool_calls 的 assistant 消息 + reasoning + 工具结果」的
    // 回喂请求。要锁住的是后者。
    let last = cap.last().expect("至少一轮请求");
    let carried = last
        .messages
        .iter()
        .any(|m| m.role == oc_llm::MsgRole::Assistant
            && !m.tool_calls.is_empty()
            && m.reasoning.as_deref() == Some("我先跑个命令看看"));
    assert!(carried, "工具轮回喂必须带 reasoning，实际消息：{:?}", last.messages);
}
