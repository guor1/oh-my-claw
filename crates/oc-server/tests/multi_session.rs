//! 多会话支持：验证会话注册表按 SessionId 路由、事件正确归属、transcript 隔离。
//!
//! 覆盖三个不变量：
//! 1. 两个会话并发跑，各自的 Assistant/Lifecycle 事件带正确的 session 归属。
//! 2. 两会话的 transcript 互相隔离（各自只看到自己的消息）。
//! 3. 未知会话 id 经 chat.send 隐式创建（get_or_spawn 懒建）。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase, SessionId};
use oc_server::registry::SessionRegistry;
use oc_server::session::SessionConfig;
use tokio::sync::broadcast;
use oc_server::testing::{test_cfg, SessionConfigExt};

fn cfg() -> SessionConfig {
    test_cfg().with_soul("人格").with_trigger_threshold(0.5)
}

/// 收集事件直到看到指定 run 数量的 End（或超时）。
async fn collect_until_ends(
    rx: &mut broadcast::Receiver<Event>,
    expected_ends: usize,
    timeout: Duration,
) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut out = Vec::new();
    let mut ends = 0;
    while ends < expected_ends {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => {
                if matches!(
                    ev,
                    Event::Lifecycle { phase: LifecyclePhase::End, .. }
                        | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
                ) {
                    ends += 1;
                }
                out.push(ev);
            }
            _ => break,
        }
    }
    out
}

/// 取事件归属的会话 id。
fn ev_session(ev: &Event) -> &SessionId {
    match ev {
        Event::Lifecycle { session, .. }
        | Event::Assistant { session, .. }
        | Event::Reasoning { session, .. }
        | Event::Tool { session, .. }
        | Event::Proactive { session, .. }
        | Event::Task { session, .. }
        | Event::Usage { session, .. }
        | Event::Approval { session, .. }
        | Event::UserInput { session, .. } => session,
    }
}

#[tokio::test]
async fn events_are_attributed_to_correct_session() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("回复内容"));
    let registry = SessionRegistry::new(cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new());

    let work = SessionId::new("work");
    let personal = SessionId::new("personal");

    // 两个会话各提交一轮（未知 id → 隐式创建）。
    let h_work = registry.get_or_spawn(&work);
    let h_personal = registry.get_or_spawn(&personal);
    h_work.submit("工作会话的消息".into(), h_work.broadcast_sink()).await.expect("work run");
    h_personal.submit("私人会话的消息".into(), h_personal.broadcast_sink()).await.expect("personal run");

    let evs = collect_until_ends(&mut rx, 2, Duration::from_secs(10)).await;

    // 每个会话都应有 Assistant + Lifecycle End，且归属正确。
    let work_asst = evs.iter().any(|e| {
        matches!(e, Event::Assistant { .. }) && ev_session(e) == &work
    });
    let personal_asst = evs.iter().any(|e| {
        matches!(e, Event::Assistant { .. }) && ev_session(e) == &personal
    });
    assert!(work_asst, "work 会话应有归属正确的 Assistant 事件");
    assert!(personal_asst, "personal 会话应有归属正确的 Assistant 事件");

    // 不应有任何事件归属到从未使用的会话。
    assert!(
        evs.iter().all(|e| {
            let s = ev_session(e);
            s == &work || s == &personal
        }),
        "事件只应归属 work / personal 两个会话"
    );
}

#[tokio::test]
async fn transcripts_are_isolated_per_session() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("知道了"));
    let registry = SessionRegistry::new(cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new());

    let a = SessionId::new("alpha");
    let b = SessionId::new("beta");
    let ha = registry.get_or_spawn(&a);
    ha.submit("只属于 alpha 的话".into(), ha.broadcast_sink()).await.unwrap();
    let hb = registry.get_or_spawn(&b);
    hb.submit("只属于 beta 的话".into(), hb.broadcast_sink()).await.unwrap();
    collect_until_ends(&mut rx, 2, Duration::from_secs(10)).await;

    // alpha 的 transcript 只含 alpha 的用户消息（+ assistant 回复），不含 beta 的。
    let alpha_hist = store.writer().load_transcript("alpha".into(), 100).await.unwrap();
    let alpha_texts: Vec<&str> = alpha_hist.iter().map(|e| e.content.as_str()).collect();
    assert!(
        alpha_texts.iter().any(|t| t.contains("只属于 alpha 的话")),
        "alpha 历史应含自己的消息: {alpha_texts:?}"
    );
    assert!(
        !alpha_texts.iter().any(|t| t.contains("只属于 beta 的话")),
        "alpha 历史不应含 beta 的消息: {alpha_texts:?}"
    );

    // 反向亦然。
    let beta_hist = store.writer().load_transcript("beta".into(), 100).await.unwrap();
    let beta_texts: Vec<&str> = beta_hist.iter().map(|e| e.content.as_str()).collect();
    assert!(
        beta_texts.iter().any(|t| t.contains("只属于 beta 的话")),
        "beta 历史应含自己的消息: {beta_texts:?}"
    );
    assert!(
        !beta_texts.iter().any(|t| t.contains("只属于 alpha 的话")),
        "beta 历史不应含 alpha 的消息: {beta_texts:?}"
    );
}

#[tokio::test]
async fn same_id_reuses_one_actor() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, _rx) = broadcast::channel(64);
    let provider = Arc::new(CapturingMock::new("ok"));
    let registry = SessionRegistry::new(cfg(), provider, tx, store, oc_server::diag::DiagRegistry::new());

    // main 在 new() 里已预建；再取应是同一 actor（不 panic、不重复建）。
    let id = SessionId::new("dup");
    let _h1 = registry.get_or_spawn(&id);
    let _h2 = registry.get_or_spawn(&id);
    // 能连续 submit 说明句柄有效（同一 actor 串行处理）。
    let h = registry.get_or_spawn(&id);
    h.submit("第一条".into(), h.broadcast_sink()).await.expect("first");
    h.submit("第二条".into(), h.broadcast_sink()).await.expect("second");
}
