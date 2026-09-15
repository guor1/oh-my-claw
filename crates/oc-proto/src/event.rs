//! 服务端推送事件流（设计 §2.3）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, InputId, RunId, SessionId, TaskId, ToolCallId};
use crate::method::TaskState;

/// 服务端主动推送的事件。`tag = "event"`。
///
/// 每个变体都带 `session`，让多会话并发下 client 能把事件归属到正确的会话
/// （设计：多会话支持）。cron/heartbeat 等隔离子会话产生的事件归属 `main`。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// run 生命周期。
    Lifecycle {
        session: SessionId,
        run_id: RunId,
        phase: LifecyclePhase,
    },
    /// 流式回复增量。
    Assistant {
        session: SessionId,
        run_id: RunId,
        delta: String,
    },
    /// thinking 模型的推理增量。与 Assistant 分开：它不是可见回答，
    /// 只供客户端实时展示，不落库、不参与 prompt 重放。
    Reasoning {
        session: SessionId,
        run_id: RunId,
        delta: String,
    },
    /// 工具活动。
    Tool {
        session: SessionId,
        run_id: RunId,
        call_id: ToolCallId,
        phase: ToolPhase,
    },
    /// ★主动提醒推送（cron/intent 触发）。归属 `main` 会话。
    ///
    /// 注意：字段线上名为 `ptype`，不能叫 `kind`——`Frame` 用 `tag = "kind"`，
    /// 同名会在同一 JSON 对象产生重复 `kind` 键，反序列化报 duplicate field。
    Proactive {
        session: SessionId,
        #[serde(rename = "ptype")]
        kind: ProactiveKind,
        text: String,
        source: ProactiveSource,
    },
    /// 后台任务进展/完成。
    Task {
        session: SessionId,
        task_id: TaskId,
        update: TaskUpdate,
    },
    /// ★上下文用量：每轮 provider 报告真实 token 后推送，供 client 显示进度。
    Usage {
        session: SessionId,
        input_tokens: u32,
        context_window: u32,
    },
    /// ★审批请求：server 请求用户批准一个动作（如危险命令）。
    /// client 收到后应向用户展示，并用 `approval.reply` 方法回执。
    Approval {
        session: SessionId,
        approval_id: ApprovalId,
        run_id: RunId,
        summary: String,
        command: String,
    },
    /// ★用户输入请求（ask_user 工具）：模型主动提问，阻塞 run 等自由文本答复。
    /// client 收到后应向用户展示 `prompt` 并进入输入态，用 `user.reply` 方法回执。
    /// 与 `Approval`（仅 y/n）不同，回执带任意文本。
    UserInput {
        session: SessionId,
        input_id: InputId,
        run_id: RunId,
        prompt: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum LifecyclePhase {
    Start,
    End,
    Error { message: String, kind: RunErrorKind },
}

/// run 结束/异常的归一化分类（对应 core 的 RunOutcome）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunErrorKind {
    Aborted,
    Failed,
    Panicked,
    LoopDetected,
    Timeout,
    /// 模型输出被 max_tokens 截断，且续写次数用尽仍未产出完整回答。
    ///
    /// 与 `Failed` 分开：这不是模型或工具出错，是输出预算不够——用户的处置动作
    /// 不同（调大 `max_output_tokens`，或换个不把预算烧在 reasoning 上的模型）。
    Truncated,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ToolPhase {
    /// 工具调用发起。`args` 为完整参数 JSON 文本（不再截断——展示层自行裁剪）。
    Start { name: String, args: String },
    Update { chunk: String },
    End { status: ToolStatus },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Aborted,
    Backgrounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveKind {
    Reminder,
    Wake,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProactiveSource {
    Cron { cron_id: String },
    Intent { intent_id: String },
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdate {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::ids::{ApprovalId, InputId, RunId, SessionId, TaskId, ToolCallId};
    use crate::method::TaskState;

    /// 每个 Event 变体经 Frame 序列化后必须能反序列化回来。
    ///
    /// 回归：Frame 用 `tag = "kind"`，Event 内部字段若也叫 `kind`（如 Proactive
    /// 曾经的 kind 字段）会产生重复键，反序列化报 duplicate field。
    #[test]
    fn all_event_variants_roundtrip_through_frame() {
        let s = SessionId::main();
        let variants = vec![
            Event::Lifecycle { session: s.clone(), run_id: RunId::new("r"), phase: LifecyclePhase::Start },
            Event::Assistant { session: s.clone(), run_id: RunId::new("r"), delta: "hi".into() },
            Event::Reasoning { session: s.clone(), run_id: RunId::new("r"), delta: "想一下".into() },
            Event::Tool {
                session: s.clone(),
                run_id: RunId::new("r"),
                call_id: ToolCallId::new("c"),
                phase: ToolPhase::End { status: ToolStatus::Ok },
            },
            Event::Proactive {
                session: s.clone(),
                kind: ProactiveKind::Reminder,
                text: "t".into(),
                source: ProactiveSource::Heartbeat,
            },
            Event::Task {
                session: s.clone(),
                task_id: TaskId::new("t"),
                update: TaskUpdate { state: TaskState::Done, detail: None },
            },
            Event::Usage { session: s.clone(), input_tokens: 100, context_window: 64000 },
            Event::Approval {
                session: s.clone(),
                approval_id: ApprovalId::new("a"),
                run_id: RunId::new("r"),
                summary: "s".into(),
                command: "c".into(),
            },
            Event::UserInput {
                session: s.clone(),
                input_id: InputId::new("i"),
                run_id: RunId::new("r"),
                prompt: "你叫什么名字？".into(),
            },
        ];
        for ev in variants {
            let frame = Frame::Event(ev);
            let json = serde_json::to_string(&frame).expect("序列化");
            serde_json::from_str::<Frame>(&json)
                .unwrap_or_else(|e| panic!("反序列化失败: {e}\n  json={json}"));
        }
    }
}
