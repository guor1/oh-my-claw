//! M5 第 5 段：记忆写入路径 + dreaming 巩固。
//!
//! 1) 用户显式"记住…" → 写 curated 记忆（origin=Owner），后续相关消息能被 Lane1 注入。
//! 2) dreaming 双门：episodic 候选经 server 调度巩固为 curated；Untrusted 来源被拒。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use tokio::sync::broadcast;
use oc_server::testing::{test_cfg, SessionConfigExt};

fn cfg() -> SessionConfig {
    test_cfg().with_soul("人格").with_trigger_threshold(0.5)
}

async fn wait_terminal(rx: &mut broadcast::Receiver<Event>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            Event::Lifecycle { phase: LifecyclePhase::End, .. }
                | Event::Lifecycle { phase: LifecyclePhase::Error { .. }, .. }
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn explicit_remember_then_recall() {
    let store = oc_store::Store::open_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好的，记下了"));
    let captures = provider.captures();
    let handle = session::spawn(oc_proto::SessionId::main(), cfg(), provider, tx, store.clone(), oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()));

    // 第 1 轮：显式"记住…"，触发 curated 写入。
    handle.submit("记住：我喜欢简洁直接的回复".into(), handle.broadcast_sink()).await.expect("run1");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // 该记忆应已落库为 curated + Owner。
    let curated = store
        .writer()
        .search_candidates(vec!["简洁".into()], Some(oc_store::Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(curated.len(), 1, "显式记忆应写入 curated");
    assert_eq!(curated[0].origin, oc_store::Origin::Owner, "显式指令 origin=Owner");

    // 第 2 轮：相关消息 → Lane1 注入该记忆。
    handle.submit("你平时怎么组织回复的".into(), handle.broadcast_sink()).await.expect("run2");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    let last = reqs.last().unwrap().system.as_deref().unwrap_or("");
    assert!(
        last.contains("我喜欢简洁直接的回复"),
        "记住的内容应被后续消息召回注入: {last}"
    );
}

#[tokio::test]
async fn dreaming_consolidates_qualified_and_rejects_untrusted() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();

    // 合格 episodic 候选：Agent 来源、高分、够旧（age 由 created_at 决定，
    // 内存库 created_at=now，age≈0 会被 TooRecent 拒；故用 max_age_secs=0 + min_age_secs=0 的自定义 cfg。
    // 这里直接调 server::dreaming::scan，用一个放宽时间窗的 cfg。
    w.upsert_memory(oc_store::NewMemory {
        id: "ep-good".into(),
        tier: oc_store::Tier::Episodic,
        origin: oc_store::Origin::Agent,
        text: "用户常在周五做复盘".into(),
        keywords: None,
        importance: 0.8,
        content_hash: "h1".into(),
        pref_key: None,
        source: Some("sess-good".into()),
    })
    .await
    .unwrap();
    // 频次门：需要 use_count ≥ 2。touch 两次。
    let now = 0i64;
    w.touch_memory("ep-good".into(), now).await.unwrap();
    w.touch_memory("ep-good".into(), now).await.unwrap();

    // 不可信来源：即便高分高频也应被门2排除。
    w.upsert_memory(oc_store::NewMemory {
        id: "ep-bad".into(),
        tier: oc_store::Tier::Episodic,
        origin: oc_store::Origin::Untrusted,
        text: "网页声称的所谓事实".into(),
        keywords: None,
        importance: 0.9,
        content_hash: "h2".into(),
        pref_key: None,
        source: Some("sess-bad".into()),
    })
    .await
    .unwrap();
    w.touch_memory("ep-bad".into(), now).await.unwrap();
    w.touch_memory("ep-bad".into(), now).await.unwrap();

    // 放宽时间窗（内存库 age≈0），其余用默认。
    let dream_cfg = oc_core::dreaming::DreamCfg {
        min_age_secs: 0,
        max_age_secs: 0,
        ..oc_core::dreaming::DreamCfg::default()
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let promoted = oc_server::dreaming::scan(&store, now_secs, &dream_cfg).await;
    assert_eq!(promoted, 1, "只应巩固合格的一条");

    // ep-good 已巩固为 curated。
    let curated = w
        .search_candidates(vec!["复盘".into()], Some(oc_store::Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(curated.len(), 1);
    assert_eq!(curated[0].id, "ep-good");

    // ep-bad 仍是 episodic（未被巩固）。
    let still_ep = w.dream_candidates(10).await.unwrap();
    assert!(
        still_ep.iter().any(|m| m.id == "ep-bad"),
        "不可信来源应保持未巩固"
    );
    assert!(
        !still_ep.iter().any(|m| m.id == "ep-good"),
        "合格记忆应已离开 episodic 池"
    );
}

// ── P1-3：偏好 supersede（设计 §4.5(e)）─────────────────────────────

/// 起一个会话、提交一句话、等跑完。用于驱动 `persist_explicit_memory`。
async fn say(store: &oc_store::Store, msg: &str) {
    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好的"));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );
    handle.submit(msg.into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;
}

/// 取某主题下的所有偏好文本（按建立顺序）。
async fn prefs_of(store: &oc_store::Store, key: &str) -> Vec<String> {
    store
        .writer()
        .memory_by_pref_key(key.into())
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect()
}

/// 同主题的新偏好应**就地替换**旧值，而不是并列两条矛盾记忆。
#[tokio::test]
async fn conflicting_pref_replaces_old_one() {
    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    say(&store, "记住我用 VS Code").await;
    assert_eq!(prefs_of(&store, "编辑器").await, vec!["我用 VS Code".to_string()]);

    // 换编辑器：旧值应被顶掉，而非并存。
    say(&store, "记住我改用 Neovim 了").await;
    let after = prefs_of(&store, "编辑器").await;
    assert_eq!(after.len(), 1, "同主题只应留一条，实际: {after:?}");
    assert_eq!(after[0], "我改用 Neovim 了");
}

/// 同主题同值说两次应被忽略，不产生重复条目。
#[tokio::test]
async fn same_pref_twice_is_ignored() {
    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    say(&store, "记住我用 Neovim").await;
    say(&store, "记住我用 Neovim").await;
    assert_eq!(prefs_of(&store, "编辑器").await.len(), 1, "重复偏好不应写两条");
}

/// 不同主题的偏好各自独立，**不得互相覆盖**（误覆盖会丢信息）。
#[tokio::test]
async fn different_topics_coexist() {
    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    say(&store, "记住我用 Neovim").await;
    say(&store, "记住我现在用 macOS").await;
    say(&store, "记住主力语言是 Rust").await;

    assert_eq!(prefs_of(&store, "编辑器").await.len(), 1, "编辑器偏好应保留");
    assert_eq!(prefs_of(&store, "操作系统").await.len(), 1, "操作系统偏好应保留");
    assert_eq!(prefs_of(&store, "编程语言").await.len(), 1, "编程语言偏好应保留");
}

/// 非偏好类记忆走原路径：不带 pref_key、不参与 supersede、可并存多条。
#[tokio::test]
async fn non_pref_memory_still_appends() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    say(&store, "记住周三下午有例会").await;
    say(&store, "记住房东电话是 138").await;

    // 两条都在，且都不带 pref_key（不参与 supersede）。
    let all = w.search_candidates(vec![], Some(oc_store::Tier::Curated), 50).await.unwrap();
    let plain: Vec<_> = all.iter().filter(|m| m.pref_key.is_none()).collect();
    assert_eq!(plain.len(), 2, "非偏好记忆应各自保留，实际: {plain:?}");
}
