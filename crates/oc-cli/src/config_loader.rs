//! 加载 `~/.oc/config.toml`（设计 §13.2）。
//!
//! 文件存在则解析并校验；不存在则用默认配置，并**不**自动写盘（由 `oc onboard`
//! 或用户手动创建）。校验失败直接报错，避免带病启动。

use anyhow::{Context, Result};
use oc_core::Config;

use crate::paths;

/// 加载配置。文件缺失时回退默认配置（内含 mock/anthropic 占位）。
pub fn load() -> Result<Config> {
    let path = paths::config_path()?;
    if !path.exists() {
        eprintln!(
            "[info] 未找到 {}，使用默认配置。可参考 config.example.toml 创建。",
            path.display()
        );
        return Ok(Config::default_local());
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let cfg: Config = toml::from_str(&text)
        .with_context(|| format!("解析 {} 失败（TOML 格式错误）", path.display()))?;
    cfg.validate_shape()
        .map_err(|report| anyhow::anyhow!("配置校验失败:\n{report}"))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use oc_core::Config;

    /// 老配置文件（`[proactive]` 缺 `intent_max_per_turn`）仍应解析成功并取默认值。
    ///
    /// P1-2 给 `ProactiveConfig` 新增了该键；用户机上已存在的 config.toml 里没有它，
    /// 若无 `#[serde(default)]` 兜底，升级后 `load()` 直接报错、daemon 起不来。
    /// 这里走的就是 `load()` 的解析+校验两步，只是绕开文件 IO。
    #[test]
    fn old_config_without_intent_max_per_turn_still_loads() {
        // 刻意省略 intent_max_per_turn（模拟 P1-2 之前 onboard 生成的配置）。
        let toml_text = r#"proto_version = 1

[server]
transport = "pipe"

[[models]]
alias = "default"
provider = "openai"
model = "deepseek-chat"
hosting = "cloud"
base_url = "https://api.deepseek.com/v1"
api_key = { env = "DEEPSEEK_API_KEY" }

[memory]
vec = false
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
timeout_secs = 120

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#;
        let cfg: Config = toml::from_str(toml_text).expect("缺新键的老配置应仍可解析");
        assert_eq!(
            cfg.proactive.intent_max_per_turn, 3,
            "缺键应回落到 serde 默认 3（设计 §12.5 ≤3）"
        );
        assert!(cfg.validate_shape().is_ok(), "回落后的配置应通过校验");

        // 同一份老配置也没有 max_output_tokens / max_tokens_field：
        // 它们必须回落到「自动推导」，而不是解析失败。
        let m = &cfg.models[0];
        assert_eq!(m.max_output_tokens, None);
        assert_eq!(m.max_tokens_field, None);
        // 未配置也要推出一个值——留空等于把上限交给服务端默认（常见 4k），
        // 模型写稍长的脚本就被截断。
        assert_eq!(m.clamped_max_output_tokens(), 8_192);
    }

    /// 新增的两个模型键能被真实解析路径接受（示例配置里就是这么写的）。
    #[test]
    fn model_output_limit_keys_parse() {
        let toml_text = r#"proto_version = 1

[server]
transport = "pipe"

[[models]]
alias = "default"
provider = "openai"
model = "doubao-seed-evolving"
hosting = "cloud"
base_url = "https://ark.cn-beijing.volces.com/api/v3"
api_key = { env = "ARK_API_KEY" }
context_window = 32768
max_output_tokens = 16384
max_tokens_field = "max_completion_tokens"

[memory]
vec = false
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
timeout_secs = 120

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#;
        let cfg: Config = toml::from_str(toml_text).expect("新键应可解析");
        let m = &cfg.models[0];
        assert_eq!(m.max_output_tokens, Some(16_384));
        assert_eq!(m.max_tokens_field.as_deref(), Some("max_completion_tokens"));
        // 手填 16384 未超窗口 32768，原样生效。
        assert_eq!(m.clamped_max_output_tokens(), 16_384);
        assert!(cfg.validate_shape().is_ok());
    }

    /// 新增的 `[context]` 节能被真实解析路径接受（示例配置里就是这么写的）。
    #[test]
    fn context_section_parses() {
        let toml_text = r#"proto_version = 1

[server]
transport = "pipe"

[[models]]
alias = "default"
provider = "openai"
model = "deepseek-chat"
hosting = "cloud"
base_url = "https://api.deepseek.com"
api_key = { env = "DEEPSEEK_API_KEY" }

[context]
history_token_budget = 16384
auto_compact = true

[memory]
vec = false
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
timeout_secs = 120

[watchdog]
idle_cloud_secs = 120
idle_self_secs = 300
run_timeout_secs = 0
abort_min_secs = 300
"#;
        let cfg: Config = toml::from_str(toml_text).expect("含 [context] 的配置应可解析");
        assert_eq!(cfg.context.history_token_budget, 16_384);
        assert!(cfg.context.auto_compact);
        assert!(cfg.validate_shape().is_ok());
    }
}
