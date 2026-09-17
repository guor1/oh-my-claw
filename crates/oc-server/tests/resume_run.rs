//! Detached run 断连后，新连接经 ChatResume 接续剩余流（回放 + 续流到终态）。
//!
//! 真机场景：Web 发消息 → 收到部分流 → 刷新页面（旧连接断）→ 新连接 resume，
//! 拿到「刷新前已 stream 的文本（回放）+ 后续剩余文本（续流）」，拼接 = 全量。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_llm::mock::{MockProvider, ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{ClientKind, Event, Frame, Method, MethodOk, ResResult, SessionId};
use oc_server::testing::{SessionConfigExt, TestDaemon};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::ToolRegistry;

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
            since_seq: 0,
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
            since_seq: 0,
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

/// 免审批 exec：`ApprovalMode::Allow` 让工具轮不卡在审批门上。
/// 与 concurrent_submit.rs:28-32 同款。
fn exec_tools() -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(
        ApprovalMode::Allow,
        Duration::from_secs(10),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));
    ToolExecutor::new(Arc::new(reg))
}

/// 带 since_seq 的接续：已落库轮次的事件不重放，只回放「客户端历史还没有的」。
///
/// 真机对应场景：模型先说一句话再调一次工具（这一整轮落进 seq=2 的 assistant
/// dispatch，工具结果落 seq=3），刷新时客户端 loadHistory 已拿到这两条。若 resume
/// 仍把这一轮的事件重放一遍，前端会建出第二张同 call_id 的工具卡、把工具输出拼
/// 两遍、把「我看一下。」渲染两遍——正是本计划要消除的三个症状。
#[tokio::test]
async fn resume_with_since_seq_skips_persisted_rounds() {
    // 第 1 轮：先吐一句正文，再请求工具调用；第 2 轮：慢慢吐 6 段文本。
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Text("我看一下。".into()),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "c1".into(),
                name: Some("exec".into()),
                args_chunk: r#"{"command":"echo hi"}"#.into(),
            }),
        },
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::Done(FinishReason::ToolUse),
        },
    ];
    let provider = Arc::new(SequencedMock::new(vec![
        round1,
        streaming_reply(6, Duration::from_millis(120)),
    ]));

    let daemon = TestDaemon::builder("resume-since", provider)
        .map_cfg(|c| {
            c.with_idle_timeout(Duration::from_secs(120))
                .with_tools(exec_tools())
                // 关掉自动压缩：它会往会话尾插一条 system 摘要 entry，打乱
                // 下面写死的 seq=3。
                .with_auto_compact(false)
        })
        .start()
        .await;

    let mut first = daemon.client_no_handshake().await;
    first.handshake_as(ClientKind::Detached).await;
    first.chat("跑一下", None).await;

    // 等到工具结束 + 至少一段第 2 轮正文，确保 seq=2(dispatch) 与 seq=3(tool
    // result) 都已落库、标记点都已打上。
    let mut run_id = None;
    let mut tool_ended = false;
    let mut round2_deltas = 0usize;
    while !(tool_ended && round2_deltas >= 1) {
        match tokio::time::timeout(Duration::from_secs(10), first.recv())
            .await
            .expect("等工具结束 + 第 2 轮首段")
        {
            Frame::Res(oc_proto::Res {
                result: ResResult::Ok(MethodOk::ChatSend { run_id: rid }),
                ..
            }) => run_id = Some(rid),
            Frame::Event(Event::Tool {
                phase: oc_proto::ToolPhase::End { .. },
                ..
            }) => tool_ended = true,
            Frame::Event(Event::Assistant { .. }) if tool_ended => round2_deltas += 1,
            _ => {}
        }
    }
    let run_id = run_id.expect("应从 Res(ChatSend) 拿到 run_id");
    drop(first);

    // 新连接接续，声明「我历史里已有到 seq=3」。
    // seq 排布：1=user「跑一下」，2=assistant dispatch（含「我看一下。」+ tool_calls），
    // 3=tool result。第 2 轮正文要到 run 收尾才落 seq=4。
    let mut second = daemon.client_no_handshake().await;
    second.handshake_as(ClientKind::Detached).await;
    second.resume(SessionId::main(), run_id.clone(), 3).await;

    let mut tool_events = 0usize;
    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), second.recv())
            .await
            .expect("等接续流到终态")
        {
            Frame::Event(Event::Tool { .. }) => tool_events += 1,
            Frame::Event(Event::Assistant { delta, .. }) => text.push_str(&delta),
            Frame::Event(Event::Lifecycle {
                phase: oc_proto::LifecyclePhase::End,
                ..
            }) => break,
            _ => {}
        }
    }

    assert_eq!(
        tool_events, 0,
        "工具轮已落库（seq<=3），接续不得重放其事件——否则前端建出第二张工具卡、输出拼两遍"
    );
    assert!(
        !text.contains("我看一下。"),
        "第 1 轮正文已落进 seq=2，不得重放，实际收到: {text}"
    );
    assert!(
        text.contains("第5段。"),
        "第 2 轮剩余正文仍须完整续上，实际收到: {text}"
    );
}
