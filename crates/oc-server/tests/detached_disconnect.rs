//! Detached 客户端断连回归：Web/HTTP 网关发起的 run 不随连接断开而中止。
//!
//! 真机缺陷：Web 端发出消息 → 收到 thinking 后立即刷新页面 → run 被断连
//! 掐死（日志 `等模型期间客户端断开，收敛 run`），回复既不再继续，也未落库。
//!
//! 根因：`chat.send` 内联事件经 `RunSink::Conn` 定向回发到发起连接；断连 →
//! 出站队列关闭 → `sink.closed()` → run Aborted（Aborted 不落库）。
//!
//! 修复：Web/HTTP 网关连接标记 `ClientKind::Detached`，其 run 用
//! `RunSink::Detached`（send 恒成功、closed 永不完成），run 归属会话而非连接。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::ClientKind;
use oc_server::testing::{SessionConfigExt, TestDaemon};

/// 持续吐字：每 100ms 一段共 10 段。断连发生在中途，剩余段证明 run 仍在推进。
fn streaming_reply(n: usize, gap: Duration) -> Vec<ScriptStep> {
    let mut steps: Vec<_> = (0..n)
        .map(|i| ScriptStep {
            delay: gap,
            delta: Delta::Text(format!("第{i}段。")),
        })
        .collect();
    steps.push(ScriptStep {
        delay: Duration::ZERO,
        delta: Delta::Done(FinishReason::Stop),
    });
    steps
}

#[tokio::test]
async fn detached_client_disconnect_does_not_abort_run_and_persists() {
    let store = oc_store::Store::open_memory().expect("store");
    let daemon = TestDaemon::builder(
        "detached-disc",
        Arc::new(MockProvider::scripted(streaming_reply(
            10,
            Duration::from_millis(100),
        ))),
    )
    .store(store.clone())
    // idle_timeout 给足，确保「跑完并落库」只可能来自 Detached 语义，而非看门狗。
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    // Detached 连接（模拟 Web/HTTP 网关）。
    let mut client = daemon.client_no_handshake().await;
    client.handshake_as(ClientKind::Detached).await;
    client.chat("讲个故事", None).await;

    // 收到首个事件（证明 run 已起步、连接存活）后立即断连。
    // 注意帧序：`chat.send` 先同步回一条 Res(ChatSend{run_id})（dispatch 返回后
    // 连接层即发出，早于 run 任务经 sink 推送的任意事件），故先收掉应答，
    // 再等首个事件。
    let ack = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("应在断连前收到 chat.send 应答");
    assert!(
        matches!(&ack, oc_proto::Frame::Res(_)),
        "chat.send 应先返回应答, got: {ack:?}"
    );
    let first = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("应在断连前收到首个事件");
    assert!(matches!(first, oc_proto::Frame::Event(_)), "首个事件帧应为事件, got: {first:?}");
    drop(client);

    // run 应在断连后继续跑完并落库 assistant。轮询 transcript 直至出现 assistant。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut assistant: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        let hist = store
            .writer()
            .load_transcript("main".into(), 100)
            .await
            .expect("load");
        assistant = hist
            .iter()
            .find(|e| e.role == oc_store::Role::Assistant)
            .map(|e| e.content.clone());
        if assistant.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = assistant.expect("断连后 run 应跑完并落库 assistant 回复");
    assert!(
        text.contains("第9段"),
        "应含完整回复（断连后仍继续产出），实际: {text}"
    );
}
