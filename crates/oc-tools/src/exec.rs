//! exec 工具（设计 §6.1）：跑一次性命令/脚本。
//!
//! 审批门（危险命令弹审批）+ 工具级超时 + 取消 + 结果净化。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use oc_core::tool::{approval_decision, classify_command, ApprovalMode, ApprovalOutcome};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::error::{ToolError, ToolResult};
use crate::sanitize::sanitize;
use crate::shell::Shell;
use crate::types::{ApprovalReply, ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// exec 工具。持有审批模式（从 config 派生）。
pub struct ExecTool {
    pub mode: ApprovalMode,
    pub timeout: Duration,
    /// 等待用户审批回执的上限；`ZERO` = 不设上限。超时按拒绝处理。
    pub approval_timeout: Duration,
    /// 执行 shell（启动时探测，见 `crate::shell`）。
    pub shell: Shell,
}

#[derive(Deserialize)]
struct ExecArgs {
    /// 要执行的命令行。
    command: String,
}

impl ExecTool {
    pub fn new(mode: ApprovalMode, timeout: Duration, approval_timeout: Duration, shell: Shell) -> Self {
        Self { mode, timeout, approval_timeout, shell }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exec".to_string(),
            description: "执行一条 shell 命令并返回输出。危险命令会先请求用户审批。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令行" }
                },
                "required": ["command"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: true,
            timeout: self.timeout,
            backgroundable: true,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let args: ExecArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let cmd = args.command.trim().to_string();
        if cmd.is_empty() {
            return Err(ToolError::BadArgs("命令为空".into()));
        }

        // 审批门：分类 → 决策。
        let risk = classify_command(&cmd);
        match approval_decision(risk, self.mode) {
            ApprovalOutcome::Reject => return Err(ToolError::Denied),
            ApprovalOutcome::AskUser => {
                let Some(gate) = &cx.approval else {
                    // 无审批门却需要审批 → 保守拒绝。
                    return Err(ToolError::Denied);
                };
                let summary = format!("请求执行命令（风险: {risk:?}）");
                // 等审批期间必须响应取消：否则用户 abort / 看门狗判卡死时，
                // run 会永久卡在 ask().await 上，车道不释放（P0-2）。
                //
                // `approval_timeout` 是**叠加**在 cancel 之上的第二重上限，不替代
                // cancel 分支：cancel 覆盖「有人主动中止」，超时覆盖「根本没人来批」
                // （cron / HTTP 网关无 TUI 可响应）。二者缺一都会留下无界等待。
                let reply = tokio::select! {
                    _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
                    r = gate.ask(summary, cmd.clone(), self.approval_timeout) => r,
                };
                if reply == ApprovalReply::Deny {
                    return Err(ToolError::Denied);
                }
            }
            ApprovalOutcome::Execute => {}
        }

        cx.update(format!("$ {cmd}\n"));

        // 运行（超时 + 取消）。
        let child_fut = run_command(&cmd, &cx, &self.shell);
        let output = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Aborted),
            r = tokio::time::timeout(self.timeout, child_fut) => {
                match r {
                    Ok(res) => res?,
                    Err(_) => return Err(ToolError::Timeout),
                }
            }
        };

        Ok(output)
    }
}

/// 实际跑命令：跨平台选 shell，收集 stdout+stderr。
///
/// 按**原始字节**读取，再 lossy 解码为 UTF-8——bash 下模型若调用原生
/// Windows 命令（`ipconfig` 等）仍可能输出 GBK(cp936) 而非 UTF-8，若按行
/// 做严格 UTF-8 解码会直接 `InvalidData` 报错（表现为「命令输出有编码问题」）。
/// lossy 让非法字节退化为 `�` 而非崩溃。
async fn run_command(cmd: &str, cx: &ToolCtx, shell: &Shell) -> ToolResult<ToolOutput> {
    let mut command = shell.command(cmd);
    command.current_dir(&cx.cwd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // 并发读 stdout/stderr，避免任一管道写满导致子进程阻塞（死锁）。
    let read_stdout = async {
        let mut buf = Vec::new();
        if let Some(out) = stdout.as_mut() {
            let _ = out.read_to_end(&mut buf).await;
        }
        buf
    };
    let read_stderr = async {
        let mut buf = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let _ = err.read_to_end(&mut buf).await;
        }
        buf
    };
    let (out_bytes, err_bytes) = tokio::join!(read_stdout, read_stderr);

    let status = child.wait().await?;

    // lossy 解码：任何字节序列都不会让工具崩溃。
    let mut collected = String::from_utf8_lossy(&out_bytes).into_owned();
    if !err_bytes.is_empty() {
        collected.push_str(&String::from_utf8_lossy(&err_bytes));
    }
    // 一次性 emit 给 client（不再逐行，因为不再按行解码）。
    if !collected.is_empty() {
        cx.update(collected.clone());
    }

    let content = sanitize(&collected);
    Ok(ToolOutput {
        content: format!("{content}\n[退出码: {}]", status.code().unwrap_or(-1)),
        success: status.success(),
        background_task: None,
        new_cwd: None,
    })
}
