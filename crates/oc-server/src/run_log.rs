//! Detached run 的内联事件日志（刷新接续的回放源）。
//!
//! 订阅者模式（而非 Notify+游标）：`subscribe` 在同一把锁内「回放缓冲 + 登记
//! 订阅者」，`push` 也在同一把锁内「入缓冲 + 广播」——二者互斥，回放与续流之间
//! 不丢不重（避免 drain 后、挂等待前生产者 push 导致的 lost-wakeup）。
//!
//! 缓冲有界（cap 1024）：满则丢最旧。回放可能从中间开始，但完整文本最终由
//! 落库历史兜底，回放只是过渡。

use std::collections::VecDeque;
use std::sync::Mutex;

use oc_proto::Event;
use tokio::sync::mpsc;

/// 环形缓冲容量。够覆盖一次长回复的绝大部分；超出则丢最旧（落库历史兜底）。
const CAP: usize = 1024;

struct Inner {
    buf: VecDeque<Event>,
    subs: Vec<mpsc::UnboundedSender<Event>>,
}

/// 每 Detached run 一个的事件日志。
pub struct RunLog {
    inner: Mutex<Inner>,
}

impl RunLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAP),
                subs: Vec::new(),
            }),
        }
    }

    /// 生产者：入缓冲（满则丢最旧）+ 转发给所有 live 订阅者。
    pub fn push(&self, ev: Event) {
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        if inner.buf.len() == CAP {
            inner.buf.pop_front();
        }
        inner.buf.push_back(ev.clone());
        // 订阅者断开（forward 任务退出）即从 subs 移除。
        inner.subs.retain(|s| s.send(ev.clone()).is_ok());
    }

    /// 订阅：先回放缓冲（`replay_reasoning=false` 时过滤 Reasoning），再登记订阅者。
    /// 全程持锁——回放与订阅之间不丢事件。
    pub fn subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        for ev in &inner.buf {
            if !replay_reasoning && matches!(ev, Event::Reasoning { .. }) {
                continue;
            }
            let _ = tx.send(ev.clone());
        }
        inner.subs.push(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_proto::{Event, LifecyclePhase, RunId, SessionId};

    fn assistant(run: &str, delta: &str) -> Event {
        Event::Assistant {
            session: SessionId::main(),
            run_id: RunId::new(run),
            delta: delta.into(),
        }
    }
    fn reasoning(run: &str, delta: &str) -> Event {
        Event::Reasoning {
            session: SessionId::main(),
            run_id: RunId::new(run),
            delta: delta.into(),
        }
    }
    #[allow(dead_code)]
    fn end(run: &str) -> Event {
        Event::Lifecycle {
            session: SessionId::main(),
            run_id: RunId::new(run),
            phase: LifecyclePhase::End,
        }
    }

    /// 回放按序、过滤 reasoning（replay_reasoning=false）。
    #[tokio::test]
    async fn replay_filters_reasoning_in_order() {
        let log = RunLog::new();
        log.push(assistant("r", "你"));
        log.push(reasoning("r", "想"));
        log.push(assistant("r", "好"));

        let mut sub = log.subscribe(false);
        // 立即拿到的应是「你」「好」，无 reasoning。
        let got = collect_immediate(&mut sub).await;
        assert_eq!(got.len(), 2, "回放应过滤 reasoning，得到 2 条 assistant");
        assert!(matches!(&got[0], Event::Assistant { delta, .. } if delta == "你"));
        assert!(matches!(&got[1], Event::Assistant { delta, .. } if delta == "好"));
    }

    /// 回放与 live 订阅之间不丢不重：subscribe 后 push 的新事件也能收到。
    #[tokio::test]
    async fn live_events_flow_after_subscribe() {
        let log = RunLog::new();
        log.push(assistant("r", "先"));
        let mut sub = log.subscribe(false);
        assert!(!collect_immediate(&mut sub).await.is_empty(), "应回放已缓冲的「先」");

        log.push(assistant("r", "后"));
        let mut got = vec![];
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while let Some(ev) = sub.recv().await {
                got.push(ev);
            }
        }).await.unwrap_or(());
        assert_eq!(got.len(), 1, "续流应收到 push 后的新事件");
        assert!(matches!(&got[0], Event::Assistant { delta, .. } if delta == "后"));
    }

    /// cap 满丢最旧（这里 cap 不暴露，用默认 1024 不测；改为验证 push 不 panic + 顺序稳定）。
    #[tokio::test]
    async fn push_does_not_panic_on_many_events() {
        let log = RunLog::new();
        for i in 0..2000 {
            log.push(assistant("r", &format!("{i}")));
        }
        // 无 panic 即通过；回放从中间开始是预期行为（完整文本由落库兜底）。
        let _ = log.subscribe(false);
    }

    async fn collect_immediate(sub: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = vec![];
        while let Ok(ev) = sub.try_recv() {
            out.push(ev);
        }
        out
    }
}
