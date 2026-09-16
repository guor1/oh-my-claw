//! 请求分发（设计 §7.2）。
//!
//! M2：处理 req → 返回 res，必要时广播 event。无 agent loop，`chat.send`
//! 以 **echo** 演示完整事件流（lifecycle → assistant delta → lifecycle end）。

use std::sync::Arc;

use oc_proto::{
    ChatAbortParams, ChatSendParams, ClientKind, ConnectParams, Features, Frame, Method, MethodOk,
    ProtoError, Req, ResResult, SessionId, Snapshot, PROTO_VERSION,
};
use tokio::sync::mpsc;

use crate::sink::RunSink;
use crate::state::ServerState;

/// 处理一个请求，返回应答载荷。副作用（事件广播）在此内部完成。
///
/// `out_tx`：本连接的出站帧队列——`chat.send` 用它构造 per-run sink，
/// 让本轮内联事件（文本/工具/审批）背压式定向回发到这条连接（P0-1）。
pub async fn handle_req(
    req: &Req,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
    client_kind: ClientKind,
) -> ResResult {
    // 幂等：side-effecting 方法命中缓存直接返回首个结果。
    if let Some(key) = &req.idempotency_key {
        if let Some(ok) = state.idem_get(key) {
            return ResResult::Ok(ok);
        }
    }

    let result = match &req.method {
        Method::Connect(p) => handle_connect(p, state),
        Method::ChatSend(p) => handle_chat_send(p, state, out_tx, client_kind).await,
        Method::ChatAbort(p) => handle_chat_abort(p, state).await,
        Method::ApprovalReply(p) => {
            state.resolve_approval(&p.approval_id, p.allow);
            Ok(MethodOk::Empty)
        }
        Method::UserReply(p) => {
            state.resolve_input(&p.input_id, p.text.clone());
            Ok(MethodOk::Empty)
        }
        Method::SessionReset(p) => {
            let session = p.session.clone().unwrap_or_else(SessionId::main);
            match reset_session(state, &session).await {
                Ok(()) => Ok(MethodOk::Empty),
                Err(e) => Err(e),
            }
        }
        Method::Compact(p) => {
            let session = p.session.clone().unwrap_or_else(SessionId::main);
            compact_session(state, &session).await;
            Ok(MethodOk::Empty)
        }
        Method::Command(p) => match crate::command::handle_command(p, state).await {
            Ok(r) => Ok(MethodOk::Command(r)),
            Err(e) => Err(e),
        },
        Method::SessionsList => handle_sessions_list(state).await,
        Method::Diagnostics => handle_diagnostics(state).await,
        Method::Status => Ok(MethodOk::Status(snapshot(state, &SessionId::main()))),
        Method::Health => Ok(MethodOk::Health(oc_proto::HealthOk {
            ok: true,
            db_version: oc_store::migrate::TARGET_VERSION,
        })),
        Method::ChatHistory(p) => {
            let limit = p.limit.unwrap_or(200) as i64;
            let session = p.session.clone().unwrap_or_else(SessionId::main);
            match state.store().load_transcript(session.to_string(), limit).await {
                Ok(entries) => {
                    let out = entries
                        .into_iter()
                        .map(|e| oc_proto::Entry {
                            seq: e.seq,
                            role: match e.role {
                                oc_store::Role::Assistant => oc_proto::Role::Assistant,
                                oc_store::Role::Tool => oc_proto::Role::Tool,
                                oc_store::Role::System => oc_proto::Role::System,
                                oc_store::Role::User => oc_proto::Role::User,
                            },
                            content: e.content,
                            // 透传工具结构：assistant 的 tool_calls（JSON 数组文本）解析成
                            // 规格数组，解析失败降级 None（脏数据不丢整轮历史）。
                            tool_calls: e.tool_calls.as_deref().and_then(|s| {
                                serde_json::from_str::<Vec<oc_proto::ToolCallSpec>>(s).ok()
                            }),
                            tool_call_id: e.tool_call_id,
                            created_at: e.created_at,
                        })
                        .collect();
                    Ok(MethodOk::History(out))
                }
                Err(e) => Err(ProtoError {
                    kind: oc_proto::ErrorKind::Internal,
                    message: format!("加载历史失败: {e}"),
                }),
            }
        }
        Method::TasksList => Ok(MethodOk::Tasks(state.ledger().list())),
        Method::TasksCancel(p) => {
            state.ledger().cancel(&p.task_id);
            Ok(MethodOk::Empty)
        }
        Method::CronAdd(p) => handle_cron_add(p, state).await,
        Method::CronList => handle_cron_list(state).await,
        Method::CronRm(p) => handle_cron_rm(p, state).await,
        Method::IntentAdd(p) => handle_intent_add(p, state).await,
        Method::IntentList => handle_intent_list(state).await,
        Method::IntentRm(p) => handle_intent_rm(p, state).await,
        Method::MemorySearch(p) => handle_memory_search(p, state).await,
    };

    match result {
        Ok(ok) => {
            // 写幂等缓存。
            if let Some(key) = &req.idempotency_key {
                state.idem_put(key.clone(), ok.clone());
            }
            ResResult::Ok(ok)
        }
        Err(e) => ResResult::Err(e),
    }
}

fn handle_connect(p: &ConnectParams, state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    if p.proto_version != PROTO_VERSION {
        return Err(ProtoError {
            kind: oc_proto::ErrorKind::ProtoVersionMismatch,
            message: format!(
                "协议版本不匹配：client={}, server={PROTO_VERSION}",
                p.proto_version
            ),
        });
    }
    Ok(MethodOk::Hello {
        features: Features {
            ws_remote: false, // WS 远程为 Phase 2 feature
            memory_vec: false,
            sandbox: false,
            proto_version: PROTO_VERSION,
        },
        snapshot: snapshot(state, &SessionId::main()),
    })
}

/// M3：提交到主会话车道，返回分配的 run_id；实际处理经 per-run sink 定向推送。
async fn handle_chat_send(
    p: &ChatSendParams,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
    client_kind: ClientKind,
) -> Result<MethodOk, ProtoError> {
    // 缺省路由到 main；未知 id 由 registry 懒创建（隐式建会话）。
    let session = p.session.clone().unwrap_or_else(SessionId::main);
    let handle = state.registry().get_or_spawn(&session);
    // Detached 网关（Web/HTTP）：run 归属会话而非连接，断连不中止；
    // Interactive 客户端：本轮内联事件定向回发到这条连接（背压不丢，P0-1）。
    let sink = match client_kind {
        ClientKind::Detached => RunSink::Detached,
        ClientKind::Interactive => RunSink::Conn(out_tx.clone()),
    };
    let mut result = handle.submit(p.text.clone(), sink.clone()).await;

    // 拿到句柄后、submit 前，该 actor 可能刚被空闲淘汰（P2-3 的 GC 落在这个缝里）。
    // 重取一次即可：`get_or_spawn` 见死句柄会换新 actor。窗口极窄且只发生在
    // 「闲置满 24h 的会话正好此刻被唤醒」，重试一次足够；不重试的话，这条请求会
    // 收到「队列已满」——队列其实空着，只是没人收命令，属误报。
    if result.is_none() && handle.is_closed() {
        let fresh = state.registry().get_or_spawn(&session);
        result = fresh.submit(p.text.clone(), sink).await;
    }

    match result {
        Some(run_id) => Ok(MethodOk::ChatSend { run_id }),
        // 队列已满或 actor 已停。**必须报错而不是回一个 run_id**：该轮不会执行，
        // 也就永不产生 Lifecycle 事件，调用方拿着 id 只会白等（HTTP 侧无超时
        // recv 循环 → 挂死）。`ErrorKind` 无 busy/unavailable 变体，暂用
        // `Internal`，消息里说明是队列满，便于调用方区分。
        None => Err(ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: "会话繁忙：队列已满，请稍后重试".to_string(),
        }),
    }
}

/// M3：中止活跃 run（hard 语义在 M4 完整）。
///
/// abort 不带 session 字段，故对所有活跃会话广播中止请求；各 actor 只中止
/// 匹配 run_id 的活跃 run（run_id 全局唯一），互不影响。
async fn handle_chat_abort(
    p: &ChatAbortParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state.registry().abort_all(p.run_id.clone(), p.hard).await;
    Ok(MethodOk::Empty)
}

/// 重置指定会话：推进上下文起点，transcript 保留。被 `session.reset` 与 `/clear` 共用。
pub(crate) async fn reset_session(
    state: &Arc<ServerState>,
    session: &SessionId,
) -> Result<(), ProtoError> {
    // 推进起点前先沉淀 episodic 候选（设计 §11.5，P1-6）：reset 之后这段对话
    // 不再进入任何提示词，这是它进入长期记忆的最后机会。失败不阻塞 reset。
    crate::session::flush_before_reset(
        state.store(),
        &session.to_string(),
        state.registry().cfg().max_history_entries,
    )
    .await;

    state
        .store()
        .writer()
        .reset_session(session.to_string())
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("重置会话失败: {e}"),
        })?;
    Ok(())
}

/// 手动压缩指定会话（/compact）：触发摘要，立即返回（异步执行）。被 `compact` 与 `/compact` 共用。
pub(crate) async fn compact_session(state: &Arc<ServerState>, session: &SessionId) {
    let handle = state.registry().get_or_spawn(session);
    handle.compact().await;
}

/// 列出所有会话（sessions.list）。
async fn handle_sessions_list(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    let rows = state.store().session_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出会话失败: {e}"),
    })?;
    let out = rows
        .into_iter()
        .map(|s| oc_proto::SessionView {
            id: SessionId::new(s.id),
            kind: s.kind,
            created_at: s.created_at,
            reset_at: s.reset_at,
        })
        .collect();
    Ok(MethodOk::Sessions(out))
}

/// 整机诊断快照（`oc debug`）：会话运行时状态 + 写线程健康 + 订阅者数。
async fn handle_diagnostics(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    // 写线程健康：投一条 no-op 到写队列并等它被执行。
    //
    // 曾经用 `session_list()` 当探针，但 P2-1 把读挪到独立连接池之后，
    // 那条命令根本不再经过写线程——探针会在写线程已死时照样返回 true。
    // 现在用 `writer_ping()`：既确认线程活着，也确认队列在推进
    // （长事务卡住时线程活着但不动，`is_alive()` 单独答不了这一问）。
    let store_writer_alive = state.store().writer_ping().await.is_ok();
    let snap = oc_proto::DiagnosticsSnapshot {
        uptime_secs: state.diag().uptime_secs(),
        sessions: state.diag().snapshot_sessions(),
        store_writer_alive,
        event_subscribers: state.subscriber_count(),
        idem_entries: state.idem_len(),
        sampled_at: now_millis(),
        proto_version: PROTO_VERSION,
    };
    Ok(MethodOk::Diagnostics(snap))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 新增 cron：校验表达式（core::next_fire）→ 算首次触发 → 落库。
///
/// 表达式按 `p.tz` 解释（P1-5）；tz 非法一并按 BadRequest 回绝，不静默按 UTC 落库。
async fn handle_cron_add(
    p: &oc_proto::CronAddParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let now = now_secs();
    // 校验 + 算首次触发。
    let next_at = match oc_core::proactive::next_fire(&p.expr, now, &p.tz) {
        Ok(n) => n,
        Err(e) => {
            return Err(ProtoError {
                kind: oc_proto::ErrorKind::BadRequest,
                message: format!("cron 表达式非法: {e}"),
            })
        }
    };
    let id = format!("cron-{}", uuid::Uuid::now_v7());
    let cron = oc_store::NewCron {
        id: id.clone(),
        expr: p.expr.clone(),
        prompt: p.prompt.clone(),
        tz: p.tz.clone(),
        next_at,
    };
    state.store().writer().cron_add(cron).await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("新增 cron 失败: {e}"),
    })?;
    Ok(MethodOk::CronAdd { cron_id: oc_proto::CronId::new(id) })
}

async fn handle_cron_list(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    let rows = state.store().cron_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出 cron 失败: {e}"),
    })?;
    let out = rows
        .into_iter()
        .map(|c| oc_proto::CronSpec {
            id: oc_proto::CronId::new(c.id),
            expr: c.expr,
            prompt: c.prompt,
            tz: c.tz,
            enabled: c.enabled,
            next_at: c.next_at,
        })
        .collect();
    Ok(MethodOk::CronList(out))
}

async fn handle_cron_rm(
    p: &oc_proto::CronRmParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state
        .store()
        .writer()
        .cron_rm(p.cron_id.to_string())
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("删除 cron 失败: {e}"),
        })?;
    Ok(MethodOk::Empty)
}

/// 新增 standing intent（话题触发式待办）：校验 → 算 expiry_at → 落库。
///
/// anti-nagging 三项未指定时取配置 `[proactive]` 的默认值（见 `ServerState::intent_defaults`）。
async fn handle_intent_add(
    p: &oc_proto::IntentAddParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    let text = p.text.trim();
    if text.is_empty() {
        return Err(ProtoError {
            kind: oc_proto::ErrorKind::BadRequest,
            message: "intent text 不能为空".into(),
        });
    }
    let keywords: Vec<String> = p
        .keywords
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if keywords.is_empty() {
        return Err(ProtoError {
            kind: oc_proto::ErrorKind::BadRequest,
            message: "至少需一个非空 keyword（话题触发靠关键词命中）".into(),
        });
    }

    let defaults = state.intent_defaults();
    let now = now_secs();
    let expiry_days = p.expiry_days.unwrap_or(defaults.expiry_days);
    // 0 天 = 不过期（expiry_at = None）。
    let expiry_at = if expiry_days == 0 {
        None
    } else {
        Some(now + expiry_days as i64 * 86_400)
    };

    let id = format!("intent-{}", uuid::Uuid::now_v7());
    let intent = oc_store::NewStandingIntent {
        id: id.clone(),
        text: text.to_string(),
        keywords,
        cooldown_secs: p.cooldown_secs.unwrap_or(defaults.cooldown_secs),
        budget: p.budget.unwrap_or(defaults.budget),
        expiry_at,
    };
    state.store().writer().intent_add(intent).await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("新增 standing intent 失败: {e}"),
    })?;
    Ok(MethodOk::IntentAdd { intent_id: oc_proto::IntentId::new(id) })
}

async fn handle_intent_list(state: &Arc<ServerState>) -> Result<MethodOk, ProtoError> {
    let rows = state.store().intent_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出 standing intent 失败: {e}"),
    })?;
    let out = rows
        .into_iter()
        .map(|r| oc_proto::IntentSpec {
            id: oc_proto::IntentId::new(r.id),
            text: r.text,
            keywords: r.keywords,
            cooldown_secs: r.cooldown_secs,
            budget: r.budget,
            fired_count: r.fired_count,
            last_fired_at: r.last_fired_at,
            expiry_at: r.expiry_at,
        })
        .collect();
    Ok(MethodOk::IntentList(out))
}

async fn handle_intent_rm(
    p: &oc_proto::IntentRmParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    state
        .store()
        .writer()
        .intent_rm(p.intent_id.to_string())
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("删除 standing intent 失败: {e}"),
        })?;
    Ok(MethodOk::Empty)
}

/// 记忆检索（调试/自省）：Lane1 词法排名（复用 core::rank）。
async fn handle_memory_search(
    p: &oc_proto::MemSearchParams,
    state: &Arc<ServerState>,
) -> Result<MethodOk, ProtoError> {
    use oc_core::memory::{rank, MemCandidate, Origin as CoreOrigin, Tier as CoreTier, RankCfg};

    let terms = tokenize(&p.query);
    let limit = p.limit.unwrap_or(10) as i64;
    let rows = state
        .store()
        .search_candidates(terms.clone(), None, 64)
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("记忆检索失败: {e}"),
        })?;

    let cands: Vec<MemCandidate> = rows
        .iter()
        .map(|r| MemCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            text: r.text.clone(),
            importance: r.importance,
            last_used_secs: r.last_used_at.unwrap_or(r.created_at) / 1000,
            source: r.source.clone(),
        })
        .collect();

    let ranked = rank(&cands, &terms, now_secs(), &RankCfg::default());
    let hits: Vec<oc_proto::MemHit> = ranked
        .into_iter()
        .filter(|r| r.score > 0.0)
        .take(limit as usize)
        .filter_map(|r| {
            let c = cands.iter().find(|c| c.id == r.id)?;
            Some(oc_proto::MemHit {
                id: oc_proto::MemoryId::new(r.id.clone()),
                tier: match c.tier {
                    CoreTier::Curated => "curated",
                    CoreTier::Episodic => "episodic",
                    CoreTier::Prospective => "prospective",
                    CoreTier::Review => "review",
                }
                .to_string(),
                text: c.text.clone(),
                score: r.score as f32,
                source: c.source.clone(),
            })
        })
        .collect();
    Ok(MethodOk::MemorySearch(hits))
}

/// 词法分词（与 session::tokenize 同策略：空白/标点切 + CJK 2-gram）。
fn tokenize(msg: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for seg in msg.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation()) {
        let chars: Vec<char> = seg.chars().collect();
        if chars.len() < 2 {
            continue;
        }
        terms.push(seg.to_string());
        if chars.len() > 2 {
            for w in chars.windows(2) {
                terms.push(w.iter().collect());
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── 斜杠指令的文本格式化（command.rs 调用）──────────────────────
//
// 这些是「把既有只读数据渲染成给人看的文本」的薄封装，与 handle_* 的协议返回
// 并列而非重复实现——命令走的是同一份 store/ledger/snapshot 数据源。
//
// 排版约定（TUI 等宽终端与 Web UI 都按它读）：列之间用**两个以上空格**分隔，
// 一个空格属于值内部；多行列表用 [`align_rows`] 把各列按最宽项补齐，让 id、
// 状态、说明各自对齐成竖列，而不是每行长度不同、眼睛得来回找。

/// 把 `rows`（每行若干列）渲染成对齐的多行文本。
///
/// 每列按该列最宽项补齐后接两个空格；一行里最后一个**非空**单元之后不补，
/// 免得留下看不见的尾随空白（也免得 Web UI 把它当成又一个空列）。
fn align_rows(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    // 宽度按**字符数**算：中文在等宽终端里占两格，但这里的对齐同时要服务
    // 比例字体的 Web UI（那边最终由 CSS 网格对齐），按字符数是两边都不糟的
    // 折中；真正的双宽对齐留给需要它的一端自己做。
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let last = row.iter().rposition(|c| !c.is_empty()).unwrap_or(0);
        for (i, cell) in row.iter().take(last + 1).enumerate() {
            out.push_str(cell);
            if i != last {
                out.push_str(&" ".repeat(widths[i].saturating_sub(cell.chars().count()) + 2));
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `/sessions`：列出会话（标记当前）。
pub(crate) async fn sessions_text(
    state: &Arc<ServerState>,
    current: &SessionId,
) -> Result<String, ProtoError> {
    let rows = state.store().session_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出会话失败: {e}"),
    })?;
    if rows.is_empty() {
        return Ok("（无会话）".to_string());
    }
    let cur_id = current.to_string();
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                format!("[{}]", s.kind),
                if s.id == cur_id { "←当前".to_string() } else { String::new() },
            ]
        })
        .collect();
    Ok(align_rows(&table))
}

/// `/status`：运行状态一行 + 模型一行。
pub(crate) fn status_text(state: &Arc<ServerState>, session: &SessionId) -> String {
    let s = snapshot(state, session);
    let ctx = match s.last_input_tokens {
        Some(used) => {
            let pct = if s.context_window > 0 {
                used as f64 / s.context_window as f64 * 100.0
            } else {
                0.0
            };
            format!("{used}/{} ({pct:.0}%)", s.context_window)
        }
        None => format!("-/{}", s.context_window),
    };
    let mut out = format!(
        "会话:{}  活跃run:{}  排队:{}  后台任务:{}  上下文:{}",
        s.session.as_str(),
        s.active_run.map(|r| r.as_str().to_string()).unwrap_or_else(|| "-".into()),
        s.queued_turns,
        s.background_tasks,
        ctx
    );
    if !s.model.is_empty() {
        out.push_str(&format!("\n模型:{}  provider:{}", s.model, s.provider));
    }
    out
}

/// `/model`：当前模型与 provider / 端点。
pub(crate) fn model_text(state: &Arc<ServerState>) -> String {
    let rt = state.runtime();
    if rt.model.is_empty() {
        return "（未配置模型）".to_string();
    }
    let ep = rt
        .endpoint
        .as_deref()
        .map(|e| format!("  端点:{e}"))
        .unwrap_or_default();
    format!("模型:{}  provider:{}{}", rt.model, rt.provider, ep)
}

/// `/tasks`：后台任务台账。
pub(crate) fn tasks_text(state: &Arc<ServerState>) -> String {
    let tasks = state.ledger().list();
    if tasks.is_empty() {
        return "（无后台任务）".to_string();
    }
    let table: Vec<Vec<String>> = tasks
        .into_iter()
        .map(|t| {
            let state_str = match t.state {
                oc_proto::TaskState::Queued => "排队",
                oc_proto::TaskState::Running => "运行",
                oc_proto::TaskState::Done => "完成",
                oc_proto::TaskState::Failed => "失败",
                oc_proto::TaskState::Cancelled => "已取消",
            };
            vec![
                t.id.as_str().to_string(),
                format!("[{state_str}]"),
                t.detail.unwrap_or_default(),
            ]
        })
        .collect();
    align_rows(&table)
}

/// `/cron list`：定时任务。
pub(crate) async fn cron_text(state: &Arc<ServerState>) -> Result<String, ProtoError> {
    let rows = state.store().cron_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出 cron 失败: {e}"),
    })?;
    if rows.is_empty() {
        return Ok("（无定时任务）".to_string());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|c| {
            let next = match c.next_at {
                Some(t) => oc_core::proactive::fmt_in_tz(t, &c.tz)
                    .map(|s| format!("{s} [{}]", c.tz))
                    .unwrap_or_else(|| format!("unix {t}")),
                None => "-".into(),
            };
            let kind = if oc_core::proactive::is_once(&c.expr) {
                "一次性".to_string()
            } else {
                c.expr.clone()
            };
            vec![
                c.id.clone(),
                format!("[{}]", if c.enabled { "启用" } else { "停用" }),
                kind,
                format!("下次:{next}"),
                format!("«{}»", c.prompt),
            ]
        })
        .collect();
    Ok(align_rows(&table))
}

/// `/intent list`：话题待办。
pub(crate) async fn intent_text(state: &Arc<ServerState>) -> Result<String, ProtoError> {
    let rows = state.store().intent_list().await.map_err(|e| ProtoError {
        kind: oc_proto::ErrorKind::Internal,
        message: format!("列出 standing intent 失败: {e}"),
    })?;
    if rows.is_empty() {
        return Ok("（无话题待办）".to_string());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|i| {
            let last = i
                .last_fired_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "从未".into());
            vec![
                i.id.clone(),
                format!("触发词:[{}]", i.keywords.join(" ")),
                format!("已提醒:{}/{}", i.fired_count, i.budget),
                format!("上次:{last}"),
                format!("«{}»", i.text),
            ]
        })
        .collect();
    Ok(align_rows(&table))
}

/// `/memory search <q>`：按关键词检索记忆（复用 Lane1 排名逻辑）。
pub(crate) async fn memory_text(
    state: &Arc<ServerState>,
    query: &str,
) -> Result<String, ProtoError> {
    use oc_core::memory::{rank, MemCandidate, Origin as CoreOrigin, Tier as CoreTier, RankCfg};

    let terms = tokenize(query);
    let rows = state
        .store()
        .search_candidates(terms.clone(), None, 64)
        .await
        .map_err(|e| ProtoError {
            kind: oc_proto::ErrorKind::Internal,
            message: format!("记忆检索失败: {e}"),
        })?;

    let cands: Vec<MemCandidate> = rows
        .iter()
        .map(|r| MemCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            text: r.text.clone(),
            importance: r.importance,
            last_used_secs: r.last_used_at.unwrap_or(r.created_at) / 1000,
            source: r.source.clone(),
        })
        .collect();

    let ranked = rank(&cands, &terms, now_secs(), &RankCfg::default());
    let hits: Vec<&MemCandidate> = ranked
        .iter()
        .filter(|r| r.score > 0.0)
        .take(10)
        .filter_map(|r| cands.iter().find(|c| c.id == r.id))
        .collect();
    if hits.is_empty() {
        return Ok("（无匹配记忆）".to_string());
    }
    let table: Vec<Vec<String>> = hits
        .iter()
        .map(|c| {
            let tier = match c.tier {
                CoreTier::Curated => "curated",
                CoreTier::Episodic => "episodic",
                CoreTier::Prospective => "prospective",
                CoreTier::Review => "review",
            };
            // 记忆正文可能自带换行，会把「一行一条」的表格结构撕开——压成单行。
            vec![format!("({tier})"), c.text.split_whitespace().collect::<Vec<_>>().join(" ")]
        })
        .collect();
    Ok(align_rows(&table))
}

fn snapshot(state: &Arc<ServerState>, session: &SessionId) -> Snapshot {
    let rt = state.runtime();
    // 活跃 run 与排队深度的活数据源是诊断注册表——session actor 在每个状态迁移点
    // （起步 / 入队 / 出队 / 完成）更新它。会话还没起 actor（或已被空闲淘汰）时
    // 没有格位，按「空闲、无排队」处理。
    let diag = state.diag().snapshot_session(session);
    Snapshot {
        active_run: diag.as_ref().and_then(|d| d.active.as_ref().map(|r| r.run_id.clone())),
        queued_turns: diag.as_ref().map(|d| d.queue_depth).unwrap_or(0) as u32,
        // 台账按任务键组织、不记来源会话，故这是**全局**未结束数，与
        // `Event::Task` 一律归属 main 的现状一致。
        background_tasks: state.ledger().unfinished_count() as u32,
        session: session.clone(),
        context_window: rt.context_window,
        last_input_tokens: state.last_input_tokens(session),
        model: rt.model.clone(),
        provider: rt.provider.clone(),
        endpoint: rt.endpoint.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::align_rows;

    fn rows(src: &[&[&str]]) -> Vec<Vec<String>> {
        src.iter().map(|r| r.iter().map(|c| c.to_string()).collect()).collect()
    }

    /// 列要对齐成竖列：同一列的起始位置在每行相同。
    #[test]
    fn columns_line_up() {
        let out = align_rows(&rows(&[&["a", "x"], &["bbbb", "y"]]));
        let starts: Vec<usize> = out.lines().map(|l| l.find(['x', 'y']).unwrap()).collect();
        assert_eq!(starts[0], starts[1], "第二列没对齐:\n{out}");
    }

    /// 列间至少两个空格——这是两端识别「这是多列」的依据。
    #[test]
    fn columns_are_separated_by_two_spaces() {
        // 两行同宽，故补齐量为 0，只剩固定的两格分隔。
        let out = align_rows(&rows(&[&["ab", "x"], &["cd", "y"]]));
        assert_eq!(out, "ab  x\ncd  y");
    }

    /// 空尾列不留尾随空白（终端里看不见，Web UI 会当成又一个空列）。
    #[test]
    fn empty_trailing_cells_leave_no_padding() {
        let out = align_rows(&rows(&[&["a", "[x]", "←当前"], &["b", "[y]", ""]]));
        for line in out.lines() {
            assert_eq!(line.trim_end(), line, "行有尾随空白: {line:?}");
        }
    }

    /// 宽度按字符数算，不是字节数——否则中文列会补过头。
    #[test]
    fn width_counts_chars_not_bytes() {
        let out = align_rows(&rows(&[&["中文", "x"], &["ab", "y"]]));
        let (first, second) = out.split_once('\n').expect("两行");
        assert_eq!(first.chars().count(), second.chars().count());
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(align_rows(&[]), "");
    }
}
