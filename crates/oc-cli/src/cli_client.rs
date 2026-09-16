//! 薄命令行客户端（设计 §9）：连 daemon、发一个请求、收对应应答、返回。
//!
//! 用于 `oc cron`/`oc memory`/`oc status` 等一次性命令（非交互 TUI）。
//! 握手（connect → hello）后发目标方法，按 req id 匹配应答，忽略中途事件帧。

use anyhow::{anyhow, bail, Result};
use oc_proto::{
    ClientKind, ConnectParams, Frame, Method, MethodOk, Req, ReqId, ResResult, PROTO_VERSION,
};
use oc_tui::client::{ClientTransport, ConnectTo};

use crate::paths;

/// 连接 daemon 并完成握手，返回就绪的传输。
async fn connect() -> Result<ClientTransport> {
    let home = paths::oc_home()?;
    let to = ConnectTo::platform_default(&home);
    let mut client = ClientTransport::connect(&to).await.map_err(|e| {
        anyhow!("无法连接 daemon：{e}\n请先在另一个终端运行：oc serve")
    })?;

    // 握手。
    client
        .send(&Frame::Req(Req {
            id: ReqId::new("connect-0"),
            method: Method::Connect(ConnectParams {
                proto_version: PROTO_VERSION,
                token: None,
                client_kind: ClientKind::Interactive,
            }),
            idempotency_key: None,
        }))
        .await?;
    // 等 hello（跳过可能先到的事件帧）。
    loop {
        match client.recv().await? {
            Some(Frame::Res(res)) => match res.result {
                ResResult::Ok(_) => break,
                ResResult::Err(e) => bail!("连接被拒: {}", e.message),
            },
            Some(_) => continue,
            None => bail!("daemon 在握手期间断开"),
        }
    }
    Ok(client)
}

/// 发一个方法并等其应答（按 req id 匹配，忽略事件帧）。
async fn call(client: &mut ClientTransport, method: Method) -> Result<MethodOk> {
    let req_id = "cli-1";
    client
        .send(&Frame::Req(Req {
            id: ReqId::new(req_id),
            method,
            idempotency_key: None,
        }))
        .await?;
    loop {
        match client.recv().await? {
            Some(Frame::Res(res)) if res.id.as_str() == req_id => match res.result {
                ResResult::Ok(ok) => return Ok(ok),
                ResResult::Err(e) => bail!("{}", e.message),
            },
            Some(_) => continue, // 事件帧 / 其它应答，跳过
            None => bail!("daemon 断开"),
        }
    }
}

/// 在一个临时 runtime 上跑一次 request/response。
fn run_once<F>(f: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(f)
}

// ── 命令实现 ────────────────────────────────────────────────────

/// 新增定时任务。`tz=None` → 用本机时区（用户敲 `0 9 * * *` 想要的是本地 9 点）。
pub fn cron_add(expr: String, prompt: String, tz: Option<String>) -> Result<()> {
    let tz = tz
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(crate::tz::local_tz);
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(
            &mut c,
            Method::CronAdd(oc_proto::CronAddParams { expr, prompt, tz: tz.clone() }),
        )
        .await?;
        if let MethodOk::CronAdd { cron_id } = ok {
            // 回显时区：省略 --tz 时用户需要看到实际生效的是哪个。
            println!("已添加定时任务：{}（时区 {tz}）", cron_id.as_str());
        }
        Ok(())
    })
}

pub fn cron_list() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::CronList).await?;
        if let MethodOk::CronList(list) = ok {
            if list.is_empty() {
                println!("（无定时任务）");
            } else {
                for c in list {
                    // 下次触发按该行自己的 tz 渲染成墙上时间。
                    // 原先只印 unix 秒，排查「提醒为什么没响」时根本看不出差了 8 小时
                    // ——那正是 P1-5 时区 bug 迟迟未被发现的原因之一。
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
                    let en = if c.enabled { "启用" } else { "停用" };
                    println!(
                        "{}  [{}]  {}  下次:{}  «{}»",
                        c.id.as_str(),
                        en,
                        kind,
                        next,
                        c.prompt
                    );
                }
            }
        }
        Ok(())
    })
}

pub fn cron_rm(cron_id: String) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        call(
            &mut c,
            Method::CronRm(oc_proto::CronRmParams { cron_id: oc_proto::CronId::new(cron_id) }),
        )
        .await?;
        println!("已删除。");
        Ok(())
    })
}

pub fn intent_add(
    text: String,
    keywords: Vec<String>,
    cooldown_secs: Option<i64>,
    budget: Option<u32>,
    expiry_days: Option<u32>,
) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(
            &mut c,
            Method::IntentAdd(oc_proto::IntentAddParams {
                text,
                keywords,
                cooldown_secs,
                budget,
                expiry_days,
            }),
        )
        .await?;
        if let MethodOk::IntentAdd { intent_id } = ok {
            println!("已添加话题待办：{}", intent_id.as_str());
        }
        Ok(())
    })
}

pub fn intent_list() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::IntentList).await?;
        if let MethodOk::IntentList(list) = ok {
            if list.is_empty() {
                println!("（无话题待办）");
            } else {
                for i in list {
                    let last = i
                        .last_fired_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "从未".into());
                    println!(
                        "{}  触发词:[{}]  已提醒:{}/{}  上次:{}  «{}»",
                        i.id.as_str(),
                        i.keywords.join(" "),
                        i.fired_count,
                        i.budget,
                        last,
                        i.text
                    );
                }
            }
        }
        Ok(())
    })
}

pub fn intent_rm(intent_id: String) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        call(
            &mut c,
            Method::IntentRm(oc_proto::IntentRmParams {
                intent_id: oc_proto::IntentId::new(intent_id),
            }),
        )
        .await?;
        println!("已删除。");
        Ok(())
    })
}

pub fn memory_search(query: String, limit: Option<u32>) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(
            &mut c,
            Method::MemorySearch(oc_proto::MemSearchParams { query, limit }),
        )
        .await?;
        if let MethodOk::MemorySearch(hits) = ok {
            if hits.is_empty() {
                println!("（无匹配记忆）");
            } else {
                for h in hits {
                    match &h.source {
                        Some(src) => println!("[{:.3}] ({}) 来源:{} {}", h.score, h.tier, src, h.text),
                        None => println!("[{:.3}] ({}) {}", h.score, h.tier, h.text),
                    }
                }
            }
        }
        Ok(())
    })
}

pub fn sessions() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::SessionsList).await?;
        if let MethodOk::Sessions(list) = ok {
            if list.is_empty() {
                println!("（无会话）");
            } else {
                for s in list {
                    println!("{}  [{}]  创建:{}", s.id.as_str(), s.kind, s.created_at);
                }
            }
        }
        Ok(())
    })
}

pub fn compact() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        call(&mut c, Method::Compact(oc_proto::CompactParams { session: None })).await?;
        println!("已请求压缩上下文（摘要将在后台生成）。");
        Ok(())
    })
}

pub fn debug(watch: bool) -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        if !watch {
            let snap = fetch_diag(&mut c).await?;
            print!("{}", render_diag(&snap));
            return Ok(());
        }
        // --watch：每秒刷新。清屏 + 重绘。Ctrl-C 退出。
        loop {
            let snap = fetch_diag(&mut c).await?;
            // ANSI 清屏 + 光标归位。
            print!("\x1b[2J\x1b[H{}", render_diag(&snap));
            use std::io::Write;
            std::io::stdout().flush().ok();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}

async fn fetch_diag(c: &mut ClientTransport) -> Result<oc_proto::DiagnosticsSnapshot> {
    match call(c, Method::Diagnostics).await? {
        MethodOk::Diagnostics(s) => Ok(s),
        _ => bail!("daemon 返回了非预期的应答类型"),
    }
}

/// 渲染诊断快照为文本表格。
fn render_diag(s: &oc_proto::DiagnosticsSnapshot) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let (hh, mm, ss) = (s.uptime_secs / 3600, (s.uptime_secs % 3600) / 60, s.uptime_secs % 60);
    let writer = if s.store_writer_alive { "OK" } else { "DOWN" };
    let _ = writeln!(
        out,
        "uptime {hh:02}:{mm:02}:{ss:02}   writer {writer}   subs {}   idem {}   proto v{}",
        s.event_subscribers, s.idem_entries, s.proto_version
    );
    let _ = writeln!(
        out,
        "{:<10} {:<6} {:<13} {:<12} {:>6} {:>10} {:>6} {:<20}",
        "session", "queue", "phase", "run_id", "age", "last_delta", "tools", "err"
    );
    if s.sessions.is_empty() {
        let _ = writeln!(out, "（无会话）");
    }
    for sess in &s.sessions {
        let err = sess.last_error.as_deref().unwrap_or("-");
        match &sess.active {
            Some(r) => {
                let age = fmt_age_ms(s.sampled_at - r.started_at);
                let last_delta = match r.last_delta_at {
                    Some(at) => fmt_age_ms(s.sampled_at - at),
                    None => "-".to_string(),
                };
                let rid_short: String = r.run_id.as_str().chars().take(8).collect();
                let _ = writeln!(
                    out,
                    "{:<10} {:<6} {:<13} {:<12} {:>6} {:>10} {:>6} {:<20}",
                    truncate(sess.session_id.as_str(), 10),
                    sess.queue_depth,
                    phase_str(r.phase),
                    rid_short,
                    age,
                    last_delta,
                    r.tool_rounds,
                    truncate(err, 20),
                );
            }
            None => {
                let last = sess.last_finish_reason.as_deref().unwrap_or("-");
                let _ = writeln!(
                    out,
                    "{:<10} {:<6} {:<13} {:<12} {:>6} {:>10} {:>6} {:<20}",
                    truncate(sess.session_id.as_str(), 10),
                    sess.queue_depth,
                    "idle",
                    format!("last:{}", truncate(last, 6)),
                    "-",
                    "-",
                    "-",
                    truncate(err, 20),
                );
            }
        }
    }
    out
}

fn phase_str(p: oc_proto::RunPhase) -> &'static str {
    match p {
        oc_proto::RunPhase::Starting => "starting",
        oc_proto::RunPhase::AwaitingModel => "awaiting-mdl",
        oc_proto::RunPhase::Streaming => "streaming",
        oc_proto::RunPhase::ToolExec => "tool-exec",
        oc_proto::RunPhase::Compacting => "compacting",
    }
}

/// 毫秒时长格式化为紧凑 age（如 4.1s / 61s / 3m）。
fn fmt_age_ms(ms: i64) -> String {
    let ms = ms.max(0);
    let secs = ms as f64 / 1000.0;
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 120.0 {
        format!("{:.0}s", secs)
    } else {
        format!("{:.0}m", secs / 60.0)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

pub fn status() -> Result<()> {
    run_once(async move {
        let mut c = connect().await?;
        let ok = call(&mut c, Method::Status).await?;
        if let MethodOk::Status(s) = ok {
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
            println!(
                "会话:{}  活跃run:{}  排队:{}  后台任务:{}  上下文:{}",
                s.session.as_str(),
                s.active_run.map(|r| r.as_str().to_string()).unwrap_or_else(|| "-".into()),
                s.queued_turns,
                s.background_tasks,
                ctx
            );
            // 模型单独一行：换 provider 后靠这行验证生效，不必开对话去问模型。
            // endpoint 必须打——DeepSeek 与豆包同为 provider=openai，只看模型名分不出。
            // 老 daemon 不发这三项（serde default），此时整行跳过而非打一堆空值。
            if !s.model.is_empty() {
                let ep = s
                    .endpoint
                    .as_deref()
                    .map(|e| format!("  端点:{e}"))
                    .unwrap_or_default();
                println!("模型:{}  provider:{}{}", s.model, s.provider, ep);
            }
        }
        Ok(())
    })
}
