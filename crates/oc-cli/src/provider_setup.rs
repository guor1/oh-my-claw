//! 从 Config 组装 provider + 会话参数（设计 §4.8 SecretRef 解引用在 server 侧）。
//!
//! M3：解引用第一个模型的 api_key；无 key 则回退到 mock provider（离线可用，
//! 便于本地演示与测试）。真实 provider 在有 key 时启用。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use oc_core::config::{ApprovalMode as CfgApprovalMode, Config, Hosting, Provider as ProviderKind, SecretRef};
use oc_core::tool::ApprovalMode;
use oc_llm::mock::MockProvider;
use oc_llm::Provider;
use oc_server::tools_bridge::ToolExecutor;
use oc_server::SessionConfig;
use oc_tools::exec::ExecTool;
use oc_tools::file::FileTool;
use oc_tools::shell::Shell;
use oc_tools::sys::SysTool;
use oc_tools::ToolRegistry;

/// 返回 (provider, 会话配置, 心跳间隔)。
pub fn build(cfg: &Config) -> Result<(Arc<dyn Provider>, SessionConfig, Duration)> {
    let model = cfg
        .models
        .first()
        .ok_or_else(|| anyhow::anyhow!("配置中没有模型"))?;

    // 空闲看门狗阈值据 hosting 区分。
    let idle = match model.hosting {
        Hosting::Cloud => cfg.watchdog.idle_cloud_secs,
        Hosting::SelfHosted => cfg.watchdog.idle_self_secs,
    };

    // 上下文窗口 → 历史预算：请求值夹进 (MIN, window−reserve) 区间。
    let context_window = model.effective_context_window() as i64;
    let budget = (cfg.context.history_token_budget as i64)
        .min(context_window - oc_core::compaction::DEFAULT_RESERVE_TOKENS)
        .max(oc_core::compaction::MIN_BUDGET_TOKENS);

    // 组装工具注册表（exec + file）。
    let tools = build_tools(cfg)?;

    let session_cfg = SessionConfig {
        model: model.model.clone(),
        // system_prompt 已由 oc-core::prompt 每轮组装（见 soul 字段），此处保留兼容。
        system_prompt: None,
        idle_timeout: Duration::from_secs(idle),
        run_timeout: if cfg.watchdog.run_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(cfg.watchdog.run_timeout_secs))
        },
        queue_cap: 16,
        tools: Some(tools),
        warn_secs: idle, // 警告阈值取空闲看门狗阈值
        abort_min_secs: cfg.watchdog.abort_min_secs,
        max_history_entries: 200,
        history_token_budget: budget,
        auto_compact: cfg.context.auto_compact,
        soul: load_soul(),
        skills: crate::skills_loader::load(&cfg.skills, std::env::consts::OS),
        trigger_threshold: cfg.memory.trigger_threshold as f64,
        trigger_max_per_turn: cfg.memory.trigger_max_per_turn as usize,
        context_window: context_window as u32,
        // 单轮输出上限：手填优先，否则 min(窗口, 8192)。**总是有值**——留空
        // 等于把上限交给服务端默认（方舟 doubao 4k），写稍长的脚本就被截断。
        max_output_tokens: Some(model.clamped_max_output_tokens()),
        // soul 目录（设计 §13.1）：dreaming 巩固轮据此重写 MEMORY.md。
        // 取不到 OC_HOME 时为 None——server 会跳过文件重写，只做 DB 内巩固。
        soul_dir: crate::paths::oc_home().ok().map(|h| h.join("soul")),
        // 本机时区（P1-5）：cron 未指定 tz 时的默认值。
        default_tz: crate::tz::local_tz(),
        // standing intent anti-nagging（设计 §12.5）：配置驱动，接上此前的死键。
        intent_defaults: oc_server::IntentDefaults {
            cooldown_secs: cfg.proactive.intent_cooldown_secs as i64,
            budget: cfg.proactive.intent_budget,
            expiry_days: cfg.proactive.intent_expiry_days,
            max_per_turn: cfg.proactive.intent_max_per_turn as usize,
        },
    };

    // 解引用 api_key。
    let key = resolve_secret(&model.api_key);

    let provider: Arc<dyn Provider> = match key {
        Some(k) if !k.is_empty() => make_real(
            model.provider,
            k,
            model.base_url.clone(),
            model.max_tokens_field.as_deref(),
        ),
        _ => {
            eprintln!("[warn] 未找到 API key，回退到 mock provider（离线演示）");
            Arc::new(MockProvider::echo_text(
                "（mock 回复）你好，我是 oc。配置 API key 后可接入真实模型。",
            ))
        }
    };

    Ok((provider, session_cfg, Duration::from_secs(cfg.proactive.heartbeat_secs)))
}

/// 加载 SOUL.md 人格（设计 §13.1：`~/.oc/soul/SOUL.md`）。
///
/// 文件缺失返回空串，由 run driver 用内置默认人格兜底。
fn load_soul() -> String {
    let Ok(home) = crate::paths::oc_home() else {
        return String::new();
    };
    let path = home.join("soul").join("SOUL.md");
    std::fs::read_to_string(&path).unwrap_or_default()
}

/// 组装工具注册表：exec（审批门由 config 派生）+ file（限当前目录 + OC_HOME）。
fn build_tools(cfg: &Config) -> Result<ToolExecutor> {
    let mode = match cfg.tools.approval.mode {
        CfgApprovalMode::Prompt => ApprovalMode::Prompt,
        CfgApprovalMode::Allow => ApprovalMode::Allow,
        CfgApprovalMode::Deny => ApprovalMode::Deny,
    };
    let exec_timeout = Duration::from_secs(cfg.tools.exec_timeout_secs);
    let approval_timeout = Duration::from_secs(cfg.tools.approval.timeout_secs);

    // Windows 上执行 shell 用 Git Bash；探测失败即启动失败（硬依赖，不回退 cmd）。
    let shell = Shell::resolve().map_err(|e| anyhow::anyhow!("{e}"))?;

    // file/sys 允许根：`~/.oc/workspace` + OC_HOME。
    //
    // 曾经第一项是 `std::env::current_dir()`，于是允许根等于 daemon 的启动目录：
    // 在 `/root` 或仓库里 `oc serve`，模型就拿到了那整棵树的读写权（含 `.ssh/`）。
    // 现在固定在 OC_HOME 下，与启动位置无关。
    // OC_HOME 自身也在内——soul 自我编辑要写 `~/.oc/soul/`。
    let mut roots = Vec::new();
    let ws = crate::paths::workspace()?;
    // 允许根必须真实存在：path_guard 用 canonicalize 比前缀，目录不存在则
    // 任何路径都判不进根，file 工具会整体失效。
    std::fs::create_dir_all(&ws)
        .with_context(|| format!("创建工作区 {} 失败", ws.display()))?;
    roots.push(ws.clone());
    if let Ok(home) = crate::paths::oc_home() {
        roots.push(home);
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ExecTool::new(mode, exec_timeout, approval_timeout, shell.clone())));
    registry.register(Arc::new(FileTool::new(roots.clone())));
    // sys：pwd/cd/now（cd 受同一组 allowed_roots 约束）。
    // 传本机时区：now 要与系统提示词的「当前时间」同口径，否则模型拿两个格式对账。
    registry.register(Arc::new(SysTool::new(roots, crate::tz::local_tz())));

    // process 工具：后台移交 channel，接口另一端在 serve_with 接到台账。
    let (handoff_tx, handoff_rx) = tokio::sync::mpsc::unbounded_channel();
    registry.register(Arc::new(oc_tools::process::ProcessTool::new(handoff_tx, shell)));

    // message：主动通知用户（不等回复）。
    registry.register(Arc::new(oc_tools::message::MessageTool));

    // ask_user：主动向用户提问并阻塞等待回答（交互式输入门经 server 注入）。
    registry.register(Arc::new(oc_tools::ask_user::AskUserTool));

    // cron：定时/延时提醒的增删查（P1-5；cron 门经 server 注入）。
    registry.register(Arc::new(oc_tools::cron::CronTool));

    // web_fetch / web_search：联网（需 web-tools feature，默认开）。
    #[cfg(feature = "web-tools")]
    {
        registry.register(Arc::new(oc_tools::web::WebFetchTool));
        registry.register(Arc::new(oc_tools::web::WebSearchTool));
    }

    Ok(ToolExecutor::new(Arc::new(registry))
        .with_handoff(handoff_rx)
        // 会话 cwd 初值 = 工作区。不给的话 executor 退回进程 current_dir，
        // 那 `sys pwd` 会报一个不在允许根里的目录，模型据此拼的相对路径全被拒。
        .with_initial_cwd(ws))
}

fn resolve_secret(s: &SecretRef) -> Option<String> {
    match s {
        SecretRef::Inline(v) => Some(v.clone()),
        SecretRef::Env(name) => std::env::var(name).ok(),
        SecretRef::File(path) => std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()),
    }
}

fn make_real(
    kind: ProviderKind,
    key: String,
    base_url: Option<String>,
    max_tokens_field: Option<&str>,
) -> Arc<dyn Provider> {
    match kind {
        #[cfg(feature = "provider-openai")]
        ProviderKind::Openai => {
            let mut p = oc_llm::openai::OpenAiProvider::new(key, base_url);
            if let Some(f) = max_tokens_field {
                p = p.with_max_tokens_field(f);
            }
            Arc::new(p)
        }
        #[cfg(feature = "provider-anthropic")]
        ProviderKind::Anthropic => Arc::new(oc_llm::anthropic::AnthropicProvider::new(key, base_url)),
        // 未启用对应 feature 时回退 mock（不 panic）。
        #[allow(unreachable_patterns)]
        _ => {
            let _ = (key, base_url);
            Arc::new(MockProvider::echo_text("（provider feature 未启用，mock 回复）"))
        }
    }
}
