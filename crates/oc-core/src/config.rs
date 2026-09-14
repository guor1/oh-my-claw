//! 配置类型、校验、SecretRef（设计 §4.8、§13.2）。
//!
//! 解引用 SecretRef（读 env/文件）是 IO，在 server 做；core 只持类型 + 校验形状。
//!
//! 不支持热更（P2-5 定案方案 B）：修改 config.toml 后需重启 `oc serve` 才会生效。

use std::path::PathBuf;

use garde::Validate;
use serde::{Deserialize, Serialize};

/// 顶层配置。分节：server / models / memory / proactive / tools / watchdog / skills。
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct Config {
    #[garde(range(min = 1))]
    pub proto_version: u16,

    #[garde(dive)]
    pub server: ServerConfig,

    #[garde(length(min = 1), dive)]
    pub models: Vec<ModelConfig>,

    #[garde(dive)]
    #[serde(default)]
    pub context: ContextConfig,

    #[garde(dive)]
    pub memory: MemoryConfig,

    #[garde(dive)]
    pub proactive: ProactiveConfig,

    #[garde(dive)]
    pub tools: ToolsConfig,

    #[garde(dive)]
    pub watchdog: WatchdogConfig,

    #[garde(skip)]
    #[serde(default)]
    pub skills: SkillsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ServerConfig {
    #[garde(skip)]
    pub transport: Transport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Unix,
    Pipe,
    Ws,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ModelConfig {
    #[garde(length(min = 1))]
    pub alias: String,
    #[garde(skip)]
    pub provider: Provider,
    #[garde(length(min = 1))]
    pub model: String,
    #[garde(skip)]
    pub hosting: Hosting,
    /// API key，SecretRef 三态。
    #[garde(skip)]
    pub api_key: SecretRef,
    /// 自定义 API 基地址（OpenAI 兼容端点，如 DeepSeek）。None = 官方默认。
    #[garde(skip)]
    #[serde(default)]
    pub base_url: Option<String>,
    /// 上下文窗口（token）。None = 按 model 名查内置默认表，查不到用保守默认。
    /// 压缩预算据此派生（budget = window − reserve）。
    #[garde(skip)]
    #[serde(default)]
    pub context_window: Option<u32>,
    /// 单轮**输出**上限（token），即请求体里的 `max_tokens`。
    ///
    /// 与 `context_window`（输入侧预算）是两回事，别混：这一项管模型一轮能吐多长。
    /// `None` = 不发该字段，由服务端挑默认值。
    ///
    /// thinking 类模型要留意：reasoning 也算在这个预算里。服务端默认值往往只够
    /// 一段普通回复，模型把预算烧在推理上就会被硬截断，一个工具都调不出来。
    #[garde(skip)]
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// 强制指定输出上限的请求字段名：`max_tokens` 或 `max_completion_tokens`。
    ///
    /// `None` = 按 provider 自动判断（见 oc-llm::openai::max_tokens_field）。
    /// 各家对这两个名字的取舍还在变，自动判断必然滞后；撞上时改这一项即可，
    /// 不用等改代码。填别的值忽略。
    #[garde(skip)]
    #[serde(default)]
    pub max_tokens_field: Option<String>,
}

/// 上下文预算与自动压缩配置（`[context]` 节）。
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ContextConfig {
    /// 每轮发给模型的历史 token 上限（输入侧）。默认 16384。
    #[garde(range(min = 1024))]
    pub history_token_budget: u32,
    /// 每轮结束后历史超出预算水位时自动滚动摘要压缩。默认 true。
    #[garde(skip)]
    pub auto_compact: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            history_token_budget: 16_384,
            auto_compact: true,
        }
    }
}

/// 保守默认上下文窗口（内置表与手填都缺时兜底）。
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;

/// 按模型名查内置默认上下文窗口（手填缺失时的兜底表）。
///
/// 匹配常见模型名子串；查不到返回 [`DEFAULT_CONTEXT_WINDOW`]。单用户可随时
/// 在 config 显式填 `context_window` 覆盖本表（手填优先）。
pub fn default_context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    // 从具体到通用匹配。
    if m.contains("deepseek") {
        65_536
    } else if m.contains("claude") {
        200_000
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("gpt-4-turbo")
        || m.contains("gpt-4-1106")
    {
        // 注意：本分支必须在下面的裸 "gpt-4" 之前——否则 gpt-4o / gpt-4-turbo
        // 会被 "gpt-4" 抢先命中，错拿 8K 窗口。
        128_000
    } else if m.contains("gpt-4") {
        8_192
    } else if m.contains("gpt-3.5") {
        16_385
    } else if m.contains("kimi") || m.contains("moonshot") {
        128_000
    } else if m.contains("qwen") {
        131_072
    } else if m.contains("gemini") {
        1_000_000
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

impl ModelConfig {
    /// 生效的上下文窗口：手填优先，否则查内置默认表。
    pub fn effective_context_window(&self) -> u32 {
        self.context_window
            .unwrap_or_else(|| default_context_window(&self.model))
    }

    /// 生效的单轮输出上限：手填优先，否则 `min(窗口, 8192)`。
    ///
    /// 对齐 openclaw 的兜底公式（provider-catalog-live-normalize.internal.ts
    /// `?? Math.min(contextWindow, 8192)`）。它那边前面还有两层——打 provider
    /// 的 /models 接口在线发现、内置各家静态目录——我们没有，所以这条公式就是
    /// 主路径。
    ///
    /// **必须有个值，不能留空**：留空等于把上限交给服务端默认，而那个值往往
    /// 很小（方舟 doubao 系列 4k），模型写稍长的脚本就被砍在半个 JSON 处。
    /// 8192 填大了也不怕——[`Self::clamped_max_output_tokens`] 会压回窗口内，
    /// 且服务端对超过自身上限的请求普遍是截断而非报错。
    pub fn effective_max_output_tokens(&self) -> u32 {
        self.max_output_tokens
            .unwrap_or_else(|| self.effective_context_window().min(DEFAULT_MAX_OUTPUT_TOKENS))
    }

    /// 上一项再夹到上下文窗口内（手填过大时兜底）。
    ///
    /// 输出上限大于整个窗口没有意义，个别 provider 还会因此 400。
    pub fn clamped_max_output_tokens(&self) -> u32 {
        self.effective_max_output_tokens()
            .min(self.effective_context_window())
            .max(1)
    }
}

/// 未配置时的输出上限兜底值（与 openclaw 同一个数）。
///
/// 取 8192 而非更小：截断的成因就是预算不够，调小只会更早撞上。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
}

/// 托管方式，决定空闲看门狗阈值（cloud 120s / self 300s，见 §10.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hosting {
    Cloud,
    #[serde(rename = "self")]
    SelfHosted,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct MemoryConfig {
    #[garde(skip)]
    pub vec: bool,
    #[garde(range(min = 1))]
    pub halflife_days: u32,
    #[garde(range(min = 0.0, max = 1.0))]
    pub trigger_threshold: f32,
    #[garde(range(min = 1))]
    pub trigger_max_per_turn: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ProactiveConfig {
    #[garde(range(min = 1))]
    pub heartbeat_secs: u64,
    #[garde(range(min = 0))]
    pub intent_cooldown_secs: u64,
    #[garde(range(min = 1))]
    pub intent_budget: u32,
    #[garde(range(min = 1))]
    pub intent_expiry_days: u32,
    /// standing intent 每轮最多注入条数（设计 §12.5 ≤3）。
    ///
    /// 与上面三项的分工：cooldown/budget/expiry 是**每条待办自己的**参数（落库在
    /// standing_intent 行上，上面三项仅作新建时的默认值）；本项是**每轮全局**上限，
    /// 防止一条消息同时命中多条待办时把上下文塞满。
    #[garde(range(min = 1))]
    #[serde(default = "default_intent_max_per_turn")]
    pub intent_max_per_turn: u32,
}

/// `intent_max_per_turn` 的 serde 默认（老配置文件缺该键时用）。
fn default_intent_max_per_turn() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ToolsConfig {
    #[garde(range(min = 1))]
    pub exec_timeout_secs: u64,
    #[garde(dive)]
    pub approval: ApprovalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ApprovalConfig {
    #[garde(skip)]
    pub mode: ApprovalMode,
    /// 等待用户审批回执的上限（秒）；超时按**拒绝**处理。`0` = 不超时。
    ///
    /// `#[serde(default)]` 是必需的：现网 `~/.oc/config.toml` 都没有这一项，
    /// 缺省必须能加载，否则升级即打断所有已有配置。
    #[serde(default = "default_approval_timeout_secs")]
    #[garde(skip)]
    pub timeout_secs: u64,
}

/// 审批等待上限默认 120s。无人值守场景（cron / HTTP 网关）没有 TUI 响应审批，
/// 无上限会让 run 占着车道直到卡死诊断兜底（默认 360s，且语义是「run 病了」）。
fn default_approval_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Prompt,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct WatchdogConfig {
    #[garde(range(min = 1))]
    pub idle_cloud_secs: u64,
    #[garde(range(min = 1))]
    pub idle_self_secs: u64,
    /// run 墙钟上限，0 = 无限。
    #[garde(skip)]
    pub run_timeout_secs: u64,
    /// 卡死 abort 下限（见 §10.1，默认 300）。
    #[garde(range(min = 1))]
    pub abort_min_secs: u64,
}

/// 技能门控配置（ROAD-1）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate)]
pub struct SkillsConfig {
    /// 非空则只加载列表内的技能名。
    #[garde(skip)]
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// 永不加载（优先于 allowlist）。
    #[garde(skip)]
    #[serde(default)]
    pub denylist: Vec<String>,
}

/// Secret 引用三态（inline/env/file）。解引用是 IO，在 server 做。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRef {
    Inline(String),
    Env(String),
    File(PathBuf),
}

impl Config {
    /// 校验配置形状（garde）。SecretRef 的解引用不在此处。
    pub fn validate_shape(&self) -> Result<(), garde::Report> {
        Validate::validate(self)
    }

    /// 单用户本地默认配置。
    pub fn default_local() -> Self {
        Self {
            proto_version: 1,
            server: ServerConfig {
                transport: if cfg!(windows) {
                    Transport::Pipe
                } else {
                    Transport::Unix
                },
            },
            models: vec![ModelConfig {
                alias: "default".to_string(),
                provider: Provider::Anthropic,
                model: "claude-opus-4-8".to_string(),
                hosting: Hosting::Cloud,
                api_key: SecretRef::Env("ANTHROPIC_API_KEY".to_string()),
                base_url: None,
                context_window: None,
                max_output_tokens: None,
                max_tokens_field: None,
            }],
            context: ContextConfig::default(),
            memory: MemoryConfig {
                vec: true,
                halflife_days: 30,
                trigger_threshold: 0.72,
                trigger_max_per_turn: 3,
            },
            proactive: ProactiveConfig {
                heartbeat_secs: 60,
                intent_cooldown_secs: 86_400,
                intent_budget: 3,
                intent_expiry_days: 90,
                intent_max_per_turn: 3,
            },
            tools: ToolsConfig {
                exec_timeout_secs: 120,
                approval: ApprovalConfig {
                    mode: ApprovalMode::Prompt,
                    timeout_secs: default_approval_timeout_secs(),
                },
            },
            watchdog: WatchdogConfig {
                idle_cloud_secs: 120,
                idle_self_secs: 300,
                run_timeout_secs: 0,
                abort_min_secs: 300,
            },
            skills: SkillsConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_local_is_valid() {
        let cfg = Config::default_local();
        assert!(cfg.validate_shape().is_ok(), "default config must validate");
    }

    /// 现网 `~/.oc/config.toml` 里没有 `timeout_secs`（该项后加的）。缺省必须
    /// 能加载并取到默认值，否则升级会打断所有已有配置。
    #[test]
    fn approval_timeout_defaults_when_absent() {
        let cfg: ApprovalConfig = toml::from_str(r#"mode = "prompt""#).expect("旧配置应能加载");
        assert_eq!(cfg.mode, ApprovalMode::Prompt);
        assert_eq!(cfg.timeout_secs, 120, "缺省应取默认 120s，而非 0（0 = 不超时）");
    }

    #[test]
    fn approval_timeout_explicit_wins() {
        let cfg: ApprovalConfig =
            toml::from_str("mode = \"prompt\"\ntimeout_secs = 5").expect("显式值应能加载");
        assert_eq!(cfg.timeout_secs, 5);
    }

    #[test]
    fn rejects_out_of_range_threshold() {
        let mut cfg = Config::default_local();
        cfg.memory.trigger_threshold = 1.5;
        assert!(cfg.validate_shape().is_err());
    }

    #[test]
    fn rejects_empty_models() {
        let mut cfg = Config::default_local();
        cfg.models.clear();
        assert!(cfg.validate_shape().is_err());
    }

    #[test]
    fn default_context_window_table() {
        assert_eq!(default_context_window("deepseek-chat"), 65_536);
        assert_eq!(default_context_window("deepseek-reasoner"), 65_536);
        assert_eq!(default_context_window("claude-opus-4-8"), 200_000);
        assert_eq!(default_context_window("gpt-4o"), 128_000);
        assert_eq!(default_context_window("gemini-2.0-flash"), 1_000_000);
        // 未知模型 → 保守默认。
        assert_eq!(default_context_window("some-unknown-model"), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn effective_window_prefers_explicit() {
        let mut m = Config::default_local().models.remove(0);
        m.model = "deepseek-chat".to_string();
        // 未填 → 查表得 65536。
        m.context_window = None;
        assert_eq!(m.effective_context_window(), 65_536);
        // 手填 → 优先。
        m.context_window = Some(100_000);
        assert_eq!(m.effective_context_window(), 100_000);
    }

    /// 输出上限的零配置推导：`min(窗口, 8192)`，手填优先。
    ///
    /// 关键是**未配置时也必须有值**。留空等于把上限交给服务端默认，而那个值
    /// 往往只有 4k（方舟 doubao 系列），模型写稍长的脚本就被砍在半个 JSON 处，
    /// 工具压根没执行。
    #[test]
    fn max_output_tokens_derives_from_window() {
        let mut m = Config::default_local().models.remove(0);
        m.max_output_tokens = None;

        // 大窗口 → 取 8192 上限（对齐 openclaw 的 min(contextWindow, 8192)）。
        m.context_window = Some(200_000);
        assert_eq!(m.effective_max_output_tokens(), DEFAULT_MAX_OUTPUT_TOKENS);

        // 小窗口 → 不超过窗口本身。
        m.context_window = Some(4_096);
        assert_eq!(m.effective_max_output_tokens(), 4_096);

        // 手填优先。
        m.max_output_tokens = Some(32_768);
        m.context_window = Some(200_000);
        assert_eq!(m.effective_max_output_tokens(), 32_768);
    }

    /// 手填超过窗口时夹回去：输出上限大于整个窗口没有意义，个别 provider 会 400。
    #[test]
    fn max_output_tokens_clamped_to_window() {
        let mut m = Config::default_local().models.remove(0);
        m.context_window = Some(8_192);
        m.max_output_tokens = Some(200_000);
        assert_eq!(m.clamped_max_output_tokens(), 8_192);
    }

    /// `intent_max_per_turn` 缺失时回落到默认 3（设计 §12.5 ≤3）。
    ///
    /// P1-2 新增了该键；已存在的 config.toml 里没有它，若无 serde default 会解析
    /// 失败、daemon 起不来。TOML 层面的端到端回归在 oc-cli::config_loader（那里才
    /// 有 toml 依赖与真实加载路径），此处只锁默认值本身。
    #[test]
    fn intent_max_per_turn_defaults_to_three() {
        assert_eq!(default_intent_max_per_turn(), 3);
        assert_eq!(Config::default_local().proactive.intent_max_per_turn, 3);
    }

    /// 现网 config.toml 没有 [skills] 节，缺省必须能加载。
    #[test]
    fn skills_section_defaults_when_absent() {
        let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
[memory]
vec = true
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3
[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#).expect("旧配置应能加载");
        assert!(cfg.skills.allowlist.is_empty());
        assert!(cfg.skills.denylist.is_empty());
    }

    /// 现网 config.toml 没有 [context] 节，缺省必须拿到默认预算 + 自动压缩开。
    #[test]
    fn context_section_defaults_when_absent() {
        let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
[memory]
vec = true
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3
[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#).expect("无 [context] 节也应能加载");
        assert_eq!(cfg.context.history_token_budget, 16_384, "缺省预算应为 16K");
        assert!(cfg.context.auto_compact, "缺省应开启自动压缩");
        assert!(cfg.validate_shape().is_ok());
    }

    #[test]
    fn context_explicit_wins() {
        let cfg: Config = toml::from_str(r#"
proto_version = 1
[server]
transport = "pipe"
[[models]]
alias = "default"
provider = "openai"
model = "m"
hosting = "cloud"
api_key = { env = "K" }
[context]
history_token_budget = 8192
auto_compact = false
[memory]
vec = true
halflife_days = 30
trigger_threshold = 0.72
trigger_max_per_turn = 3
[proactive]
heartbeat_secs = 60
intent_cooldown_secs = 86400
intent_budget = 3
intent_expiry_days = 90
[tools]
exec_timeout_secs = 120
[tools.approval]
mode = "prompt"
[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#).expect("显式 [context] 应能解析");
        assert_eq!(cfg.context.history_token_budget, 8192);
        assert!(!cfg.context.auto_compact);
    }

    #[test]
    fn context_budget_below_min_rejected() {
        let mut cfg = Config::default_local();
        cfg.context.history_token_budget = 512;
        assert!(cfg.validate_shape().is_err(), "预算低于 1024 应校验失败");
    }
}
