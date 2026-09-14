//! oc 单库存储（设计 §3）。
//!
//! 单库 `oc.sqlite` 装全部；schema 版本化 + 前向迁移；WAL；单写线程。
//! **store 只做持久化与查询执行，不做策略判定**（策略在 oc-core）。
//!
//! M1 落地：建库、迁移、WAL/PRAGMA、`user_version` 报告。
//! 单写线程 actor 与完整 Store trait 在 M2+ 补齐。

pub mod error;
pub mod fts;
pub mod migrate;
pub mod ops;
pub mod reader;
pub mod schema;
pub mod types;
pub mod writer;

pub use error::{StoreError, StoreResult};
pub use reader::Reader;
pub use types::*;
pub use writer::Writer;

use std::path::Path;

use rusqlite::Connection;

/// Store 门面：写走单写线程，读走独立连接池（P2-1）。
///
/// **读写分流**（设计 §3.1）：写由 [`Writer`] 的专用线程串行执行（唯一可写连接）；
/// 读由 [`Reader`] 的小连接池在 `spawn_blocking` 上跑。WAL 下二者互不阻塞，
/// 所以一次全表扫描不再拖住所有会话的落库。
///
/// **内存库的例外**：`reader` 为 `None` 时读回落到写线程。SQLite 的
/// `:memory:` 库**每条连接一个独立库**——池里的连接会看到一个空库，
/// 而共享它需要 `cache=shared` URI，那又会关掉 WAL 并引入
/// `SQLITE_LOCKED_SHAREDCACHE`（`busy_timeout` 管不着）。测试用内存库图的是
/// 快和干净，不值得为此换一套并发语义，故只在文件库上分流。
/// 生产路径（[`Store::open_path`]）始终有池。
#[derive(Clone)]
pub struct Store {
    writer: Writer,
    reader: Option<Reader>,
}

impl Store {
    /// 打开磁盘库并启动写线程 + 读连接池。
    pub fn open_path(path: std::path::PathBuf) -> StoreResult<Self> {
        // 先起写线程：它负责建库与迁移，读连接必须在那之后才开得出正确 schema。
        let writer = Writer::spawn(Some(path.clone()))?;
        Ok(Self {
            writer,
            reader: Some(Reader::new(path)),
        })
    }

    /// 打开内存库（测试）。读回落到写线程，见类型文档。
    pub fn open_memory() -> StoreResult<Self> {
        Ok(Self {
            writer: Writer::spawn(None)?,
            reader: None,
        })
    }

    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// 读连接池；内存库为 `None`（读回落写线程）。
    pub fn reader(&self) -> Option<&Reader> {
        self.reader.as_ref()
    }

    /// 写线程是否存活（一次原子读）。写侧死后读仍可用——降级而非全瘫。
    pub fn writer_alive(&self) -> bool {
        self.writer.is_alive()
    }

    /// 端到端写侧健康探针：投一条 no-op 并等它被执行。
    pub async fn writer_ping(&self) -> StoreResult<()> {
        self.writer.ping().await
    }
}

/// 生成一个读方法：有池走池（`spawn_blocking`），无池回落写线程。
///
/// 两条路径共用 `ops::*` 同一个函数，所以不存在"读池版本和写线程版本
/// 行为不一致"的风险——回落只是少了并行，不是另一套实现。
macro_rules! read_call {
    (
        $(#[$m:meta])*
        $name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty,
        pool: |$conn:ident| $body:expr
    ) => {
        $(#[$m])*
        pub async fn $name(&self, $($arg: $ty),*) -> StoreResult<$ret> {
            match &self.reader {
                Some(r) => r.read(move |$conn| $body).await,
                None => self.writer.$name($($arg),*).await,
            }
        }
    };
}

impl Store {
    read_call!(
        /// 取会话历史（正序，`reset_at` 之后、最多 `max_entries` 条）。
        load_transcript(session_id: String, max_entries: i64) -> Vec<types::Entry>,
        pool: |c| ops::load_transcript(c, &session_id, max_entries)
    );
    read_call!(
        /// Lane1 记忆检索的候选集（词法预筛，排名在 oc-core）。
        search_candidates(
            query_terms: Vec<String>,
            tier_filter: Option<types::Tier>,
            limit: i64,
        ) -> Vec<types::MemoryRow>,
        pool: |c| ops::search_candidates(c, &query_terms, tier_filter, limit)
    );
    read_call!(
        /// dreaming 的巩固候选（仅 episodic）。
        dream_candidates(limit: i64) -> Vec<types::MemoryRow>,
        pool: |c| ops::dream_candidates(c, limit)
    );
    read_call!(
        /// 按偏好主题取既有偏好（供 supersede 判冲突）。
        memory_by_pref_key(key: String) -> Vec<types::MemoryRow>,
        pool: |c| ops::memory_by_pref_key(c, &key)
    );
    read_call!(
        /// 全部会话（`oc sessions`）。
        session_list() -> Vec<types::SessionRow>,
        pool: |c| ops::session_list(c)
    );
    read_call!(
        /// 全部 cron 任务（心跳扫描 + `oc cron list`）。
        cron_list() -> Vec<types::CronRow>,
        pool: |c| ops::cron_list(c)
    );
    read_call!(
        /// 全部 standing intent（每轮入站消息预筛 + `oc intent list`）。
        intent_list() -> Vec<types::StandingIntentRow>,
        pool: |c| ops::intent_list(c)
    );
}

/// 打开（或创建）数据库，应用启动 PRAGMA 并跑前向迁移。
///
/// 返回一个已就绪的写连接。M2 起将其移入单写线程 actor。
pub fn open<P: AsRef<Path>>(path: P) -> StoreResult<Connection> {
    let mut conn = Connection::open(path)?;
    apply_startup_pragmas(&conn)?;
    migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

/// 打开内存库（测试用）。
pub fn open_in_memory() -> StoreResult<Connection> {
    let mut conn = Connection::open_in_memory()?;
    apply_startup_pragmas(&conn)?;
    migrate::run_migrations(&mut conn)?;
    Ok(conn)
}

/// 启动 PRAGMA（设计 §3.1）。
pub(crate) fn apply_startup_pragmas(conn: &Connection) -> StoreResult<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

/// 读取当前 schema 版本（`user_version` PRAGMA）。供 `oc doctor` 使用。
pub fn schema_version(conn: &Connection) -> StoreResult<u32> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(v as u32)
}

/// 校验库内实际结构是否跟得上当前代码。`Err(缺失项描述)` = 该删库重建。
///
/// **为什么版本号不够**：开发阶段的 schema 变更直接改建表 DDL、不写迁移步进，
/// 于是旧库的 `user_version` 已等于 `TARGET_VERSION`，迁移 no-op，但表结构是旧的。
/// 这种库能开、能报版本，却在第一次记忆操作时炸（`no such table: memory_fts`）。
/// 这里查的是**结构**而非版本，把那类失败提前到 `oc doctor`。
///
/// 只查最近改动涉及、且缺了就必然运行时报错的东西；不做全量 schema diff
/// （那要维护一份期望结构的副本，成本高且易与 DDL 漂移）。
pub fn check_shape(conn: &Connection) -> Result<(), String> {
    let mut missing = Vec::new();

    // P2-4：memory.text 的 FTS 索引 + memory 的显式 rowid 列。
    let has_fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'memory_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_fts == 0 {
        missing.push("memory_fts 表（记忆全文索引）");
    }
    // `no` 列：contentless FTS 靠它关联回主表。用一次实查而非翻 pragma——
    // 拿不到就是拿不到，无论原因。
    if conn.query_row("SELECT no FROM memory LIMIT 1", [], |_| Ok(())).is_err()
        // 空表时上面查不到行但不报列错，故再确认一次列存在。
        && conn.prepare("SELECT no FROM memory").is_err()
    {
        missing.push("memory.no 列（FTS 索引的关联键）");
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join("、"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_migrates_to_target() {
        let conn = open_in_memory().expect("open");
        let v = schema_version(&conn).expect("version");
        assert_eq!(v, migrate::TARGET_VERSION, "should migrate to target");
    }

    /// 新建的库必须通过形状校验——否则 `oc doctor` 会对着好库报"结构过旧"。
    #[test]
    fn fresh_db_passes_shape_check() {
        let conn = open_in_memory().expect("open");
        assert_eq!(check_shape(&conn), Ok(()), "刚建的库应通过形状校验");
    }

    /// 旧结构的库（`user_version` 已到目标值但表结构是老的）必须被认出来。
    ///
    /// 这是开发阶段"改 DDL 不写迁移"约定下的真实路径：版本号骗过了迁移，
    /// 只有查结构才拦得住。没有这一层，用户会在第一次写记忆时才看到
    /// `no such table: memory_fts`。
    #[test]
    fn stale_db_fails_shape_check() {
        let conn = Connection::open_in_memory().expect("open");
        // 老 DDL：TEXT 主键、无 memory_fts。
        conn.execute_batch(
            "CREATE TABLE memory(
               id TEXT PRIMARY KEY, tier TEXT NOT NULL, origin TEXT NOT NULL,
               text TEXT NOT NULL, keywords TEXT, importance REAL NOT NULL DEFAULT 0.5,
               created_at INTEGER NOT NULL, last_used_at INTEGER,
               use_count INTEGER NOT NULL DEFAULT 0, content_hash TEXT NOT NULL,
               injected_mark INTEGER NOT NULL DEFAULT 0, pref_key TEXT);",
        )
        .expect("建老表");
        let err = check_shape(&conn).expect_err("老结构应被拦住");
        assert!(err.contains("memory_fts"), "应指出缺 memory_fts：{err}");
        assert!(err.contains("memory.no"), "应指出缺 no 列：{err}");
    }

    /// 空的新库也要过——形状校验不能依赖表里有数据。
    #[test]
    fn shape_check_passes_on_empty_memory_table() {
        let conn = open_in_memory().expect("open");
        let n: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "本用例要的就是空表");
        assert_eq!(check_shape(&conn), Ok(()));
    }

    #[test]
    fn expected_tables_exist() {
        let conn = open_in_memory().expect("open");
        for t in [
            "session", "entry", "memory", "memory_fts", "cron", "standing_intent", "task",
            "audit", "kv",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .expect("query");
            assert_eq!(count, 1, "table {t} must exist");
        }
    }

    #[tokio::test]
    async fn entry_roundtrip_and_reset() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        for (role, text) in [(Role::User, "你好"), (Role::Assistant, "在的")] {
            w.append_entry(NewEntry::text("main", role, text, 2)).await.unwrap();
        }

        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "你好"); // 正序
        assert_eq!(hist[1].role, Role::Assistant);

        // reset 后历史起点前移，load 应为空。
        w.reset_session("main".into()).await.unwrap();
        let after = w.load_transcript("main".into(), 100).await.unwrap();
        assert!(after.is_empty(), "reset 后上下文应为空");
    }

    /// 工具调用结构可写可读（P2-4）。这两列缺了，重放就只能把工具结果降级成
    /// user 文本，模型学会「宣布完就等人贴结果」。
    #[tokio::test]
    async fn entry_roundtrip_keeps_tool_structure() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        // 纯工具调用轮：content 为空，只有 tool_calls。
        w.append_entry(NewEntry {
            tool_calls: Some(r#"[{"id":"call_1","name":"sys","args":"{}"}]"#.into()),
            ..NewEntry::text("main", Role::Assistant, "", 1)
        })
        .await
        .unwrap();
        w.append_entry(NewEntry {
            tool_call_id: Some("call_1".into()),
            ..NewEntry::text("main", Role::Tool, "输出", 1)
        })
        .await
        .unwrap();
        // 普通消息两列都该是 NULL。
        w.append_entry(NewEntry::text("main", Role::Assistant, "干完了", 2))
            .await
            .unwrap();

        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        assert_eq!(hist.len(), 3, "纯工具调用轮（空 content）也必须落库: {hist:?}");
        assert!(hist[0].tool_calls.as_deref().unwrap().contains("call_1"));
        assert_eq!(hist[0].tool_call_id, None);
        assert_eq!(hist[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(hist[2].tool_calls, None, "普通消息不该有工具结构");
        assert_eq!(hist[2].tool_call_id, None);
    }

    #[tokio::test]
    async fn compact_with_summary_replaces_range() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        // 5 条历史。
        for (role, text) in [
            (Role::User, "第一个问题"),
            (Role::Assistant, "第一个回答"),
            (Role::User, "第二个问题"),
            (Role::Assistant, "第二个回答"),
            (Role::User, "最近的问题"),
        ] {
            w.append_entry(NewEntry::text("main", role, text, 2)).await.unwrap();
        }

        // 压缩前 4 条（seq 1..=4）成摘要，保留第 5 条。
        w.compact_with_summary("main".into(), 4, "前两轮讨论了问题一和问题二".into())
            .await
            .unwrap();

        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        // reset_at=4 → 只回 seq>4：摘要 entry(seq6) + 最近一条(seq5)。
        assert_eq!(hist.len(), 2, "应只剩摘要 + 最近一条: {hist:?}");
        assert!(hist.iter().any(|e| e.content.contains("上下文摘要")), "应有摘要 entry");
        assert!(hist.iter().any(|e| e.content.contains("最近的问题")), "最近消息应保留");
        assert!(!hist.iter().any(|e| e.content.contains("第一个问题")), "旧消息应被排除");
    }

    #[tokio::test]
    async fn compact_with_summary_does_not_rewind_reset_at() {
        use crate::types::{NewEntry, Role};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.ensure_session("main".into(), "main".into()).await.unwrap();

        for (role, text) in [
            (Role::User, "第一个问题"),
            (Role::Assistant, "第一个回答"),
            (Role::User, "第二个问题"),
            (Role::Assistant, "第二个回答"),
            (Role::User, "最近的问题"),
        ] {
            w.append_entry(NewEntry::text("main", role, text, 2)).await.unwrap();
        }

        // 用户先 /reset：reset_at 推进到最大 seq(5)。
        w.reset_session("main".into()).await.unwrap();

        // 后台压缩拿着旧快照 up_to_seq=4 后提交：不得把 reset_at 拉回 4。
        w.compact_with_summary("main".into(), 4, "旧快照的摘要".into())
            .await
            .unwrap();

        // reset_at 仍为 5 → 只回 seq>5。摘要 entry(seq6) 在 reset 之后，属正常可见；
        // 但若 up_to_seq=4 把 reset_at 拉回 4，会错误地重新暴露 seq5（"最近的问题"）。
        let hist = w.load_transcript("main".into(), 100).await.unwrap();
        assert!(
            !hist.iter().any(|e| e.content.contains("最近的问题")),
            "reset_at 不得被旧快照拉回，seq5 不得重新暴露: {hist:?}"
        );
        assert!(
            hist.iter().any(|e| e.content.contains("上下文摘要")),
            "摘要 entry 应保留（在 reset 之后）: {hist:?}"
        );
    }

    #[tokio::test]
    async fn memory_upsert_and_search() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.upsert_memory(NewMemory {
            id: "m1".into(),
            tier: Tier::Curated,
            origin: Origin::Owner,
            text: "用户喜欢简洁的回复".into(),
            keywords: Some("简洁 回复".into()),
            importance: 0.8,
            content_hash: "h1".into(),
            pref_key: None,
        })
        .await
        .unwrap();

        let hits = w
            .search_candidates(vec!["简洁".into()], None, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert_eq!(hits[0].origin, Origin::Owner);

        // 不匹配的词返回空。
        let none = w.search_candidates(vec!["登山".into()], None, 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn memory_pref_key_roundtrip_and_lookup() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        let mk = |id: &str, text: &str, key: Option<&str>| NewMemory {
            id: id.into(),
            tier: Tier::Curated,
            origin: Origin::Owner,
            text: text.into(),
            keywords: None,
            importance: 0.8,
            content_hash: format!("h-{id}"),
            pref_key: key.map(|k| k.to_string()),
        };

        w.upsert_memory(mk("p1", "我用 VS Code", Some("编辑器"))).await.unwrap();
        w.upsert_memory(mk("p2", "我用 macOS", Some("操作系统"))).await.unwrap();
        w.upsert_memory(mk("m1", "周三有例会", None)).await.unwrap();

        // 按主题查只得该主题的条目。
        let editors = w.memory_by_pref_key("编辑器".into()).await.unwrap();
        assert_eq!(editors.len(), 1, "只应查到编辑器主题");
        assert_eq!(editors[0].id, "p1");
        assert_eq!(editors[0].pref_key.as_deref(), Some("编辑器"), "pref_key 应往返");

        // 无此主题返回空。
        assert!(w.memory_by_pref_key("编程语言".into()).await.unwrap().is_empty());

        // pref_key=None 的普通记忆不会被任何主题查到。
        let all_keys = ["编辑器", "操作系统", "编程语言"];
        for k in all_keys {
            let rows = w.memory_by_pref_key(k.into()).await.unwrap();
            assert!(!rows.iter().any(|r| r.id == "m1"), "非偏好记忆不应出现在主题查询里");
        }
    }

    #[tokio::test]
    async fn delete_memory_removes_row() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.upsert_memory(NewMemory {
            id: "d1".into(),
            tier: Tier::Curated,
            origin: Origin::Owner,
            text: "待删".into(),
            keywords: None,
            importance: 0.5,
            content_hash: "hd".into(),
            pref_key: Some("编辑器".into()),
        })
        .await
        .unwrap();

        assert!(w.delete_memory("d1".into()).await.unwrap(), "应删到行");
        assert!(w.memory_by_pref_key("编辑器".into()).await.unwrap().is_empty());
        assert!(!w.delete_memory("nope".into()).await.unwrap(), "删不存在应返回 false");
    }

    #[tokio::test]
    async fn dream_promote_and_audit_chain() {
        use crate::types::{NewMemory, Origin, Tier};
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        // 一条 episodic 沉淀候选。
        w.upsert_memory(NewMemory {
            id: "e1".into(),
            tier: Tier::Episodic,
            origin: Origin::Agent,
            text: "用户常在周五复盘".into(),
            keywords: None,
            importance: 0.7,
            content_hash: "h1".into(),
            pref_key: None,
        })
        .await
        .unwrap();

        // dream_candidates 应取到 episodic。
        let cands = w.dream_candidates(10).await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].tier, Tier::Episodic);

        // 巩固 → curated；此后 curated 检索能命中。
        w.promote_memory("e1".into()).await.unwrap();
        let curated = w
            .search_candidates(vec!["周五".into()], Some(Tier::Curated), 10)
            .await
            .unwrap();
        assert_eq!(curated.len(), 1, "巩固后应进入 curated");
        // episodic 池已空。
        assert!(w.dream_candidates(10).await.unwrap().is_empty());

        // 审计链：连写两条，第二条的 hash_prev = 第一条 hash_self。
        w.write_audit("dreaming".into(), "promote".into(), Some("e1".into()))
            .await
            .unwrap();
        w.write_audit("owner".into(), "remember".into(), None)
            .await
            .unwrap();
        // 无直接读 API，此处只验证不报错即通过（链完整性属实现内不变量）。
    }

    #[tokio::test]
    async fn cron_crud_roundtrip() {
        use crate::types::NewCron;
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        w.cron_add(NewCron {
            id: "c1".into(),
            expr: "0 9 * * *".into(),
            prompt: "写周报".into(),
            tz: "UTC".into(),
            next_at: Some(12345),
        })
        .await
        .unwrap();

        let list = w.cron_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].expr, "0 9 * * *");
        assert!(list[0].enabled);
        assert_eq!(list[0].next_at, Some(12345));

        // 触发后更新。
        w.cron_mark_fired("c1".into(), 20000, Some(99999)).await.unwrap();
        let after = w.cron_list().await.unwrap();
        assert_eq!(after[0].last_fired_at, Some(20000));
        assert_eq!(after[0].next_at, Some(99999));

        // 删除。
        assert!(w.cron_rm("c1".into()).await.unwrap());
        assert!(w.cron_list().await.unwrap().is_empty());
        // 删不存在的返回 false。
        assert!(!w.cron_rm("nope".into()).await.unwrap());
    }

    #[tokio::test]
    async fn intent_crud_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();

        w.intent_add(NewStandingIntent {
            id: "i1".into(),
            text: "带转换插头".into(),
            keywords: vec!["出差".into(), "德国".into()],
            cooldown_secs: 86_400,
            budget: 3,
            expiry_at: Some(999_999),
        })
        .await
        .unwrap();

        let list = w.intent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text, "带转换插头");
        // keywords 经空格编码/解码往返不丢。
        assert_eq!(list[0].keywords, vec!["出差".to_string(), "德国".to_string()]);
        assert_eq!(list[0].cooldown_secs, 86_400);
        assert_eq!(list[0].budget, 3);
        assert_eq!(list[0].fired_count, 0, "新建未触发");
        assert!(list[0].last_fired_at.is_none());
        assert_eq!(list[0].expiry_at, Some(999_999));
        assert!(list[0].created_at > 0, "created_at 应为 unix 秒");

        // 触发一次：fired_count 抬升 + last_fired_at 记录。
        w.intent_mark_fired("i1".into(), 12_345).await.unwrap();
        let after = w.intent_list().await.unwrap();
        assert_eq!(after[0].fired_count, 1);
        assert_eq!(after[0].last_fired_at, Some(12_345));

        // 再触发一次累加（budget 判定在 core，store 只记账）。
        w.intent_mark_fired("i1".into(), 23_456).await.unwrap();
        assert_eq!(w.intent_list().await.unwrap()[0].fired_count, 2);

        // 删除。
        assert!(w.intent_rm("i1".into()).await.unwrap());
        assert!(w.intent_list().await.unwrap().is_empty());
        assert!(!w.intent_rm("nope".into()).await.unwrap());
    }

    #[tokio::test]
    async fn intent_empty_keywords_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        // 空 keywords（编码为空串）+ 不过期（expiry_at=None）不应炸。
        w.intent_add(NewStandingIntent {
            id: "i2".into(),
            text: "无关键词".into(),
            keywords: vec![],
            cooldown_secs: 0,
            budget: 1,
            expiry_at: None,
        })
        .await
        .unwrap();
        let list = w.intent_list().await.unwrap();
        assert!(list[0].keywords.is_empty());
        assert!(list[0].expiry_at.is_none());
    }

    /// 含空格的关键词必须整条往返，不能被劈开。
    ///
    /// 回归：keywords 曾用空格分隔落库，于是 `business trip` 取回时变成两条
    /// （"business" / "trip"），单命中 "trip" 就触发——比用户指定的宽得多。
    #[tokio::test]
    async fn intent_keyword_with_space_survives_roundtrip() {
        use crate::types::NewStandingIntent;
        let store = Store::open_memory().expect("open");
        let w = store.writer();
        w.intent_add(NewStandingIntent {
            id: "i3".into(),
            text: "带转换插头".into(),
            keywords: vec!["business trip".into(), "德国".into()],
            cooldown_secs: 0,
            budget: 1,
            expiry_at: None,
        })
        .await
        .unwrap();
        let list = w.intent_list().await.unwrap();
        assert_eq!(
            list[0].keywords,
            vec!["business trip".to_string(), "德国".to_string()],
            "含空格的关键词应整条保留，不得被劈成两条"
        );
    }
}
