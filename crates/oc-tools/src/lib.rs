//! oc 工具集（设计 §6）。
//!
//! `Tool` trait + 策略管道 + exec 审批门 + 结果净化。
//! 工具：exec/file/process/web_fetch/web_search/message/ask_user/cron_add。

pub mod ask_user;
pub mod cron;
pub mod error;
pub mod exec;
pub mod file;
pub mod message;
pub mod path_guard;
pub mod process;
pub mod registry;
pub mod sanitize;
pub mod shell;
pub mod sys;
pub mod types;
#[cfg(feature = "web")]
pub mod web;

pub use error::{ToolError, ToolResult};
pub use registry::ToolRegistry;
pub use types::*;

use async_trait::async_trait;

/// 一个工具（设计 §6）。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 规格：名称/描述/参数 schema（进 prompt）。
    fn spec(&self) -> ToolSpec;

    /// 策略：是否需审批 / 超时 / 可后台。
    fn policy(&self) -> ToolPolicy;

    /// 执行。
    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput>;
}
