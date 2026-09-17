//! Detached 客户端在 ask_user 等待期间断连：等待必须立即收敛，而非干等 600s。
//!
//! 真机场景：Web 端跑到 ask_user（模型提问等用户回答），用户刷新页面 → 旧连接断、
//! 没人能再答复。此前 `sink.closed()` 对 Detached 被刻意设为永挂起（那是给流式阶段
//! 「断连继续跑」用的），于是 ask_user 的等待也永挂起，只能等 600s 超时兜底。
//!
//! 修复：ask_user 的等待改用 `sink.closed_interactive()`——Detached 也会在断连时
//! 完成；断连后工具返回「未作答」、run 继续跑完落库（而非 cancel 整个 run，后者
//! 是 Conn/TUI 走人的语义）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{ClientKind, Event};
use oc_server::testing::{SessionConfigExt, TestDaemon};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::ask_user::AskUserTool;
use oc_tools::ToolRegistry;

fn tool_call_step(id: &str, name: &str, args: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: id.into(),
                name: Some(name.into()),
                args_chunk: args.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ]
}

fn text_reply(n: usize) -> Vec<ScriptStep> {
    let mut steps: Vec<_> = (0..n)
        .map(|i| ScriptStep {
            delay: Duration::from_millis(40),
            delta: Delta::Text(format!("第{i}段。")),
        })
        .collect();
    steps.push(ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) });
    steps
}

fn executor_with_ask_user() -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(AskUserTool));
    ToolExecutor::new(Arc::new(reg))
}

/// Detached 断连后 ask_user 等待立即收敛、run 继续跑完落库。
#[tokio::test]
async fn detached_ask_user_disconnect_resolves_and_run_completes() {
    let provider = Arc::new(SequencedMock::new(vec![
        tool_call_step("c-ask", "ask_user", r#"{"prompt":"你叫什么名字？"}"#),
        text_reply(3),
    ]));
    let store = oc_store::Store::open_memory().expect("store");
    let daemon = TestDaemon::builder("detached-ask", provider)
        .store(store.clone())
        .map_cfg(|c| {
            c.with_idle_timeout(Duration::from_secs(120))
                .with_tools(executor_with_ask_user())
        })
        .start()
        .await;

    let mut client = daemon.client_no_handshake().await;
    client.handshake_as(ClientKind::Detached).await;
    client.chat("问我名字", None).await;

    // 收到 UserInput 事件（证明 run 已卡在等答复），随后断连。
    let mut saw_input = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), client.recv()).await {
            Ok(frame) => {
                if matches!(frame, oc_proto::Frame::Event(Event::UserInput { .. })) {
                    saw_input = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_input, "应收到 UserInput 请求事件");
    drop(client);

    // 断连后 run 应在**远短于 600s** 内收敛并落库 assistant。
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
            .find(|e| e.role == oc_store::Role::Assistant && !e.content.is_empty())
            .map(|e| e.content.clone());
        if assistant.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = assistant.expect("断连后 ask_user 应返回未作答、run 继续跑完落库");
    assert!(text.contains("第2段"), "应含断连后继续产出的文本，实际: {text}");
}
