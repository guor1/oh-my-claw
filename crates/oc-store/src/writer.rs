//! 单写线程 actor（设计 §3.1）。
//!
//! 一个专用 OS 线程持有唯一可写 `Connection`，通过 mpsc 命令队列**串行**执行所有写。
//! 贴合"事务内不 await"：写线程是同步的，事务体内纯 rusqlite 调用。
//! 上层用异步接口投递命令并 `oneshot` 等回执。
//!
//! **读命令为何还在这里**：读的生产路径是 [`crate::reader`] 的连接池（P2-1）。
//! 但内存库无法跨连接共享（见 [`crate::Store`]），故测试用的内存库把读回落到
//! 本线程执行。回落路径与生产路径共用 `ops::*`，行为一致，只是不并行。
//!
//! **健康位（P2-2）**：线程内挂一个 [`HealthGuard`]，drop 时把 `alive` 置 false。
//! 这样 panic 展开与正常关停都会被记上，`Writer::send` 得以立刻回
//! [`StoreError::WriterDead`] 而不是让调用方卡在"写线程无响应"这种含糊错误上。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::error::{StoreError, StoreResult};
use crate::ops;
use crate::types::{NewEntry, NewMemory};

/// 写命令：每个变更一种，带 oneshot 回执。
pub enum WriteCmd {
    EnsureSession {
        id: String,
        kind: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    AppendEntry {
        entry: NewEntry,
        reply: oneshot::Sender<StoreResult<i64>>,
    },
    ResetSession {
        id: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    SessionList {
        reply: oneshot::Sender<StoreResult<Vec<crate::types::SessionRow>>>,
    },
    CompactWithSummary {
        session_id: String,
        up_to_seq: i64,
        summary_text: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    UpsertMemory {
        mem: NewMemory,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    TouchMemory {
        id: String,
        at: i64,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    // ── 读命令（单用户下与写共线程串行，简单可靠；将来可拆读连接池）──
    LoadTranscript {
        session_id: String,
        max_entries: i64,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::Entry>>>,
    },
    SearchCandidates {
        query_terms: Vec<String>,
        tier_filter: Option<crate::types::Tier>,
        limit: i64,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::MemoryRow>>>,
    },
    DreamCandidates {
        limit: i64,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::MemoryRow>>>,
    },
    PromoteMemory {
        id: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    MergeMemory {
        source_id: String,
        target_id: String,
        merged_text: String,
        merged_content_hash: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CuratedList {
        reply: oneshot::Sender<StoreResult<Vec<(String, String)>>>,
    },
    MemoryByPrefKey {
        key: String,
        reply: oneshot::Sender<StoreResult<Vec<crate::types::MemoryRow>>>,
    },
    DeleteMemory {
        id: String,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    WriteAudit {
        actor: String,
        action: String,
        payload: Option<String>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CronAdd {
        cron: crate::types::NewCron,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CronList {
        reply: oneshot::Sender<StoreResult<Vec<crate::types::CronRow>>>,
    },
    CronRm {
        id: String,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    CronMarkFired {
        id: String,
        fired_at: i64,
        next_at: Option<i64>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    IntentAdd {
        intent: crate::types::NewStandingIntent,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    IntentList {
        reply: oneshot::Sender<StoreResult<Vec<crate::types::StandingIntentRow>>>,
    },
    IntentRm {
        id: String,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    IntentMarkFired {
        id: String,
        fired_at: i64,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    /// 用于优雅关停。
    Shutdown,
    /// 健康探针：命令真的排到队头并被执行才回。
    ///
    /// 与 `alive` 标志互补——标志答"线程还在吗"，这条答"它还在推进队列吗"
    /// （长事务卡住时线程活着但不动）。
    Ping {
        reply: oneshot::Sender<StoreResult<()>>,
    },
    /// 仅测试：让写线程 panic，验证健康位与 `WriterDead`。
    #[cfg(test)]
    PanicForTest,
}

/// 写线程句柄。
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<WriteCmd>,
    /// 写线程存活标志。线程退出（含 panic 展开）时由 [`HealthGuard`] 置 false。
    alive: Arc<AtomicBool>,
}

/// 写线程退出时翻健康位。
///
/// 用 RAII 而非在 `run_loop` 末尾赋值：panic 展开**不会**走到函数末尾，
/// 而 panic 恰恰是本机制最想覆盖的情形。
struct HealthGuard(Arc<AtomicBool>);

impl Drop for HealthGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Writer {
    /// 启动写线程。`db` 为 None 时用内存库（测试）。
    pub fn spawn(db: Option<PathBuf>) -> StoreResult<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<WriteCmd>();
        // 用 std 线程 + 一个就绪回执，确保建库/迁移成功后才返回。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<StoreResult<()>>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = alive.clone();

        thread::Builder::new()
            .name("oc-store-writer".into())
            .spawn(move || {
                let _guard = HealthGuard(alive_thread);
                let conn = match open_writer_conn(db) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run_loop(conn, &mut rx);
            })
            .map_err(StoreError::Io)?;

        ready_rx
            .recv()
            .map_err(|_| StoreError::Migration("写线程启动失败".into()))??;
        Ok(Self { tx, alive })
    }

    /// 写线程是否仍存活。**便宜**（一次原子读），可在每个写操作前调。
    ///
    /// 注意它只答"线程没退出"，不答"队列在推进"——后者用 [`Writer::ping`]。
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// 端到端健康探针：投一条 no-op 并等它被执行。
    pub async fn ping(&self) -> StoreResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WriteCmd::Ping { reply })?;
        rx.await.map_err(|_| StoreError::WriterDead)?
    }

    fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        if !self.is_alive() {
            return Err(StoreError::WriterDead);
        }
        self.tx.send(cmd).map_err(|_| StoreError::WriterDead)
    }
}

/// 生成一个转发方法：投命令 → 等回执。
///
/// 22 个方法此前逐字重复同样四行，改一次错误语义要改 22 处
/// （P2-2 把 `Migration("写线程无响应")` 换成 `WriterDead` 时正是如此）。
macro_rules! writer_call {
    ($(#[$m:meta])* $name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty => $variant:ident { $($field:ident),* $(,)? }) => {
        $(#[$m])*
        pub async fn $name(&self, $($arg: $ty),*) -> StoreResult<$ret> {
            let (reply, rx) = oneshot::channel();
            self.send(WriteCmd::$variant { $($field,)* reply })?;
            // oneshot 被 drop 只可能是写线程在执行这条命令时死了。
            rx.await.map_err(|_| StoreError::WriterDead)?
        }
    };
}

impl Writer {
    writer_call!(ensure_session(id: String, kind: String) -> () => EnsureSession { id, kind });
    /// 追加 entry，返回其**会话内 seq**。
    writer_call!(append_entry(entry: NewEntry) -> i64 => AppendEntry { entry });
    writer_call!(reset_session(id: String) -> () => ResetSession { id });
    writer_call!(session_list() -> Vec<crate::types::SessionRow> => SessionList {});
    writer_call!(
        compact_with_summary(session_id: String, up_to_seq: i64, summary_text: String) -> ()
        => CompactWithSummary { session_id, up_to_seq, summary_text }
    );
    writer_call!(upsert_memory(mem: NewMemory) -> () => UpsertMemory { mem });
    writer_call!(touch_memory(id: String, at: i64) -> () => TouchMemory { id, at });
    writer_call!(
        load_transcript(session_id: String, max_entries: i64) -> Vec<crate::types::Entry>
        => LoadTranscript { session_id, max_entries }
    );
    writer_call!(
        search_candidates(
            query_terms: Vec<String>,
            tier_filter: Option<crate::types::Tier>,
            limit: i64,
        ) -> Vec<crate::types::MemoryRow>
        => SearchCandidates { query_terms, tier_filter, limit }
    );
    writer_call!(dream_candidates(limit: i64) -> Vec<crate::types::MemoryRow> => DreamCandidates { limit });
    writer_call!(promote_memory(id: String) -> () => PromoteMemory { id });

    writer_call!(
        /// 把一条新记忆合并进既有 curated（FEAT-4 四动作的非 create 分支）。
        merge_memory(
            source_id: String,
            target_id: String,
            merged_text: String,
            merged_content_hash: String,
        ) -> () => MergeMemory { source_id, target_id, merged_text, merged_content_hash }
    );

    writer_call!(
        /// 取全部 curated（id + text），供巩固模型轮判断动作落点。
        curated_list() -> Vec<(String, String)> => CuratedList {}
    );

    writer_call!(
        /// 按偏好主题取既有偏好（供 supersede 判冲突）。
        memory_by_pref_key(key: String) -> Vec<crate::types::MemoryRow> => MemoryByPrefKey { key });

    writer_call!(
        /// 删除一条记忆（supersede 的 Replace 清旧偏好）。
        delete_memory(id: String) -> bool => DeleteMemory { id });

    writer_call!(
        write_audit(actor: String, action: String, payload: Option<String>) -> ()
        => WriteAudit { actor, action, payload }
    );
    writer_call!(cron_add(cron: crate::types::NewCron) -> () => CronAdd { cron });
    writer_call!(cron_list() -> Vec<crate::types::CronRow> => CronList {});
    writer_call!(cron_rm(id: String) -> bool => CronRm { id });
    writer_call!(
        cron_mark_fired(id: String, fired_at: i64, next_at: Option<i64>) -> ()
        => CronMarkFired { id, fired_at, next_at }
    );
    writer_call!(intent_add(intent: crate::types::NewStandingIntent) -> () => IntentAdd { intent });
    writer_call!(intent_list() -> Vec<crate::types::StandingIntentRow> => IntentList {});
    writer_call!(intent_rm(id: String) -> bool => IntentRm { id });
    writer_call!(intent_mark_fired(id: String, fired_at: i64) -> () => IntentMarkFired { id, fired_at });
}

fn open_writer_conn(db: Option<PathBuf>) -> StoreResult<Connection> {
    let conn = match db {
        Some(path) => Connection::open(path)?,
        None => Connection::open_in_memory()?,
    };
    crate::apply_startup_pragmas(&conn)?;
    let mut conn = conn;
    crate::migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

fn run_loop(conn: Connection, rx: &mut mpsc::UnboundedReceiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::EnsureSession { id, kind, reply } => {
                let _ = reply.send(ops::ensure_session(&conn, &id, &kind));
            }
            WriteCmd::AppendEntry { entry, reply } => {
                let _ = reply.send(ops::append_entry(&conn, &entry));
            }
            WriteCmd::ResetSession { id, reply } => {
                let _ = reply.send(ops::reset_session(&conn, &id));
            }
            WriteCmd::SessionList { reply } => {
                let _ = reply.send(ops::session_list(&conn));
            }
            WriteCmd::CompactWithSummary { session_id, up_to_seq, summary_text, reply } => {
                let _ = reply.send(ops::compact_with_summary(&conn, &session_id, up_to_seq, &summary_text));
            }
            WriteCmd::UpsertMemory { mem, reply } => {
                let _ = reply.send(ops::upsert_memory(&conn, &mem));
            }
            WriteCmd::TouchMemory { id, at, reply } => {
                let _ = reply.send(ops::touch_memory(&conn, &id, at));
            }
            WriteCmd::LoadTranscript { session_id, max_entries, reply } => {
                let _ = reply.send(ops::load_transcript(&conn, &session_id, max_entries));
            }
            WriteCmd::SearchCandidates { query_terms, tier_filter, limit, reply } => {
                let _ = reply.send(ops::search_candidates(&conn, &query_terms, tier_filter, limit));
            }
            WriteCmd::DreamCandidates { limit, reply } => {
                let _ = reply.send(ops::dream_candidates(&conn, limit));
            }
            WriteCmd::PromoteMemory { id, reply } => {
                let _ = reply.send(ops::promote_memory(&conn, &id));
            }
            WriteCmd::MergeMemory { source_id, target_id, merged_text, merged_content_hash, reply } => {
                let _ = reply.send(ops::merge_memory(&conn, &source_id, &target_id, &merged_text, &merged_content_hash));
            }
            WriteCmd::CuratedList { reply } => {
                let _ = reply.send(ops::curated_list(&conn));
            }
            WriteCmd::MemoryByPrefKey { key, reply } => {
                let _ = reply.send(ops::memory_by_pref_key(&conn, &key));
            }
            WriteCmd::DeleteMemory { id, reply } => {
                let _ = reply.send(ops::delete_memory(&conn, &id));
            }
            WriteCmd::WriteAudit { actor, action, payload, reply } => {
                let _ = reply.send(ops::write_audit(&conn, &actor, &action, payload.as_deref()));
            }
            WriteCmd::CronAdd { cron, reply } => {
                let _ = reply.send(ops::cron_add(&conn, &cron));
            }
            WriteCmd::CronList { reply } => {
                let _ = reply.send(ops::cron_list(&conn));
            }
            WriteCmd::CronRm { id, reply } => {
                let _ = reply.send(ops::cron_rm(&conn, &id));
            }
            WriteCmd::CronMarkFired { id, fired_at, next_at, reply } => {
                let _ = reply.send(ops::cron_mark_fired(&conn, &id, fired_at, next_at));
            }
            WriteCmd::IntentAdd { intent, reply } => {
                let _ = reply.send(ops::intent_add(&conn, &intent));
            }
            WriteCmd::IntentList { reply } => {
                let _ = reply.send(ops::intent_list(&conn));
            }
            WriteCmd::IntentRm { id, reply } => {
                let _ = reply.send(ops::intent_rm(&conn, &id));
            }
            WriteCmd::IntentMarkFired { id, fired_at, reply } => {
                let _ = reply.send(ops::intent_mark_fired(&conn, &id, fired_at));
            }
            WriteCmd::Ping { reply } => {
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            WriteCmd::PanicForTest => panic!("故意 panic（写线程自愈测试）"),
            WriteCmd::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等健康位翻假（线程退出是异步的）。返回是否等到。
    ///
    /// 用 `std::thread::sleep` 而非 `tokio::time`：本 crate 的 tokio 只开了
    /// `sync`/`rt` 两个 feature，为一句测试等待去加 `time` 不划算。
    /// 写线程是 OS 线程，短暂阻塞测试线程不妨碍它推进。
    async fn wait_dead(w: &Writer) -> bool {
        for _ in 0..200 {
            if !w.is_alive() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// P2-2 的核心：写线程 **panic** 后，后续写立刻拿到明确的 `WriterDead`。
    ///
    /// 改动前的行为是：`JoinHandle` 被丢弃、无人知情，调用方只会收到
    /// `Migration("写线程无响应")`——分不清"这条写失败了"和"写侧整体没了"。
    #[tokio::test]
    async fn panicked_writer_reports_writer_dead() {
        let w = Writer::spawn(None).expect("启动写线程");
        assert!(w.is_alive());
        w.ping().await.expect("健康探针应通");

        w.tx.send(WriteCmd::PanicForTest).expect("投 panic 命令");
        assert!(wait_dead(&w).await, "线程 panic 后健康位应翻假");

        let err = w.ensure_session("main".into(), "main".into()).await;
        assert!(
            matches!(err, Err(StoreError::WriterDead)),
            "panic 后写应回 WriterDead，实际 {err:?}"
        );
        assert!(
            matches!(w.ping().await, Err(StoreError::WriterDead)),
            "panic 后健康探针也应回 WriterDead"
        );
    }

    /// 正常关停走同一个 [`HealthGuard`]，语义一致。
    #[tokio::test]
    async fn shutdown_also_marks_dead() {
        let w = Writer::spawn(None).expect("启动写线程");
        w.tx.send(WriteCmd::Shutdown).expect("投 shutdown");
        assert!(wait_dead(&w).await, "关停后健康位应翻假");
        assert!(matches!(
            w.append_entry(NewEntry::text("main", crate::types::Role::User, "x", 1))
                .await,
            Err(StoreError::WriterDead)
        ));
    }
}
