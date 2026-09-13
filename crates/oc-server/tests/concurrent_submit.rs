//! 回归：同一 session 并发提交两轮时，历史落库顺序 = 执行顺序，不交错。
//!
//! 旧 bug：用户消息在 submit 时立即落库。A 的长会话（多轮工具调用）仍在跑时，
//! B 提交 → B 的 user 消息先落库；A 后续又追加 assistant/tool 消息，排在 B 之后。
//! 等 B 起步 load_history，B 的 user 消息被 A 的回复埋在中间，序列以 assistant
//! 结尾 → provider 400。
//!
//! 修法：用户消息落库推迟到该轮真正起步时（见 session::begin_run）。本测试构造
//! A（工具轮 + 文本）与 B 交错提交，断言最终 transcript 里 A 的全部消息（user +
//! assistant/tool）都排在 B 的 user 消息之前。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;
use oc_server::testing::{test_cfg, SessionConfigExt};

fn tool_executor() -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
    ToolExecutor::new(Arc::new(reg))
}

fn cfg(tools: ToolExecutor) -> SessionConfig {
    test_cfg().with_tools(tools)
}

fn tool_call_step(id: &str, name: &str, args: &str, delay_ms: u64) -> Vec<ScriptStep> {
    vec![
        ScriptStep {
            delay: Duration::from_millis(delay_ms),
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: id.into(),
                name: Some(name.into()),
                args_chunk: args.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ]
}

fn text_step(t: &str, delay_ms: u64) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::from_millis(delay_ms), delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

/// 收满 N 个终态（End/Error）后返回。
async fn wait_n_terminal(rx: &mut broadcast::Receiver<Event>, n: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = 0;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        ) {
            seen += 1;
            if seen >= n {
                return;
            }
        }
    }
}

#[tokio::test]
async fn concurrent_submit_preserves_history_order() {
    let store = oc_store::Store::open_memory().expect("store");
    let (tx, mut rx) = broadcast::channel(512);

    // A：先工具调用（带 300ms 延迟，制造"仍在跑"的窗口）→ 结果回喂 → 文本收尾。
    // B：单轮文本。SequencedMock 按调用顺序发脚本；A 占两次模型调用，B 占一次。
    let scripts = vec![
        tool_call_step("call-a", "exec", "{\"command\": \"echo A\"}", 300), // A 第一轮
        text_step("A 完成", 50),                                             // A 第二轮（工具结果回喂后）
        text_step("B 完成", 0),                                             // B 唯一一轮
    ];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(tool_executor()),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    // 提交 A（会立即起步、占道），再几乎立刻提交 B（车道忙 → 排队）。
    handle.submit("A 的问题".into(), handle.broadcast_sink()).await.expect("run A");
    // 极短等待，确保 A 已起步占道，B 必然排队（而非抢先）。
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle.submit("B 的问题".into(), handle.broadcast_sink()).await.expect("run B");

    // 等两轮都终态。
    wait_n_terminal(&mut rx, 2, Duration::from_secs(10)).await;

    let hist = store
        .writer()
        .load_transcript("main".into(), 100)
        .await
        .expect("load");
    let contents: Vec<(&oc_store::Role, &str)> =
        hist.iter().map(|e| (&e.role, e.content.as_str())).collect();

    // 定位两条用户消息的下标。
    let a_user = contents.iter().position(|(_, c)| *c == "A 的问题").expect("A user 应落库");
    let b_user = contents.iter().position(|(_, c)| *c == "B 的问题").expect("B user 应落库");

    // 核心断言：B 的用户消息排在 A 的用户消息之后（执行顺序 = 落库顺序）。
    assert!(a_user < b_user, "A 的用户消息应先于 B：{contents:?}");

    // 且 A 轮的所有消息（user + tool 结果 + assistant）都排在 B 的 user 之前——
    // 即 A 与 B 的消息段不交错。B 的 user 之前不应出现属于 B 轮的 assistant。
    // 简化断言：B user 之后应只剩 B 的 assistant 回复（"B 完成"）。
    let after_b: Vec<&str> = contents[b_user + 1..].iter().map(|(_, c)| *c).collect();
    assert!(
        after_b.iter().all(|c| c.contains("B 完成")),
        "B 的 user 之后不应再夹杂 A 轮消息，实际尾部: {after_b:?}"
    );

    // A 的收尾文本应在 B 的 user 之前。
    let a_done = contents.iter().position(|(_, c)| c.contains("A 完成")).expect("A 收尾应落库");
    assert!(a_done < b_user, "A 的收尾回复应先于 B 的用户消息：{contents:?}");
}
