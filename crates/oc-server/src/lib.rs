//! oc 常驻进程（设计 §7）。
//!
//! M3 落地：agent 循环（经 oc-core 状态机 + oc-llm 流）、空闲看门狗 /
//! run 超时 / abort、心跳 tick 底座。`chat.send` 走真正的模型轮。

pub mod codec;
pub mod command;
pub mod conn;
pub mod diag;
pub mod dispatch;
pub mod dreaming;
pub mod error;
pub mod ledger;
pub mod proactive;
pub mod registry;
pub mod run;
pub mod run_log;
pub mod scheduler;
pub mod session;
pub mod sink;
pub mod state;
pub mod summarize;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod tools_bridge;
pub mod transport;

pub use error::{ServerError, ServerResult};
pub use session::{IntentDefaults, SessionConfig};
pub use state::ServerState;
pub use transport::TransportKind;

use std::sync::Arc;
use std::time::Duration;

use oc_llm::Provider;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

const EVENT_CHANNEL_CAP: usize = 256;

/// 每多少个心跳 tick 触发一轮 dreaming 巩固（稀疏，避免频繁重写）。
const DREAM_EVERY_TICKS: u64 = 60;

/// 启动服务端。
///
/// `provider`：模型 provider（真实或 mock）。`session_cfg`：会话/防卡死参数。
pub async fn serve_with(
    kind: TransportKind,
    provider: Arc<dyn Provider>,
    session_cfg: SessionConfig,
    heartbeat_interval: Duration,
    store: oc_store::Store,
) -> ServerResult<()> {
    let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);

    // 审批注册表：ToolExecutor（发起审批）与 ServerState（回执唤醒）共享。
    let approvals: state::ApprovalRegistry = Arc::new(dashmap::DashMap::new());
    // 用户输入注册表（ask_user）：同构，ToolExecutor 发起、ServerState 回执唤醒。
    let inputs: state::InputRegistry = Arc::new(dashmap::DashMap::new());
    // cron 创建请求通道（P1-5）：模型调 cron_add → 经此通道 → proactive 模块处理。
    let (cron_tx, cron_rx) = tokio::sync::mpsc::unbounded_channel();

    // 后台任务台账。
    let ledger = ledger::TaskLedger::new(event_tx.clone());

    // 若配置了工具，共享审批 registry（交互式审批走 per-run sink）+ 把后台移交接到台账。
    let mut session_cfg = session_cfg;
    if let Some(tools) = session_cfg.tools.take() {
        // 后台移交 → 台账登记。
        if let Some(mut rx) = tools.take_handoff() {
            let ledger2 = ledger.clone();
            tokio::spawn(async move {
                while let Some(handoff) = rx.recv().await {
                    ledger2.register(handoff);
                }
            });
        }
        session_cfg.tools =
            Some(tools.with_approvals(Arc::clone(&approvals)).with_inputs(Arc::clone(&inputs)).with_cron(cron_tx.clone()));
    }

    // proactive 上下文：在 provider 被 session 接管前克隆出所需句柄。
    let proactive_ctx = proactive::ProactiveCtx {
        provider: Arc::clone(&provider),
        events: event_tx.clone(),
        store: store.clone(),
        model: session_cfg.model.clone(),
        soul: session_cfg.soul.clone(),
        default_tz: session_cfg.default_tz.clone(),
    };

    let context_window = session_cfg.context_window;
    // 模型运行时信息：provider/endpoint 取自 provider 实例本身（而非配置的 base_url，
    // 那里 None 表示「用官方默认」，取用时得重复一遍默认值）。供 `oc status` 展示，
    // 换 provider 后不必开对话就能验证生效。
    let runtime = state::RuntimeInfo {
        provider: provider.id().to_string(),
        model: session_cfg.model.clone(),
        endpoint: provider.endpoint().map(str::to_string),
        context_window,
    };

    // standing intent 的 anti-nagging 默认值：供 `intent.add` 未指定时填充。
    let intent_defaults = session_cfg.intent_defaults.clone();
    // dreaming 巩固模型轮要用的 soul 目录与模型名——须在 session_cfg/provider
    // 被 registry 取走之前克隆出来。
    let soul_dir = session_cfg.soul_dir.clone();
    let dream_model = session_cfg.model.clone();
    // 诊断注册表：registry 派发会话级句柄给各 actor，state 侧供 diagnostics 采样。
    let diag = diag::DiagRegistry::new();
    let registry = registry::SessionRegistry::new(
        session_cfg,
        Arc::clone(&provider),
        event_tx.clone(),
        store.clone(),
        diag.clone(),
    );
    let dream_store = store.clone();
    let state = Arc::new(ServerState::new(
        event_tx,
        registry,
        approvals,
        inputs,
        ledger,
        store,
        runtime,
        diag,
        intent_defaults,
    ));

    // P1-5：消费 cron 工具请求（add/delay/list/rm），委托 proactive 处理。
    //
    // 单独一个 task 顺序消费：cron 操作都是毫秒级本地库写，不必并发；串行还顺带
    // 保证了「同一时刻只有一个 cron 写入」，与写线程的单写者语义一致。
    let cron_pctx = proactive_ctx.clone();
    tokio::spawn(async move {
        let mut rx = cron_rx;
        while let Some(req) = rx.recv().await {
            let oc_tools::types::CronRequest { op, reply } = req;
            let result = proactive::handle_cron_op(&cron_pctx, op).await;
            let _ = reply.send(result);
        }
    });

    // 订阅 Usage 事件，更新每会话最近用量（供 status 查询）。
    let usage_state = Arc::clone(&state);
    let mut usage_rx = usage_state.subscribe();
    tokio::spawn(async move {
        loop {
            match usage_rx.recv().await {
                Ok(oc_proto::Event::Usage { session, input_tokens, .. }) => {
                    usage_state.set_last_input_tokens(session, input_tokens);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    });

    // 心跳 tick：每 tick 卡死诊断扫描 + 内存 GC + cron 到期扫描；
    // 每 DREAM_EVERY_TICKS 一轮 dreaming。
    let shutdown = CancellationToken::new();
    // GC 与卡死扫描都挂在这里，而不是各起一个 tokio 定时任务：本 tick 已有关停
    // 接线（`shutdown` token），且两者都是「扫一遍、都很便宜」的周期活儿。
    let tick_state = Arc::clone(&state);
    // 巩固模型轮上下文：仅当配了 soul_dir（有落盘位置）才启用重写 MEMORY.md。
    let dream_ctx = soul_dir.map(|dir| dreaming::ConsolidateCtx {
        provider: Arc::clone(&provider),
        model: dream_model,
        soul_dir: dir,
    });
    scheduler::Heartbeat::new(heartbeat_interval).spawn(shutdown.clone(), move |tick| {
        let tick_state = Arc::clone(&tick_state);
        let store = dream_store.clone();
        let pctx = proactive_ctx.clone();
        let dctx = dream_ctx.clone();
        async move {
            tracing::debug!(tick, "heartbeat：扫描");
            tick_state.registry().health_scan_all().await;
            // 内存 GC（P2-3）：过期幂等键 + 空闲会话 actor（连带其诊断/用量格位）。
            tick_state.gc_tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // cron 到期触发（失败不阻塞）。
            proactive::cron_scan(&pctx, now).await;
            // dreaming 巩固：稀疏触发（设计 §7.4 夜间/空闲；M5 先按 tick 周期）。
            // 配了 soul_dir 则额外跑巩固模型轮重写 MEMORY.md（§11.4）。
            if tick % DREAM_EVERY_TICKS == 0 {
                dreaming::scan_with(
                    &store,
                    now,
                    &oc_core::dreaming::DreamCfg::default(),
                    dctx.as_ref(),
                )
                .await;
            }
        }
    });

    let mut listener = transport::Listener::bind(&kind)?;
    info!(?kind, "oc-server 监听中");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = accepted?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = conn::handle(stream, state).await {
                        tracing::warn!(error = %e, "连接处理结束");
                    }
                });
            }
            _ = shutdown_signal() => {
                info!("收到关停信号，退出");
                shutdown.cancel();
                break;
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
