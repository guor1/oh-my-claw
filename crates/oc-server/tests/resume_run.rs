//! Detached run 断连后，新连接经 ChatResume 接续剩余流（回放 + 续流到终态）。
//!
//! 真机场景：Web 发消息 → 收到部分流 → 刷新页面（旧连接断）→ 新连接 resume，
//! 拿到「刷新前已 stream 的文本（回放）+ 后续剩余文本（续流）」，拼接 = 全量。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::{MockProvider, ScriptStep};
use oc_llm::{Delta, FinishReason};
use oc_proto::{ClientKind, Event, Frame, Method, MethodOk, ResResult, SessionId};
use oc_server::testing::{SessionConfigExt, TestDaemon};

/// 持续吐字：每 150ms 一段共 8 段。断连发生在第 2 段后，剩余 6 段靠 resume 接续。
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
async fn resume_replays_then_streams_remainder() {
    let daemon = TestDaemon::builder(
        "resume",
        Arc::new(MockProvider::scripted(streaming_reply(
            8,
            Duration::from_millis(150),
        ))),
    )
    .map_cfg(|c| c.with_idle_timeout(Duration::from_secs(120)))
    .start()
    .await;

    // Detached 连接（Web/HTTP），发起 run。
    let mut first = daemon.client_no_handshake().await;
    first.handshake_as(ClientKind::Detached).await;
    first.chat("讲个故事", None).await;

    // 收前两段 assistant delta，然后断连（drop）。
    // 帧序：chat.send 先同步回一条 Res(ChatSend{run_id})（dispatch 返回后连接层
    // 即发出，早于 run 任务经 sink 推送的任意事件），故从它取真实 run_id——
    // 镜像 collect_turn 里 `turn.run_id = Some(run_id)` 的取法（testing.rs）。
    let mut seen = 0usize;
    let mut run_id = None;
    while seen < 2 {
        match tokio::time::timeout(Duration::from_secs(5), first.recv())
            .await
            .expect("等前两段")
        {
            Frame::Res(oc_proto::Res {
                result: ResResult::Ok(MethodOk::ChatSend { run_id: rid }),
                ..
            }) => {
                run_id = Some(rid);
            }
            Frame::Event(Event::Assistant { .. }) => seen += 1,
            _ => {}
        }
    }
    let run_id = run_id.expect("应从 chat.send 回执 Res(ChatSend{run_id}) 拿到 run_id");
    drop(first);

    // 新连接 resume 同一个 run。Interactive 也行；resume 只看 run_id+session。
    let mut second = daemon.client().await;
    second
        .request(Method::ChatResume(oc_proto::ChatResumeParams {
            session: SessionId::main(),
            run_id,
        }))
        .await;
    let turn = second.collect_turn(Duration::from_secs(10)).await;

    // 回放 ∪ 续流 = 全量 8 段。
    let full = turn.text();
    for i in 0..8 {
        assert!(full.contains(&format!("第{i}段。")), "缺少第{i}段，实际: {full}");
    }
}

#[tokio::test]
async fn resume_unknown_run_is_rejected() {
    let daemon = TestDaemon::start("resume-unknown", Arc::new(MockProvider::echo_text("x")))
        .await;
    let mut c = daemon.client().await;
    let id = c
        .request(Method::ChatResume(oc_proto::ChatResumeParams {
            session: SessionId::main(),
            run_id: oc_proto::RunId::new("不存在的run"),
        }))
        .await;
    // 应收到 Res(Err)。
    let res = c.recv().await;
    match res {
        Frame::Res(r) => {
            assert!(matches!(r.result, ResResult::Err(_)), "未知 run 应报错")
        }
        other => panic!("期望 Res，得到 {other:?}"),
    }
    let _ = id;
}
