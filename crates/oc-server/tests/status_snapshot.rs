//! `oc status` 快照必须反映真实运行时状态。
//!
//! 旧 bug：`dispatch::snapshot()` 把 `active_run` / `queued_turns` /
//! `background_tasks` 三项写死成 `None / 0 / 0`，于是无论车道上跑着什么、
//! 排了几轮、后台挂着几个进程，`oc status` 永远打「活跃run:- 排队:0 后台任务:0」。
//! 数据本身一直是有的（`DiagRegistry` 每次状态迁移都在更新，`TaskLedger` 记着
//! 每个后台进程），只是从没被接进这个快照。
//!
//! 本测试钉住「快照读的是活数据」这个不变量——三项各自有独立数据源，故分开断言。

use std::sync::Arc;
use std::time::Duration;

use oc_llm::mock::MockProvider;
use oc_proto::{ClientKind, Frame, Method, MethodOk, Req, ReqId, ResResult, SessionId, Snapshot};
use oc_server::testing::{test_cfg, test_state, SessionConfigExt};
use oc_server::ServerState;
use oc_tools::process::BackgroundHandoff;
use tokio::sync::{mpsc, oneshot};

/// 走真实分发路径取一次 `Method::Status` 的快照（与 `oc status` 同一条码路）。
async fn status(state: &Arc<ServerState>) -> Snapshot {
    // status 不产生内联事件，出站队列收不到东西；给个容量占位即可。
    let (out_tx, _out_rx) = mpsc::channel::<Frame>(8);
    let req = Req {
        id: ReqId::new("status-1"),
        method: Method::Status,
        idempotency_key: None,
    };
    match oc_server::dispatch::handle_req(&req, state, &out_tx, ClientKind::Interactive).await {
        ResResult::Ok(MethodOk::Status(s)) => s,
        other => panic!("期望 Status 应答，得到 {other:?}"),
    }
}

#[tokio::test]
async fn status_reports_active_run_and_queue_depth() {
    let store = oc_store::Store::open_memory().unwrap();
    // 首个 delta 前卡一小时：提交后这一轮必然仍占着车道，后续提交只能排队。
    let provider = Arc::new(MockProvider::stalls_for(Duration::from_secs(3600)));
    // 空闲看门狗放宽，免得它在测试期间把这轮中止、车道又空了。
    let cfg = test_cfg().with_idle_timeout(Duration::from_secs(600));
    let (state, reg, _rx) = test_state(provider, cfg, store);

    // 基线：什么都没跑时三项为空。若这里就非空，说明断言测的不是我们以为的东西。
    let s = status(&state).await;
    assert!(s.active_run.is_none(), "空闲时不该有活跃 run");
    assert_eq!(s.queued_turns, 0, "空闲时排队数应为 0");

    let h = reg.get_or_spawn(&SessionId::main());
    let first = h.submit("第一轮".into(), h.broadcast_sink()).await.expect("第一轮应起步");
    // actor 单线程按序处理：这条回执意味着第一轮的 begin_run（含 diag.run_start）
    // 已经走完，第二轮也已被判定为排队并写过 queue_depth。
    h.submit("第二轮".into(), h.broadcast_sink()).await.expect("第二轮应入队");

    let s = status(&state).await;
    assert_eq!(
        s.active_run.as_ref().map(|r| r.as_str()),
        Some(first.as_str()),
        "活跃 run 应是占着车道的第一轮"
    );
    assert_eq!(s.queued_turns, 1, "排在后面的第二轮应计入排队数");
}

#[tokio::test]
async fn status_counts_unfinished_background_tasks() {
    let store = oc_store::Store::open_memory().unwrap();
    let (state, _reg, _rx) = test_state(Arc::new(MockProvider::echo_text("ok")), test_cfg(), store);

    assert_eq!(status(&state).await.background_tasks, 0, "台账空时应为 0");

    // 直接往台账登记一个「还没结束」的后台任务：不 drop 输出端、不发退出码，
    // 台账里的跟踪任务就停在 Running。
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let (done_tx, done_rx) = oneshot::channel::<i32>();
    state.ledger().register(BackgroundHandoff {
        command: "长命令".into(),
        output: out_rx,
        done: done_rx,
    });

    assert_eq!(status(&state).await.background_tasks, 1, "在跑的后台任务应计入");

    // 让它结束：输出流关闭 → 台账收退出码 → 状态转 Done。
    drop(out_tx);
    done_tx.send(0).expect("发退出码");

    // 状态翻转由台账的后台任务完成，轮询等它（不用固定 sleep，免得偶发失败）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if status(&state).await.background_tasks == 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "后台任务结束后不应再计入");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
