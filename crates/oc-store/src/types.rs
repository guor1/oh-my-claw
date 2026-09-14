//! store 数据类型（设计 §3.3）。持久化层的输入/输出 DTO。
//!
//! 这些是 store 与上层之间的数据契约，不含策略（策略在 oc-core）。
//!
//! **枚举与字符串的两条转换路径**：库表里 role/tier/origin 都是裸 TEXT（schema
//! 无 CHECK 约束）。因此这里给每个枚举提供两种解析：
//!
//! - [`std::str::FromStr`]（`s.parse::<Tier>()`）：严格，未知值报错。用于校验
//!   外部输入（配置、协议参数），错就该让调用方知道。
//! - `from_db_str`：宽松，未知值退化到安全默认。**只给读库用**——单行的脏值
//!   （老 schema 残留、手工改库）不该让整个查询失败；`origin` 更是必须退化到
//!   `Untrusted` 而非报错，未知来源按最低信任处理才是抗投毒的正确方向。

use std::str::FromStr;

/// 枚举字符串解析失败（[`FromStr`]）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind}: {value:?}")]
pub struct ParseEnumError {
    /// 目标枚举名（如 `"role"`）。
    pub kind: &'static str,
    /// 原始输入。
    pub value: String,
}

impl ParseEnumError {
    fn new(kind: &'static str, value: &str) -> Self {
        Self { kind, value: value.to_string() }
    }
}

/// 会话种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Main,
    Cron,
    Dreaming,
    Lane2,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Main => "main",
            SessionKind::Cron => "cron",
            SessionKind::Dreaming => "dreaming",
            SessionKind::Lane2 => "lane2",
        }
    }
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }

    /// 读库用的宽松解析：未知值退化为 [`Role::User`]。见模块头说明。
    pub fn from_db_str(s: &str) -> Role {
        s.parse().unwrap_or(Role::User)
    }
}

impl FromStr for Role {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            "system" => Ok(Role::System),
            _ => Err(ParseEnumError::new("role", s)),
        }
    }
}

/// 待写入的一条 transcript 记录。
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub session_id: String,
    pub role: Role,
    pub content: String,
    pub tokens_est: i64,
    /// assistant 发起的工具调用（`Vec<ToolCallSpec>` 的 JSON 数组文本）；
    /// `None` = 该条没发起调用。见 [`NewEntry::tool_call_id`]。
    pub tool_calls: Option<String>,
    /// 该条工具结果关联的调用 id；`None` = 不是工具结果。
    ///
    /// 这两列一起让重放能还原原生工具结构（P2-4）：没有它们，`Role::Tool` 只能
    /// 降级成 user 文本，历史里就永远不出现「assistant 发起调用」的样例。
    pub tool_call_id: Option<String>,
}

impl NewEntry {
    /// 纯文本记录（无工具结构）——绝大多数 user/assistant/system 条目走这里。
    pub fn text(session_id: impl Into<String>, role: Role, content: impl Into<String>, tokens_est: i64) -> Self {
        Self {
            session_id: session_id.into(),
            role,
            content: content.into(),
            tokens_est,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

/// 已存储的一条会话记录（session.list 用）。
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub kind: String,
    pub created_at: i64,
    /// 上下文起点（reset 推进），无则 0。
    pub reset_at: i64,
}

/// 已存储的一条 transcript 记录。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub session_id: String,
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub tokens_est: i64,
    /// 见 [`NewEntry::tool_calls`]。
    pub tool_calls: Option<String>,
    /// 见 [`NewEntry::tool_call_id`]。
    pub tool_call_id: Option<String>,
    pub created_at: i64,
}

/// 记忆分层（设计 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Curated,
    Episodic,
    Prospective,
    Review,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Curated => "curated",
            Tier::Episodic => "episodic",
            Tier::Prospective => "prospective",
            Tier::Review => "review",
        }
    }
    /// 读库用的宽松解析：未知值退化为 [`Tier::Episodic`]。见模块头说明。
    pub fn from_db_str(s: &str) -> Tier {
        s.parse().unwrap_or(Tier::Episodic)
    }
}

impl FromStr for Tier {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "curated" => Ok(Tier::Curated),
            "episodic" => Ok(Tier::Episodic),
            "prospective" => Ok(Tier::Prospective),
            "review" => Ok(Tier::Review),
            _ => Err(ParseEnumError::new("tier", s)),
        }
    }
}

/// 记忆来源（provenance，抗投毒，设计 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Owner,
    Agent,
    Untrusted,
    System,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Owner => "owner",
            Origin::Agent => "agent",
            Origin::Untrusted => "untrusted",
            Origin::System => "system",
        }
    }
    /// 读库用的宽松解析：未知值退化为 [`Origin::Untrusted`]（最低信任）。
    /// 见模块头说明——这里的退化是抗投毒的有意设计，不要改成报错。
    pub fn from_db_str(s: &str) -> Origin {
        s.parse().unwrap_or(Origin::Untrusted)
    }
}

impl FromStr for Origin {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Origin::Owner),
            "agent" => Ok(Origin::Agent),
            "untrusted" => Ok(Origin::Untrusted),
            "system" => Ok(Origin::System),
            _ => Err(ParseEnumError::new("origin", s)),
        }
    }
}

/// 待写入的一条记忆。
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub keywords: Option<String>,
    pub importance: f64,
    pub content_hash: String,
    /// 偏好主题（P1-3，设计 §4.5(e)）。`Some` = 该条是偏好，参与 supersede
    /// （同主题的新值就地替换旧值）；`None` = 普通记忆，走内容哈希去重。
    /// 由 `oc_core::memory::extract_pref_key` 判定，调用方填入。
    pub pref_key: Option<String>,
    /// 来源追溯（FEAT-3，设计 §4.1）：这条记忆的出处（如 flush 出它的 session_id）。
    /// 与 `origin`（信任分级）正交；`None` = 无出处（用户显式「记住…」或历史行）。
    pub source: Option<String>,
}

/// 待写入的一条定时任务。
#[derive(Debug, Clone)]
pub struct NewCron {
    pub id: String,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    /// 下次触发（unix 秒）；由 core::next_fire 算好传入。
    pub next_at: Option<i64>,
}

/// 已存储的一条定时任务。
#[derive(Debug, Clone)]
pub struct CronRow {
    pub id: String,
    pub expr: String,
    pub prompt: String,
    pub tz: String,
    pub next_at: Option<i64>,
    pub last_fired_at: Option<i64>,
    pub enabled: bool,
}

/// 待写入的一条 standing intent（事件型待办）。
///
/// keywords 类型层用 `Vec<String>`，落库拼成空格分隔 TEXT（schema `keywords TEXT`）。
/// cooldown/budget/expiry 为该条自己的 anti-nagging 参数；调用方（server）用全局
/// 配置默认或用户指定填入。expiry_at 为绝对 unix 秒；None = 不过期。
#[derive(Debug, Clone)]
pub struct NewStandingIntent {
    pub id: String,
    /// 触发后注入的提醒正文。
    pub text: String,
    /// 词法触发关键词（命中任一即触发）。
    pub keywords: Vec<String>,
    pub cooldown_secs: i64,
    pub budget: u32,
    /// 过期时间点（unix 秒）；None = 不过期。
    pub expiry_at: Option<i64>,
}

/// 已存储的一条 standing intent（含触发判定所需的运行时状态）。
#[derive(Debug, Clone)]
pub struct StandingIntentRow {
    pub id: String,
    pub text: String,
    pub keywords: Vec<String>,
    pub cooldown_secs: i64,
    pub budget: u32,
    pub fired_count: u32,
    pub last_fired_at: Option<i64>,
    pub expiry_at: Option<i64>,
    pub created_at: i64,
}

/// 已存储的记忆（含检索所需字段）。
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub importance: f64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub content_hash: String,
    /// 偏好主题；`None` = 非偏好类记忆。见 [`NewMemory::pref_key`]。
    pub pref_key: Option<String>,
    /// 来源追溯；`None` = 无出处。见 [`NewMemory::source`]。
    pub source: Option<String>,
}
