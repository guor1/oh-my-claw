//! P0-2 / P0-3 回归：审批等待期间可被 abort 打断，且审批 registry entry 不泄漏。
//!
//! 场景：模型请求一个危险命令（触发审批）→ 用户始终不回执 → abort 活跃 run。
//! 断言：
//! - run 能被 abort 及时终止（不永久卡在 `gate.ask().await`）——P0-2。
//! - abort 后审批 registry 已清空（RAII guard 清理）——P0-3。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session;
use oc_server::testing::{test_cfg, SessionConfigExt};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

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

#[tokio::test]
async fn abort_interrupts_pending_approval_and_cleans_registry() {
    let (tx, mut rx) = broadcast::channel(512);

    // Prompt 模式：危险命令弹审批。共享 registry 供断言泄漏。
    let mut reg = ToolRegistry::new();
    // 审批超时刻意取大（30s）：本用例验证的是 **abort** 能打断等待，
    // 超时若短于用例时长会抢先结束 run，就测不到 P0-2 那条路径了。
    reg.register(Arc::new(ExecTool::new(
        ApprovalMode::Prompt,
        Duration::from_secs(30),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));
    let registry: oc_server::state::ApprovalRegistry = Arc::new(dashmap::DashMap::new());
    let executor = ToolExecutor::new(Arc::new(reg)).with_approvals(Arc::clone(&registry));

    let cfg = test_cfg()
        .with_idle_timeout(Duration::from_secs(30))
        .with_tools(executor);

    let scripts = vec![tool_call_step("call-danger", "exec", "{\"command\": \"sudo rm -rf /\"}")];
    let provider = Arc::new(SequencedMock::new(scripts));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg,
        provider,
        tx,
        oc_store::Store::open_memory().unwrap(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    let run_id = handle
        .submit("跑个危险命令".into(), handle.broadcast_sink())
        .await
        .expect("run");

    // 等审批事件出现（说明 run 已卡在 gate.ask 等回执）。
    let mut saw_approval = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(ev, Event::Approval { .. }) {
            saw_approval = true;
            break;
        }
    }
    assert!(saw_approval, "应先收到审批请求事件");
    assert_eq!(registry.len(), 1, "审批等待期间 registry 应有一条待处理 entry");

    // 关键：不回执，直接 abort。P0-2 前此处会永久卡住。
    let start = std::time::Instant::now();
    handle.abort(run_id, false).await;

    // run 应及时以错误终态结束。
    let mut got_terminal = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(ev, Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }) {
            got_terminal = true;
            break;
        }
    }
    let elapsed = start.elapsed();
    assert!(got_terminal, "abort 后 run 应以错误终态结束（P0-2）");
    assert!(elapsed < Duration::from_secs(2), "abort 应立即生效，实际 {elapsed:?}");

    // registry entry 应已被 RAII guard 清理（P0-3）。给清理任务一点时间收敛。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(registry.is_empty(), "abort 后审批 registry 应清空（P0-3），实际 {} 条", registry.len());
}
