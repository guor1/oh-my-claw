//! 协议方法与返回（设计 §2.2）。MVP 最小集。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, CronId, InputId, IntentId, MemoryId, RunId, SessionId, TaskId};

/// 请求方法。`tag = "method", content = "params"`。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    /// 建立连接，返回 features + 初始快照。
    Connect(ConnectParams),
    /// 发一条消息，立即返回 `{run_id}`。side-effecting，需幂等键。
    ChatSend(ChatSendParams),
    /// 打断（防卡死 / 用户主动停）。
    ChatAbort(ChatAbortParams),
    /// 拉历史。
    ChatHistory(HistoryParams),
    /// `/new` `/reset`：推进上下文起点（可指定会话，缺省 main）。
    SessionReset(SessionResetParams),
    /// `/compact`：把历史摘要成 checkpoint（可指定会话，缺省 main）。
    Compact(CompactParams),
    /// 列出所有会话（多会话切换/浏览用）。
    SessionsList,
    /// 添加定时任务。side-effecting。
    CronAdd(CronAddParams),
    CronList,
    CronRm(CronRmParams),
    /// 添加 standing intent（话题触发式待办）。side-effecting。
    IntentAdd(IntentAddParams),
    IntentList,
    IntentRm(IntentRmParams),
    TasksList,
    TasksCancel(TaskCancelParams),
    /// 记忆检索（调试/自省）。
    MemorySearch(MemSearchParams),
    /// 审批回执：对 `Approval` 事件的应答。
    ApprovalReply(ApprovalReplyParams),
    /// 用户输入回执：对 `UserInput` 事件（ask_user 工具）的自由文本应答。
    UserReply(UserReplyParams),
    Status,
    Health,
    /// 整机诊断快照（`oc debug`）：会话表 + 活跃 run + 队列 + 写线程健康。
    Diagnostics,
    /// 斜杠指令：daemon 是唯一解析器，客户端只负责「是否 `/` 开头」就发过来。
    /// 新增指令改 daemon 一处，TUI / Web UI 自动获得。
    Command(CommandParams),
    /// 接续一个在途 Detached run 的剩余内联事件流（回放缓冲 + 续流）。
    ChatResume(ChatResumeParams),
}

/// 方法成功返回，与 [`Method`] 一一对应。
///
/// 用**邻接标签**（`tag` + `content`）而非内部标签：内部标签无法序列化
/// "包着序列的 newtype 变体"（如 `Sessions(Vec<..>)` 会在运行时报错），
/// 邻接标签把载荷放进独立的 `data` 字段，规避该限制。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "ok", content = "data", rename_all = "snake_case")]
pub enum MethodOk {
    Hello { features: Features, snapshot: Snapshot },
    ChatSend { run_id: RunId },
    Empty,
    History(Vec<Entry>),
    CronAdd { cron_id: CronId },
    CronList(Vec<CronSpec>),
    IntentAdd { intent_id: IntentId },
    IntentList(Vec<IntentSpec>),
    Tasks(Vec<TaskView>),
    MemorySearch(Vec<MemHit>),
    Sessions(Vec<SessionView>),
    Status(Snapshot),
    Health(HealthOk),
    Diagnostics(DiagnosticsSnapshot),
    /// 斜杠指令的执行结果：`text` 给用户看，`switch_session`/`clear_view` 让客户端
    /// 应用视图副作用。
    Command(CommandResult),
    /// resume 已挂上，后续事件经本连接出站队列流式回发。
    ChatResume { session: SessionId },
}

// ── params ──────────────────────────────────────────────────────

/// 连接的客户端类型：决定 run 是否随连接断开而中止。
///
/// - `Interactive`：TUI / CLI 等驻留客户端。断连即中止 run、立即释放车道
///   （原有语义，对应 `RunSink::Conn` 的 `closed()` 探测）。
/// - `Detached`：Web / HTTP 无状态网关。run 归属会话而非连接，断连不中止——
///   客户端刷新页面后 run 继续跑完并落库（对齐 OpenClaw 的 delivery-key 解耦）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    #[default]
    Interactive,
    Detached,
}

impl ClientKind {
    fn is_interactive(&self) -> bool {
        matches!(self, ClientKind::Interactive)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConnectParams {
    pub proto_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// 客户端类型（见 [`ClientKind`]）。旧客户端缺省为 Interactive。
    #[serde(default, skip_serializing_if = "ClientKind::is_interactive")]
    pub client_kind: ClientKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatSendParams {
    #[serde(default)]
    pub session: Option<SessionId>,
    pub text: String,
}

/// ChatResume 参数：显式带 session（避免遍历 registry，也匹配前端手头的 active_session_id）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatResumeParams {
    pub session: SessionId,
    pub run_id: RunId,
    /// 客户端已从落库历史拿到的最大 entry seq。服务端据此裁剪回放起点：
    /// 落进 seq ≤ 本值的事件不重放（否则前端工具卡建两张、正文渲染两遍）。
    /// `0` = 什么都没有，全量回放。
    #[serde(default)]
    pub since_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionResetParams {
    /// 目标会话；缺省为 main。
    #[serde(default)]
    pub session: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompactParams {
    /// 目标会话；缺省为 main。
    #[serde(default)]
    pub session: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatAbortParams {
    pub run_id: RunId,
    /// soft: 先 drain 排队轮再中止；hard: 立即中止活跃 run。
    #[serde(default)]
    pub hard: bool,
}

/// 斜杠指令请求：客户端只透传原文（已 trim），daemon 负责解析与执行。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandParams {
    /// 目标会话（`/session <id>`、`/reset`、`/stop` 等按会话生效）；缺省 main。
    #[serde(default)]
    pub session: Option<SessionId>,
    /// 以 `/` 开头的原文，如 `/help`、`/session ppt2`。
    pub text: String,
}

/// 斜杠指令执行结果。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandResult {
    /// 给用户看的格式化文本（多行）。
    pub text: String,
    /// `/new` / `/session <id>`：让客户端切换到该会话（后续消息归属它）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_session: Option<SessionId>,
    /// `/reset` / `/new`：让客户端清掉当前会话的显示缓冲。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_view: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryParams {
    #[serde(default)]
    pub session: Option<SessionId>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronAddParams {
    pub expr: String,
    pub prompt: String,
    pub tz: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronRmParams {
    pub cron_id: CronId,
}

/// 新增 standing intent 的参数。
///
/// anti-nagging 三项可省略：省略则用服务端全局默认（`ProactiveConfig`）。
/// 与 cron 不同，standing intent 不带时间表达式——它由**话题命中**触发。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntentAddParams {
    /// 触发后注入的提醒正文（如"带转换插头"）。
    pub text: String,
    /// 词法触发关键词，命中任一即触发（如 ["出差", "德国"]）。
    pub keywords: Vec<String>,
    /// 两次触发最小间隔（秒）；省略取服务端默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<i64>,
    /// 触发次数上限；省略取服务端默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
    /// 多少天后过期；省略取服务端默认，0 = 不过期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_days: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntentRmParams {
    pub intent_id: IntentId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskCancelParams {
    pub task_id: TaskId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemSearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalReplyParams {
    pub approval_id: ApprovalId,
    /// true = 批准，false = 拒绝。
    pub allow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UserReplyParams {
    pub input_id: InputId,
    /// 用户输入的自由文本；`None`/空 = 用户取消（未作答）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ── 返回体 ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Features {
    pub ws_remote: bool,
    pub memory_vec: bool,
    pub sandbox: bool,
    pub proto_version: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub active_run: Option<RunId>,
    pub queued_turns: u32,
    pub background_tasks: u32,
    pub session: SessionId,
    /// 模型上下文窗口（token）。
    pub context_window: u32,
    /// 最近一轮 provider 报告的真实输入 token 数（已用上下文近似）；无则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_tokens: Option<u32>,
    /// 当前生效的模型名（实际发进请求体 `model` 字段的那个串）。
    ///
    /// 与 provider/endpoint 一起，让「换了 provider 有没有生效」不必开对话就能验证。
    /// 老 daemon 不发这三项，故都带 serde default。
    #[serde(default)]
    pub model: String,
    /// provider 标识（openai / anthropic / mock）。
    ///
    /// 单看它分不出 DeepSeek 与豆包——两者都是 `openai`，要连 `endpoint` 一起看。
    #[serde(default)]
    pub provider: String,
    /// 实际请求的 API 基地址；mock provider 无端点则为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthOk {
    pub ok: bool,
    pub db_version: u32,
}

/// assistant 发起的一次工具调用（对外视图，历史回放用）。
///
/// 与 `oc-llm` 的 `ToolCallSpec` 同构但独立定义：协议层不依赖 provider 层，
/// 只透传库里的 `tool_calls`（JSON 数组文本）解析结果。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ToolCallSpec {
    pub id: String,
    pub name: String,
    /// 参数 JSON 文本（原样）。
    pub args: String,
}

/// 一条 transcript 记录（对外视图）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Entry {
    pub seq: i64,
    pub role: Role,
    pub content: String,
    /// assistant 发起的工具调用；`None` = 该条没发起调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallSpec>>,
    /// 该条工具结果关联的调用 id；`None` = 不是工具结果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

/// 一个会话的对外视图（sessions.list 用）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub id: SessionId,
    pub kind: String,
    pub created_at: i64,
    /// 上下文起点（reset 推进），无则 0。
    pub reset_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronSpec {
    pub id: CronId,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    pub enabled: bool,
    pub next_at: Option<i64>,
}

/// 一条 standing intent 的对外视图（含触发记账，供 `oc intent list` 展示）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntentSpec {
    pub id: IntentId,
    pub text: String,
    pub keywords: Vec<String>,
    pub cooldown_secs: i64,
    pub budget: u32,
    /// 已触发次数（budget 用尽即静默）。
    pub fired_count: u32,
    pub last_fired_at: Option<i64>,
    /// 过期时间点（unix 秒）；None = 不过期。
    pub expiry_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskView {
    pub id: TaskId,
    pub kind: String,
    pub state: TaskState,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemHit {
    pub id: MemoryId,
    pub tier: String,
    pub text: String,
    pub score: f32,
    /// 来源追溯（FEAT-3）；`None` = 无出处。
    pub source: Option<String>,
}

// ── 诊断快照（oc debug）────────────────────────────────────────

/// 整机诊断快照：运行时状态的一次采样，用于定位「不回复 / 截断 / 卡死」等
/// 时序问题。纯观测数据，无副作用。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsSnapshot {
    /// daemon 运行时长（秒）。
    pub uptime_secs: u64,
    /// 当前所有会话的运行时状态。
    pub sessions: Vec<SessionDiag>,
    /// 写线程是否存活（ping 一次写线程确认）。
    pub store_writer_alive: bool,
    /// 当前事件订阅者数（活跃连接近似）。
    pub event_subscribers: usize,
    /// 幂等缓存当前条数（P2-3）。
    ///
    /// 长挂时观察内存是否稳定的抓手：本数与会话行数是两处按键无界增长的地方，
    /// 各有 TTL / 空闲淘汰在收。若它随时间单调上涨，说明清扫没在跑。
    /// `serde(default)` 兼容旧 server 的应答（缺该键按 0）。
    #[serde(default)]
    pub idem_entries: usize,
    /// 采样时刻（unix ms），供 client 计算各 run 的实时 age。
    pub sampled_at: i64,
    pub proto_version: u16,
}

/// 单会话运行时诊断。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionDiag {
    pub session_id: SessionId,
    /// 排队等待的轮数（不含活跃）。
    pub queue_depth: usize,
    /// 当前活跃 run（无则 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RunSnapshot>,
    /// 车道被占用起始时刻（unix ms）；长 run / compact 阻塞可一眼看出。无占用则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_busy_since: Option<i64>,
    /// 本会话累计起过的 run 数。
    pub total_runs: u64,
    /// 最近一次 run 的结束原因（outcome / finish_reason 文本）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finish_reason: Option<String>,
    /// 最近一次错误文本（若有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 活跃 run 的运行时快照。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunSnapshot {
    pub run_id: RunId,
    /// 当前阶段（排队/等模型/流式/工具执行）。
    pub phase: RunPhase,
    /// run 起始时刻（unix ms）。
    pub started_at: i64,
    /// 最近一次收到模型 delta 的时刻（unix ms）；判断「卡在等模型」用。无则 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delta_at: Option<i64>,
    /// 已进行的工具调用轮数。
    pub tool_rounds: usize,
    /// 已累积的 assistant 文本长度（字符）；判断「有没有在出字」。
    pub acc_chars: usize,
}

/// run 阶段（诊断用，比 oc-core 的 RunState 更粗粒度且可跨 crate 传输）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// 已提交、尚未起步（仍在 append_entry / load_history 等准备阶段）。
    Starting,
    /// 已发起模型调用、等待响应。
    AwaitingModel,
    /// 正在接收模型流式输出。
    Streaming,
    /// 正在执行工具。
    ToolExec,
    /// 正在执行 compact 摘要（占用车道）。
    Compacting,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧客户端（无 client_kind 字段）的握手帧反序列化后应为 Interactive——
    /// 这是向后兼容红线：老 CLI/TUI 不得因此被当成 Detached。
    #[test]
    fn connect_params_defaults_client_kind_to_interactive() {
        let p: ConnectParams =
            serde_json::from_str(r#"{"proto_version":1,"token":null}"#).expect("deser");
        assert_eq!(p.client_kind, ClientKind::Interactive);
    }

    #[test]
    fn client_kind_roundtrips() {
        let p = ConnectParams {
            proto_version: 1,
            token: None,
            client_kind: ClientKind::Detached,
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: ConnectParams = serde_json::from_str(&s).expect("deser");
        assert_eq!(back.client_kind, ClientKind::Detached);
    }

    #[test]
    fn chat_resume_roundtrips() {
        let m = Method::ChatResume(ChatResumeParams {
            session: SessionId::main(),
            run_id: RunId::new("r1"),
            since_seq: 3,
        });
        let s = serde_json::to_string(&m).expect("ser");
        let back: Method = serde_json::from_str(&s).expect("deser");
        assert!(matches!(back, Method::ChatResume(_)));
    }

    /// 旧客户端不带 since_seq 的帧反序列化后应为 0（全量回放）——向后兼容红线。
    #[test]
    fn chat_resume_defaults_since_seq_to_zero() {
        let p: ChatResumeParams = serde_json::from_str(
            r#"{"session":"main","run_id":"r1"}"#,
        )
        .expect("deser");
        assert_eq!(p.since_seq, 0);
    }
}
