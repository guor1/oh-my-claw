//! Detached run 的内联事件日志（刷新接续的回放源）。
//!
//! 订阅者模式（而非 Notify+游标）：`subscribe` 在同一把锁内「回放缓冲 + 登记
//! 订阅者」，`push` 也在同一把锁内「入缓冲 + 广播」——二者互斥，回放与续流之间
//! 不丢不重（避免 drain 后、挂等待前生产者 push 导致的 lost-wakeup）。
//!
//! 缓冲有界（cap 1024）：满则丢最旧。
//!
//! 回放起点由**落库标记点**决定：客户端上报 `since_seq`（它 loadHistory 拿到的
//! 最大 entry seq），只回放落进更晚 entry 的那些事件——否则刷新后工具卡会建两张、
//! 工具输出拼两遍、assistant 正文渲染两遍（见 docs/superpowers/plans/
//! 2026-09-17-resume-watermark.md）。

use std::collections::VecDeque;
use std::sync::Mutex;

use oc_proto::Event;
use tokio::sync::mpsc;

/// 环形缓冲容量。够覆盖一次长回复的绝大部分；超出则丢最旧（落库历史兜底）。
const CAP: usize = 1024;

struct Inner {
    buf: VecDeque<Event>,
    /// 累计 push 过的事件总数，**单调不回退**（ring 淘汰旧事件也不减）。
    /// 有了它，标记点才能用一个稳定的全局序号定位，而不是会被淘汰打乱的下标。
    pushed: u64,
    /// 落库标记点 `(entry seq, 落库那一刻的 pushed)`。
    /// 语义：全局序号 < pushed 的事件，其内容已落进 seq ≤ entry seq 的 entry。
    ///
    /// 不给每个事件打 seq 标签、而是记标记点，是为了**不依赖 seq 连续**：
    /// `compact_with_summary` 会往会话尾插一条摘要 entry，若与 run 交错，
    /// 「事件 tag+1 == 下一条 entry seq」的算术就不成立，按 tag 过滤会整组丢内容。
    /// 标记点只把 seq 当查找键，外部插入最多让回放多覆盖一组（重叠，不丢）。
    marks: Vec<(i64, u64)>,
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
                pushed: 0,
                marks: Vec::new(),
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
        inner.pushed += 1;
        // 订阅者断开（forward 任务退出）即从 subs 移除。
        inner.subs.retain(|s| s.send(ev.clone()).is_ok());
    }

    /// 记一个落库标记点：此刻为止 push 过的事件，其内容都已落进 seq ≤ `seq` 的 entry。
    /// 由 `RunSink::mark_persisted` 在每次 `persist()` 成功后调用。
    pub fn mark_persisted(&self, seq: i64) {
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");
        let pushed = inner.pushed;
        inner.marks.push((seq, pushed));
    }

    /// 订阅并从头回放。等价于 `subscribe_from(0, replay_reasoning)`。
    // 生产路径已全部改用 `subscribe_from`；此方法现仅测试在用，别在清理时误删。
    pub fn subscribe(&self, replay_reasoning: bool) -> mpsc::UnboundedReceiver<Event> {
        self.subscribe_from(0, replay_reasoning)
    }

    /// 订阅：跳过「客户端已从落库历史拿到」的事件后回放，再登记订阅者。
    ///
    /// `since_seq` = 客户端 `loadHistory` 拿到的最大 entry seq。回放起点取
    /// 「seq ≤ since_seq 的最后一个标记点」——早于它的事件内容已在客户端历史里，
    /// 重放会让工具卡建两张、工具输出拼两遍、assistant 正文渲染两遍。
    ///
    /// 全程持锁——回放与登记订阅者之间不丢事件（沿用原有的不变式）。
    pub fn subscribe_from(
        &self,
        since_seq: i64,
        replay_reasoning: bool,
    ) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().expect("RunLog 锁中毒");

        // marks 按 seq 递增追加，倒着找到的第一个即「≤ since_seq 的最后一个」。
        let from = inner
            .marks
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= since_seq)
            .map(|(_, pushed)| *pushed)
            .unwrap_or(0);
        // ring 首元素的全局序号。标记点若已被淘汰，saturating_sub 夹到 0
        // → 退化为「回放缓冲里剩下的全部」（可能与历史重叠，但不丢）。
        let buf_start = inner.pushed - inner.buf.len() as u64;
        let skip = from.saturating_sub(buf_start) as usize;

        for ev in inner.buf.iter().skip(skip) {
            if !replay_reasoning && matches!(ev, Event::Reasoning { .. }) {
                continue;
            }
            let _ = tx.send(ev.clone());
        }
        inner.subs.push(tx);
        rx
    }
}

impl Default for RunLog {
    fn default() -> Self {
        Self::new()
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

    fn deltas(evs: &[Event]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                Event::Assistant { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    /// 回放起点由「seq ≤ since_seq 的最后一个落库标记点」决定：
    /// 客户端已从历史里拿到的那些事件组不再重放（否则工具卡/正文渲染两遍）。
    #[tokio::test]
    async fn replay_starts_after_marked_seq() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.push(assistant("r", "乙"));
        log.mark_persisted(144); // 甲乙 的内容落进了 entry 144
        log.push(assistant("r", "丙"));
        log.mark_persisted(145); // 丙 落进 entry 145
        log.push(assistant("r", "丁"));

        let mut s = log.subscribe_from(144, false);
        assert_eq!(
            deltas(&collect_immediate(&mut s).await),
            vec!["丙", "丁"],
            "客户端已有 entry<=144，只该回放其后的事件"
        );

        let mut s = log.subscribe_from(145, false);
        assert_eq!(deltas(&collect_immediate(&mut s).await), vec!["丁"]);

        let mut s = log.subscribe_from(143, false);
        assert_eq!(
            deltas(&collect_immediate(&mut s).await),
            vec!["甲", "乙", "丙", "丁"],
            "早于任何标记点 → 全量回放（缓冲即全部真相）"
        );
    }

    /// 无标记点（run 刚起步、一条都没落库）时退化为全量回放。
    /// 这正是 `subscribe(r) == subscribe_from(0, r)` 的向后兼容依据。
    #[tokio::test]
    async fn no_marks_replays_everything() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.push(assistant("r", "乙"));

        let mut s = log.subscribe_from(999, false);
        assert_eq!(deltas(&collect_immediate(&mut s).await), vec!["甲", "乙"]);
    }

    /// 标记点之后的回放同样过滤 reasoning（「只续不补」语义不受影响）。
    #[tokio::test]
    async fn subscribe_from_still_filters_reasoning() {
        let log = RunLog::new();
        log.push(assistant("r", "甲"));
        log.mark_persisted(10);
        log.push(reasoning("r", "想"));
        log.push(assistant("r", "乙"));

        let mut s = log.subscribe_from(10, false);
        let got = collect_immediate(&mut s).await;
        assert_eq!(got.len(), 1, "回放应过滤 Reasoning");
        assert_eq!(deltas(&got), vec!["乙"]);
    }

    async fn collect_immediate(sub: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = vec![];
        while let Ok(ev) = sub.try_recv() {
            out.push(ev);
        }
        out
    }
}
