//! M4 后台任务台账测试：process 工具移交 → 台账登记 → 完成事件。

use std::time::Duration;

use oc_proto::{Event, TaskState};
use oc_server::ledger::TaskLedger;
use oc_tools::process::{BackgroundHandoff, ProcessTool};
use oc_tools::shell::Shell;
use oc_tools::types::ToolCtx;
use oc_tools::Tool;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn process_tool_registers_and_completes() {
    let (ev_tx, mut ev_rx) = broadcast::channel(64);
    let ledger = TaskLedger::new(ev_tx);

    // process 工具的移交 channel。
    let (handoff_tx, mut handoff_rx) = mpsc::unbounded_channel::<BackgroundHandoff>();
    let tool = ProcessTool::new(handoff_tx, Shell::resolve().unwrap());

    // 台账 pump：收到移交就登记。
    tokio::spawn(async move {
        while let Some(h) = handoff_rx.recv().await {
            ledger.register(h);
        }
    });

    // 跑一个立即结束的后台命令。
    // echo 在 Git Bash 与 POSIX sh 下写法一致，无需按平台分支。
    let cmd = "echo bg-done";
    let cx = ToolCtx::detached(CancellationToken::new());
    let out = tool
        .invoke(serde_json::json!({ "command": cmd }), cx)
        .await
        .expect("process spawn ok");
    assert!(out.background_task.is_some(), "应返回后台任务标记");

    // 应收到 Running → Done 的 Task 事件。
    let mut saw_running = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, ev_rx.recv()).await {
        if let Event::Task { update, .. } = ev {
            match update.state {
                TaskState::Running => saw_running = true,
                TaskState::Done => {
                    saw_done = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(saw_running, "应有 Running 事件");
    assert!(saw_done, "应有 Done 事件");
}
