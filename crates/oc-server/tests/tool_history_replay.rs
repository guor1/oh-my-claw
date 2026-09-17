//! P2-4：历史重放必须还原原生工具结构。
//!
//! 回归的是这个缺陷：`entry` 表没有工具调用列时，重放把工具结果一律降级成
//! `user` 文本（带「【历史工具结果】」前缀），于是模型在自己的上下文里从没见过
//! 「我发起工具调用」的样例，只见过「我宣布要做什么 → 用户把输出贴给我」。
//! in-context learning 压倒系统提示词，模型学会宣布完就 `Stop` 等人贴结果——
//! 而且每失败一轮就多一条坏样例，自我强化。
//!
//! 这里断言的是**到达模型的消息序列**，因为坏样例正是从那里被学去的。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::CapturingMock;
use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_proto::{Event, LifecyclePhase};
use oc_server::session::{self, SessionConfig};
use oc_server::testing::{test_cfg, SessionConfigExt};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;

fn cfg() -> SessionConfig {
    test_cfg().with_soul("人格")
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

/// 跑一轮，返回到达模型的消息序列。
async fn messages_sent_after_seeding(
    store: oc_store::Store,
) -> Vec<oc_llm::Message> {
    let (tx, mut rx) = broadcast::channel(512);
    let provider = Arc::new(CapturingMock::new("好"));
    let captures = provider.captures();
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        cfg(),
        provider,
        tx,
        store,
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );

    handle.submit("接着干".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "应发起一次模型请求");
    reqs[0].messages.clone()
}

/// 库里存了工具结构 → 重放出原生 `assistant(tool_calls)` + `tool(result)`。
#[tokio::test]
async fn replays_native_tool_structure() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::User, "装个包", 3))
        .await
        .unwrap();
    w.append_entry(oc_store::NewEntry {
        tool_calls: Some(r#"[{"id":"call_1","name":"sys","args":"{\"cmd\":\"pip install x\"}"}]"#.into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Assistant, "我装一下。", 3)
    })
    .await
    .unwrap();
    w.append_entry(oc_store::NewEntry {
        tool_call_id: Some("call_1".into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Tool, "Successfully installed x", 5)
    })
    .await
    .unwrap();

    let sent = messages_sent_after_seeding(store).await;

    let dispatch = sent
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Assistant && !m.tool_calls.is_empty())
        .unwrap_or_else(|| panic!("历史里必须出现带 tool_calls 的 assistant: {sent:?}"));
    assert_eq!(dispatch.tool_calls[0].id, "call_1");
    assert_eq!(dispatch.tool_calls[0].name, "sys");

    let result = sent
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Tool)
        .unwrap_or_else(|| panic!("工具结果应是原生 Tool 角色: {sent:?}"));
    assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(result.content, "Successfully installed x");

    // 关键：不再有被改头换面成用户发言的工具输出。
    assert!(
        !sent.iter().any(|m| m.content.contains("【历史工具结果】")),
        "有结构就不该走降级路径: {sent:?}"
    );
    // 顺序也必须是 dispatch 紧接结果，否则 provider 400。
    let di = sent.iter().position(|m| !m.tool_calls.is_empty()).unwrap();
    assert_eq!(
        sent[di + 1].role,
        oc_llm::MsgRole::Tool,
        "结果必须紧跟 dispatch: {sent:?}"
    );
}

/// 缺列的老记录（该列引入之前落的）仍走降级：不能硬造裸 `tool` 消息。
#[tokio::test]
async fn legacy_entries_still_downgrade() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::Assistant, "我看一下。", 3))
        .await
        .unwrap();
    // tool_call_id 为 NULL：老 schema 落的工具结果。
    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::Tool, "旧输出", 2))
        .await
        .unwrap();

    let sent = messages_sent_after_seeding(store).await;

    assert!(
        !sent.iter().any(|m| m.role == oc_llm::MsgRole::Tool),
        "无 id 的工具结果不能重放成裸 Tool 消息（provider 会 400）: {sent:?}"
    );
    assert!(
        sent.iter()
            .any(|m| m.role == oc_llm::MsgRole::User && m.content.contains("【历史工具结果】")),
        "缺 id 时应降级成带前缀的 user 文本: {sent:?}"
    );
}

/// 孤儿工具结果（前面没有匹配的 dispatch）整条删——半个对子会让 provider 400。
/// 预算截断正好切在对子中间时就会造出这种序列。
#[tokio::test]
async fn drops_orphan_tool_result() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::User, "问一句", 2))
        .await
        .unwrap();
    // 带 id 但前面没有发起它的 assistant：dispatch 被截断丢掉了。
    w.append_entry(oc_store::NewEntry {
        tool_call_id: Some("call_gone".into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Tool, "无主输出", 2)
    })
    .await
    .unwrap();

    let sent = messages_sent_after_seeding(store).await;
    assert!(
        !sent.iter().any(|m| m.role == oc_llm::MsgRole::Tool),
        "孤儿工具结果应被丢掉: {sent:?}"
    );
    assert!(
        !sent.iter().any(|m| m.content.contains("无主输出")),
        "孤儿是整条删，不是降级: {sent:?}"
    );
}

/// 端到端：真跑一轮工具调用 → 落库带结构 → 重启后重放成原生结构。
///
/// 前面几个用例是手工塞库造的序列，只验证读侧。这个把写侧也串上：缺陷当初就出在
/// 写侧（`run.rs` 落 assistant 时只写文本、丢掉调用本身）。
#[tokio::test]
async fn tool_round_persists_structure_and_survives_restart() {
    use oc_core::tool::ApprovalMode;
    use oc_llm::mock::{ScriptStep, SequencedMock};
    use oc_llm::{Delta, FinishReason};
    use oc_server::tools_bridge::ToolExecutor;
    use oc_tools::exec::ExecTool;
    use oc_tools::shell::Shell;
    use oc_tools::ToolRegistry;

    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(
        ApprovalMode::Allow,
        Duration::from_secs(10),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    )));
    let tools = ToolExecutor::new(Arc::new(reg));

    // 第一轮请求工具，第二轮给文本收尾。
    let scripts = vec![
        vec![
            ScriptStep {
                delay: Duration::ZERO,
                delta: Delta::ToolCall(oc_llm::types::ToolCallDelta {
                    call_id: "call-run".into(),
                    name: Some("exec".into()),
                    args_chunk: r#"{"command": "echo ran"}"#.into(),
                }),
            },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
        ],
        vec![
            ScriptStep { delay: Duration::ZERO, delta: Delta::Text("跑完了".into()) },
            ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
        ],
    ];

    let (tx, mut rx) = broadcast::channel(512);
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        test_cfg().with_tools(tools),
        Arc::new(SequencedMock::new(scripts)),
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("跑个命令".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(10)).await;

    // 落库侧：assistant 带 tool_calls，工具结果带 tool_call_id。
    let hist = w.load_transcript("main".into(), 100).await.unwrap();
    let dispatch = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Assistant && e.tool_calls.is_some())
        .unwrap_or_else(|| panic!("assistant 的工具调用必须落库: {hist:?}"));
    assert!(dispatch.tool_calls.as_deref().unwrap().contains("call-run"));
    let result = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Tool)
        .unwrap_or_else(|| panic!("工具结果应落库: {hist:?}"));
    assert_eq!(result.tool_call_id.as_deref(), Some("call-run"));

    // 重放侧：换个 provider 在同一个库上再起一轮（等价于重启后接着聊）。
    let sent = messages_sent_after_seeding(store).await;
    let replayed = sent
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Assistant && !m.tool_calls.is_empty())
        .unwrap_or_else(|| panic!("重启后历史里仍应有 tool_calls: {sent:?}"));
    assert_eq!(replayed.tool_calls[0].id, "call-run");
    assert!(
        sent.iter()
            .any(|m| m.role == oc_llm::MsgRole::Tool && m.tool_call_id.as_deref() == Some("call-run")),
        "工具结果应重放成原生 Tool 角色: {sent:?}"
    );
}

/// 悬空 dispatch（落了调用、结果还没落就崩了）：保留文本，摘掉 `tool_calls`。
#[tokio::test]
async fn strips_dangling_dispatch() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry {
        tool_calls: Some(r#"[{"id":"call_x","name":"sys","args":"{}"}]"#.into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Assistant, "我跑一下命令。", 4)
    })
    .await
    .unwrap();

    let sent = messages_sent_after_seeding(store).await;
    assert!(
        sent.iter().all(|m| m.tool_calls.is_empty()),
        "没有结果的 dispatch 必须摘掉 tool_calls: {sent:?}"
    );
    assert!(
        sent.iter().any(|m| m.content == "我跑一下命令。"),
        "文本本身是模型说过的话，应保留: {sent:?}"
    );
}

/// 已落库的空工具名不得原样重放——那正是 provider 400、会话永久卡死的来源。
/// 重放时替换成哨兵 `unknown_tool`，结构保持「dispatch + 结果」成对，模型能看懂。
#[tokio::test]
async fn replay_sanitizes_empty_tool_name() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::User, "跑一下", 3))
        .await
        .unwrap();
    // 真机脏数据的形状：name 为空串、args 却是完整的 exec 参数。
    w.append_entry(oc_store::NewEntry {
        tool_calls: Some(r#"[{"id":"call_bad","name":"","args":"{\"command\":\"ls\"}"}]"#.into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Assistant, "", 1)
    })
    .await
    .unwrap();
    w.append_entry(oc_store::NewEntry {
        tool_call_id: Some("call_bad".into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Tool, "未知工具: ", 2)
    })
    .await
    .unwrap();

    let sent = messages_sent_after_seeding(store).await;

    let dispatch = sent
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Assistant && !m.tool_calls.is_empty())
        .unwrap_or_else(|| panic!("应重放出带 tool_calls 的 assistant: {sent:?}"));
    assert_eq!(dispatch.tool_calls[0].id, "call_bad");
    assert_eq!(
        dispatch.tool_calls[0].name, "unknown_tool",
        "空工具名必须替换成哨兵，否则回喂 400: {sent:?}"
    );
    assert!(
        !sent.iter().any(|m| m.tool_calls.iter().any(|tc| tc.name.is_empty())),
        "到达模型的消息里不得有空函数名: {sent:?}"
    );
}

/// 模型在途吐出空工具名：不落 `""`，落哨兵；工具桥回「未知工具: unknown_tool」；
/// 下一轮回喂的 assistant 消息函数名非空（不再 400）。
#[tokio::test]
async fn empty_tool_name_from_model_is_sanitized_before_persist() {
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "call_x".into(),
                name: Some(String::new()), // 真机：function.name = ""
                args_chunk: r#"{"command":"ls"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ];
    let round2 = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("好的。".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(SequencedMock::new(vec![round1, round2]));
    let captures = provider.captures();

    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    // 空注册表：任何名字都是「未知工具」，正好走到 tools_bridge 的那条分支。
    let tools = ToolExecutor::new(Arc::new(ToolRegistry::new()));
    let (tx, mut rx) = broadcast::channel(512);
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        cfg().with_tools(tools),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("跑一下".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // ① 回喂第 2 轮的请求里，assistant 的函数名是哨兵而非空串。
    //    断言放进独立作用域：guard 不得跨下面 load_transcript 的 await（clippy）。
    {
        let reqs = captures.lock().unwrap();
        assert_eq!(reqs.len(), 2, "应有两轮模型请求（工具轮 + 收尾）");
        let dispatch = reqs[1]
            .messages
            .iter()
            .find(|m| !m.tool_calls.is_empty())
            .expect("第 2 轮请求应含工具调用历史");
        assert_eq!(dispatch.tool_calls[0].name, "unknown_tool");
        let result = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == oc_llm::MsgRole::Tool)
            .expect("应有工具结果");
        assert!(
            result.content.contains("未知工具: unknown_tool"),
            "工具桥应报出哨兵名，实际: {}",
            result.content
        );
    }

    // ② 落库的 dispatch 也是哨兵（否则下次重放又毒化）。
    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let persisted = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Assistant && e.tool_calls.is_some())
        .expect("应落库一条带 tool_calls 的 assistant");
    assert!(
        persisted.tool_calls.as_deref().unwrap().contains(r#""name":"unknown_tool""#),
        "落库的工具名应为哨兵: {:?}",
        persisted.tool_calls
    );
}
