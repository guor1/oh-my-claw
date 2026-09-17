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

/// 把子进程原始字节解码成字符串：UTF-8 优先，失败退 GBK，再退 lossy。
///
/// - UTF-8：正常路径，零改动。
/// - GBK(cp936)：原生 Windows 命令（`ipconfig` 等）输出的是系统代码页而非
///   UTF-8；模型写的中文脚本（如 `print('中文')`）在 Windows Python 默认也按
///   GBK 出。按 UTF-8 lossy 解会得到 `ϲ������ɽ` 这种乱码，模型等于蒙着眼 debug。
/// - lossy：两个编码都解不动时的兜底，保证任何字节序列都不崩。
fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (s, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors {
        return s.into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 实际跑命令：跨平台选 shell，收集 stdout+stderr。
///
/// 按**原始字节**读取，再经 [`decode_output`] 解码（UTF-8 → GBK → lossy）。
async fn run_command(cmd: &str, cx: &ToolCtx, shell: &Shell) -> ToolResult<ToolOutput> {
    let mut command = shell.command(cmd);
    command.current_dir(&cx.cwd);
    // 让子进程用 UTF-8 输出：Windows 上 Python 默认按控制台代码页（GBK）编码
    // stdout，遇到 GBK 里没有的字符（如 `▪` U+25AA）直接 UnicodeEncodeError、
    // 脚本以退出码 1 挂掉——而 UTF-8 装得下所有 Unicode。这两个 env 对非 Python
    // 命令无副作用（只是被继承、被忽略）。见 docs/superpowers/plans/
    // 2026-09-17-windows-exec-encoding.md。
    command.env("PYTHONUTF8", "1");
    command.env("PYTHONIOENCODING", "utf-8");
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

    // 解码：UTF-8 → GBK → lossy。
    let mut collected = decode_output(&out_bytes);
    if !err_bytes.is_empty() {
        collected.push_str(&decode_output(&err_bytes));
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

#[cfg(test)]
mod tests {
    use super::decode_output;

    #[test]
    fn utf8_passthrough() {
        // 合法 UTF-8：零改动、原样返回（严格解码成功即返回）。
        assert_eq!(decode_output("你好".as_bytes()), "你好");
    }

    #[test]
    fn gbk_bytes_decode_correctly() {
        // 「喜马拉雅山」的 GBK 编码字节。旧实现按 UTF-8 lossy 会得乱码。
        let gbk = [0xcf, 0xb2, 0xc2, 0xed, 0xc0, 0xad, 0xd1, 0xc5, 0xc9, 0xbd];
        assert_eq!(decode_output(&gbk), "喜马拉雅山");
    }

    #[test]
    fn garbage_falls_back_to_lossy() {
        // 既不是 UTF-8 也不是 GBK 的字节序列 → lossy 兜底，不崩溃。
        let junk = [0xff, 0xfe, 0x00, 0x01];
        let s = decode_output(&junk);
        assert!(!s.is_empty(), "任何字节序列都不该崩");
    }
}
