//! run 内联事件出口（P0-1 事件丢失的修复核心）。
//!
//! **问题**：原设计 run 的所有事件都发进一条全局共享的 `broadcast`（cap 256）。
//! 流式回复每 token 一条 `Assistant` delta，长回复轻松超 256；任一慢连接拖慢转发
//! 即 `Lagged`，被覆盖的 delta **永久丢失**——表现为「长回复中途截断」。
//!
//! **修法**：把 run 的**内联事件**（在对话流里按顺序渲染的：Assistant / Lifecycle /
//! Tool / Approval）从广播挪到一条**每 run 专属、有界、背压**的通道，直接接到发起
//! `chat.send` 的那条连接的出站队列。慢客户端 → 出站队列满 → `send().await` 挂起
//! → provider 停止被拉取（TCP 反压回模型侧）→ **不丢字**。
//!
//! **为何不全挪**：`Usage`（一轮一条，不会 Lagged，且有独立订阅者更新 state）与
//! `Proactive`（cron/心跳带外通知，与任何 run 无关）留在广播——低频广播几乎不 Lagged，
//! 且省一条通知路径。二分见 [06-zeroclaw流式传输调研.md] 的「两类通道职责二分」。
//!
//! **枚举而非 trait object**：变体少、无动态扩展需求，`enum` 比 `Box<dyn Fn>`
//! 性能更好（无虚调用/堆分配）、更符合 Rust 穷尽匹配规范。

use oc_proto::{Event, Frame};
use tokio::sync::{broadcast, mpsc};

/// run 内联事件的出口。
///
/// - [`RunSink::Conn`]：生产路径。事件包成 `Frame` 送发起连接的出站队列，**有界背压**。
/// - [`RunSink::Broadcast`]：测试/带外路径。沿用广播语义（多订阅者、慢则 Lagged）。
///   run driver 单测直接 `subscribe()` 断言事件序列时用它。
/// - [`RunSink::Detached`]：无状态网关（Web/HTTP）路径。连接活着时照常转发（流式完整），
///   断连后吞掉 send 失败仍返回成功，run 继续跑完落库。
#[derive(Clone)]
pub enum RunSink {
    /// 定向到单条连接的出站队列（有界背压，不丢）。
    Conn(mpsc::Sender<Frame>),
    /// 广播（测试断言 / 沿用旧语义）。
    Broadcast(broadcast::Sender<Event>),
    /// 无状态网关（Detached 客户端）：持有连接发送端 + 事件日志。
    ///
    /// - 连接活着时：事件照常转发（流式完整）；
    /// - 断连后：`tx.send` 失败被吞掉、仍返回 true，run 继续跑完落库；
    /// - 所有内联事件同时写入 `log`（回放源），刷新后新连接经它接续剩余流。
    Detached { tx: mpsc::Sender<Frame>, log: std::sync::Arc<crate::run_log::RunLog> },
}

impl RunSink {
    /// 发一个内联事件。返回 `false` 表示下游已不可达（连接断开）——
    /// 调用方（run driver）应据此中止 run，避免车道被死连接锁死。
    ///
    /// `Conn` 分支在出站队列满时 `send().await` 挂起（背压）；driver 侧用
    /// `select!` 叠加 cancel，使背压期间仍能响应 abort/看门狗（见 run.rs）。
    pub async fn send(&self, ev: Event) -> bool {
        match self {
            // Detached：连接活着时正常转发；断连后 send 失败被吞掉、仍返回 true，
            // 不触发 run driver 的「send 失败 → cancel」探测（run 继续跑完落库）。
            // 事件同时写入 log（刷新后接续剩余流的回放源）。
            RunSink::Detached { tx, log } => {
                log.push(ev.clone());
                let _ = tx.send(Frame::Event(ev)).await;
                true
            }
            RunSink::Conn(tx) => tx.send(Frame::Event(ev)).await.is_ok(),
            // 广播 send 仅在无接收端时 Err；测试里接收端一直在，视为始终可达。
            RunSink::Broadcast(tx) => {
                let _ = tx.send(ev);
                true
            }
        }
    }

    /// 等待下游连接关闭。用于 run 的**静默等待期**（ask_user / 审批：卡在等回执，
    /// 期间不 send，故无法靠 send 失败探测断连）——在等待的 `select!` 里叠这条，
    /// client 掉线时立即感知并收敛 run，不必干等到空闲看门狗兜底。
    ///
    /// `Conn`：出站队列的接收端（写任务）drop 时完成。`Broadcast`：永久挂起
    /// （广播无单一下游，测试路径不需要断连收敛）。
    pub async fn closed(&self) {
        match self {
            RunSink::Conn(tx) => tx.closed().await,
            RunSink::Broadcast(_) => std::future::pending().await,
            RunSink::Detached { .. } => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_log::RunLog;
    use oc_proto::{Event, LifecyclePhase, SessionId};

    #[tokio::test]
    async fn detached_send_always_succeeds() {
        // rx 保留（未 drop），tx 未关闭：send 应成功。即便下游断开，Detached 仍吞掉
        // 失败返回 true，故这里也顺带验证「未关闭时正常送达」路径恒成功。
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let sink = RunSink::Detached { tx, log: std::sync::Arc::new(RunLog::new()) };
        let ok = sink
            .send(Event::Lifecycle {
                session: SessionId::main(),
                run_id: oc_proto::RunId::new("run"),
                phase: LifecyclePhase::Start,
            })
            .await;
        assert!(ok, "Detached sink 的 send 必须恒成功，才不会触发断连收敛");
    }

    #[tokio::test]
    async fn detached_send_records_into_runlog() {
        let log = std::sync::Arc::new(RunLog::new());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let sink = RunSink::Detached { tx, log: log.clone() };
        let ok = sink
            .send(Event::Assistant {
                session: SessionId::main(),
                run_id: oc_proto::RunId::new("r"),
                delta: "你".into(),
            })
            .await;
        assert!(ok);
        let mut sub = log.subscribe(false);
        let ev = sub.try_recv().expect("sink.send 应写入 RunLog");
        assert!(matches!(&ev, Event::Assistant { delta, .. } if delta == "你"));
    }
}
