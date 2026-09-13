//! M4 工具循环测试：模型请求工具 → 执行 → 结果回喂 → 再次调用模型 → 完成。
//! 以及 loop detection。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_llm::mock::SequencedMock;
use oc_llm::{Delta, FinishReason};
use oc_llm::types::ToolCallDelta;
use oc_llm::mock::ScriptStep;
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
    // Allow 模式：命令直接执行，便于测试闭环（不弹审批）。
    reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
    ToolExecutor::new(Arc::new(reg))
}

fn cfg_with_tools(tools: ToolExecutor) -> SessionConfig {
    test_cfg().with_tools(tools)
}

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

fn text_step(t: &str) -> Vec<ScriptStep> {
    vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text(t.into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ]
}

async fn collect_until_terminal(rx: &mut broadcast::Receiver<Event>, timeout: Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    // 超时或通道关闭（Err）都让 while let 自然退出。
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let terminal = matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        );
        out.push(ev);
        if terminal { break; }
    }
    out
}

#[tokio::test]
async fn model_calls_tool_then_completes() {
    let (tx, mut rx) = broadcast::channel(512);
    // 第一轮：模型请求 exec echo；第二轮：模型基于结果给出文本回复。
    let echo = "echo tool-ran";
    let scripts = vec![
        tool_call_step("call-1", "exec", &format!("{{\"command\": \"{echo}\"}}")),
        text_step("命令已执行完成"),
    ];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(oc_proto::SessionId::main(), cfg_with_tools(tool_executor()), provider, tx, oc_store::Store::open_memory().unwrap(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    let _run = handle.submit("帮我执行 echo".into(), handle.broadcast_sink()).await.expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    // 应看到 tool start/update/end + 最终 assistant 文本 + End。
    assert!(evs.iter().any(|e| matches!(e, Event::Tool { .. })), "应有工具事件");
    assert!(
        evs.iter().any(|e| matches!(e, Event::Assistant { delta, .. } if delta.contains("命令已执行完成"))),
        "应有最终文本回复"
    );
    assert!(
        evs.iter().any(|e| matches!(e, Event::Lifecycle { phase: LifecyclePhase::End, .. })),
        "应正常结束"
    );
}

#[tokio::test]
async fn dangerous_command_triggers_approval_then_runs() {
    use std::sync::Arc as StdArc;

    let (tx, mut rx) = broadcast::channel(512);

    // Prompt 模式 + 共享审批 registry（审批事件经 per-run sink，回执经 registry）。
    let mut reg = ToolRegistry::new();
    reg.register(StdArc::new(ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
    let registry: oc_server::state::ApprovalRegistry = StdArc::new(dashmap::DashMap::new());
    let executor = ToolExecutor::new(StdArc::new(reg)).with_approvals(StdArc::clone(&registry));

    let cfg = test_cfg().with_tools(executor);

    // 危险命令：sudo（会判 NeedsApproval）。审批放行后进入执行。
    let scripts = vec![
        tool_call_step("call-danger", "exec", "{\"command\": \"sudo echo hi\"}"),
        text_step("完成"),
    ];
    let provider = StdArc::new(SequencedMock::new(scripts));
    let handle = session::spawn(oc_proto::SessionId::main(), cfg, provider, tx, oc_store::Store::open_memory().unwrap(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    let _run = handle.submit("执行危险命令".into(), handle.broadcast_sink()).await.expect("run");

    // 后台监听 Approval 事件并放行。
    let reg2 = StdArc::clone(&registry);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(Event::Approval { approval_id, .. })) => {
                    if let Some((_, tx)) = reg2.remove(&approval_id) {
                        let _ = tx.send(true); // 放行
                    }
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
    });

    // 等待运行结束（放行后应正常完成）。
    tokio::time::sleep(Duration::from_secs(2)).await;
    // 断言：审批注册表已清空（说明审批被消费）。
    assert!(registry.is_empty(), "审批应已被消费");
}

#[tokio::test]
async fn loop_detection_breaks_repeated_tool_calls() {
    let (tx, mut rx) = broadcast::channel(512);
    // 模型每轮都请求同一个工具调用（相同参数）→ 应被 loop detection 打断。
    let same = tool_call_step("call-x", "exec", "{\"command\": \"echo loop\"}");
    let scripts = vec![same.clone(), same.clone(), same.clone(), same.clone(), same.clone()];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(oc_proto::SessionId::main(), cfg_with_tools(tool_executor()), provider, tx, oc_store::Store::open_memory().unwrap(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    let _run = handle.submit("触发打转".into(), handle.broadcast_sink()).await.expect("run");
    let evs = collect_until_terminal(&mut rx, Duration::from_secs(5)).await;

    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::Lifecycle { phase: LifecyclePhase::Error { kind: oc_proto::RunErrorKind::LoopDetected, .. }, .. }
        )),
        "应以 LoopDetected 终态结束"
    );
}
