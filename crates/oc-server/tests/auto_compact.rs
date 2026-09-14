//! 自动滚动摘要压缩：每轮结束后历史超水位时后台压缩落库 checkpoint。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_server::session::{self, SessionConfig};
use oc_server::testing::{test_cfg, SessionConfigExt};
use tokio::sync::broadcast;

fn cfg() -> SessionConfig {
    // 默认 test_cfg 的 history_token_budget = 8000 → 水位 6400。
    test_cfg().with_auto_compact(true).with_history(500, 8000)
}

async fn wait_terminal(rx: &mut broadcast::Receiver<oc_proto::Event>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(
            ev,
            oc_proto::Event::Lifecycle { phase: oc_proto::LifecyclePhase::End, .. }
                | oc_proto::Event::Lifecycle { phase: oc_proto::LifecyclePhase::Error { .. }, .. }
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn auto_compact_triggers_on_long_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 40 条 × 500 token_est = 20000，远超水位 6400。
    let big = "字".repeat(400);
    for i in 0..40 {
        let role = if i % 2 == 0 { oc_store::Role::User } else { oc_store::Role::Assistant };
        w.append_entry(oc_store::NewEntry::text("main", role, format!("{big}#{i}"), 500))
            .await
            .unwrap();
    }

    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("这是压缩后的摘要文本"));
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    handle.submit("最新一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // 自动压缩在后台执行，轮询等落库生效（摘要 entry 出现）。
    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let hist = w.load_transcript("main".into(), 500).await.unwrap();
        if hist.iter().any(|e| e.content.contains("上下文摘要")) {
            assert!(hist.iter().any(|e| e.content.contains("这是压缩后的摘要文本")), "应含摘要文本");
            assert!(!hist.iter().any(|e| e.content.contains("历史消息 0")), "最早消息应被排除");
            ok = true;
            break;
        }
    }
    assert!(ok, "自动压缩应在超时内完成落库");
}

#[tokio::test]
async fn auto_compact_skips_short_history() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();
    // 3 条短历史，远低于水位。
    for (role, text) in [
        (oc_store::Role::User, "hi"),
        (oc_store::Role::Assistant, "在"),
        (oc_store::Role::User, "就一句"),
    ] {
        w.append_entry(oc_store::NewEntry::text("main", role, text, 1))
            .await
            .unwrap();
    }

    let (tx, mut rx) = broadcast::channel(256);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let handle = session::spawn(
        oc_proto::SessionId::main(),
        cfg(),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&oc_proto::SessionId::main()),
    );

    handle.submit("就一句".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;
    // 给足时间让「若有误触发的压缩」跑出来，再断言没有。
    tokio::time::sleep(Duration::from_millis(300)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "短历史不应触发摘要模型调用（captures 应只有主 run 一次）");
    let hist = w.load_transcript("main".into(), 500).await.unwrap();
    assert!(!hist.iter().any(|e| e.content.contains("上下文摘要")), "短历史不应产生摘要 entry");
}
