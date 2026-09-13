//! process 工具（设计 §6.1）：长命令转后台、可 kill、查状态。
//!
//! M4：后台起进程 → 注册到台账（由 server 提供的回调）→ 返回 task_id。
//! 进程生命周期由 server 的 TaskLedger 跟踪；此工具负责 spawn 与句柄移交。

use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::error::{ToolError, ToolResult};
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::shell::Shell;
use crate::Tool;

/// 后台进程移交事件：server 收到后登记到台账并跟踪。
pub struct BackgroundHandoff {
    pub command: String,
    /// 后台进程的输出流（逐行）。
    pub output: mpsc::UnboundedReceiver<String>,
    /// 完成通知（退出码）。
    pub done: tokio::sync::oneshot::Receiver<i32>,
}

/// process 工具。持有向 server 移交后台进程的发送端。
pub struct ProcessTool {
    handoff: mpsc::UnboundedSender<BackgroundHandoff>,
    /// 执行 shell（启动时探测，见 `crate::shell`）。
    shell: Shell,
}

#[derive(Deserialize)]
struct ProcArgs {
    command: String,
}

impl ProcessTool {
    pub fn new(handoff: mpsc::UnboundedSender<BackgroundHandoff>, shell: Shell) -> Self {
        Self { handoff, shell }
    }
}

#[async_trait]
impl Tool for ProcessTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process".to_string(),
            description: "在后台启动长命令，立即返回任务 id；进度经后台任务台账跟踪。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "后台命令行" }
                },
                "required": ["command"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false,
            timeout: std::time::Duration::from_secs(5), // 仅 spawn 阶段
            backgroundable: true,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: ProcArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let cmd = args.command.trim().to_string();
        if cmd.is_empty() {
            return Err(ToolError::BadArgs("命令为空".into()));
        }

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        // spawn 后台进程（工作目录 = 会话 cwd），输出打到 out_tx。
        spawn_background(&cmd, &cx.cwd, out_tx, done_tx, &self.shell)?;

        // 移交给 server 登记台账。
        self.handoff
            .send(BackgroundHandoff { command: cmd.clone(), output: out_rx, done: done_rx })
            .map_err(|_| ToolError::Failed("无法移交后台任务（server 未接收）".into()))?;

        Ok(ToolOutput {
            content: format!("已在后台启动：{cmd}"),
            success: true,
            background_task: Some("pending".to_string()), // server 分配真实 id
            new_cwd: None,
        })
    }
}

fn spawn_background(
    cmd: &str,
    cwd: &std::path::Path,
    out_tx: mpsc::UnboundedSender<String>,
    done_tx: tokio::sync::oneshot::Sender<i32>,
    shell: &Shell,
) -> ToolResult<()> {
    let mut command = shell.command(cmd);
    command.current_dir(cwd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();

    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        if let Some(out) = stdout {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = out_tx.send(line);
            }
        }
        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = done_tx.send(code);
    });

    Ok(())
}
