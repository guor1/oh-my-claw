//! 具体 SQL 操作（设计 §3.4）。同步 rusqlite 函数，写线程与读连接共用。
//!
//! 半衰期等浮点排名不在此处（策略在 oc-core）；这里只取候选集、做词法粗筛。

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::StoreResult;
use crate::types::{Entry, NewEntry, NewMemory, MemoryRow, Role, Tier, Origin};

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 确保 session 存在（不存在则建）。
pub fn ensure_session(conn: &Connection, id: &str, kind: &str) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO session(id, kind, created_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(id) DO NOTHING",
        params![id, kind, now_millis()],
    )?;
    Ok(())
}

/// 追加一条 entry，seq 在会话内单调递增。**返回该 entry 的 seq**（不是 rowid）。
///
/// 返回 seq 而非 rowid：唯一的消费者是刷新接续的水位标记（`RunSink::mark_persisted`），
/// 它要答的是「事件落进了哪条 entry」——rowid 是全局的，跨会话完全错位。
pub fn append_entry(conn: &Connection, e: &NewEntry) -> StoreResult<i64> {
    // 下一个 seq = 当前最大 + 1（同会话）。
    let next_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM entry WHERE session_id = ?1",
            params![e.session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);

    conn.execute(
        "INSERT INTO entry(session_id, seq, role, content, tokens_est,
                           tool_calls, tool_call_id, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            e.session_id,
            next_seq,
            e.role.as_str(),
            e.content,
            e.tokens_est,
            e.tool_calls,
            e.tool_call_id,
            now_millis()
        ],
    )?;
    Ok(next_seq)
}

/// 列出所有会话，按创建时间倒序（最近的在前）。
pub fn session_list(conn: &Connection) -> StoreResult<Vec<crate::types::SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, created_at, COALESCE(reset_at, 0)
         FROM session
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::SessionRow {
            id: r.get(0)?,
            kind: r.get(1)?,
            created_at: r.get(2)?,
            reset_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 重置会话：推进 reset_at 到当前最大 seq（上下文起点前移，transcript 保留）。
pub fn reset_session(conn: &Connection, id: &str) -> StoreResult<()> {
    let max_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM entry WHERE session_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    conn.execute(
        "UPDATE session SET reset_at = ?2 WHERE id = ?1",
        params![id, max_seq],
    )?;
    Ok(())
}

/// 加载会话历史（reset 之后的 entry，按 seq 升序），带最大条数限制。
///
/// token 预算裁剪由上层做（这里只按条数上限拉取，避免一次拉爆）。
pub fn load_transcript(conn: &Connection, session_id: &str, max_entries: i64) -> StoreResult<Vec<Entry>> {
    let reset_at: i64 = conn
        .query_row(
            "SELECT COALESCE(reset_at, 0) FROM session WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);

    // 取最近 max_entries 条（seq > reset_at），再正序返回。
    let mut stmt = conn.prepare(
        "SELECT id, session_id, seq, role, content, tokens_est,
                tool_calls, tool_call_id, created_at
         FROM entry
         WHERE session_id = ?1 AND seq > ?2
         ORDER BY seq DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, reset_at, max_entries], |r| {
        Ok(Entry {
            id: r.get(0)?,
            session_id: r.get(1)?,
            seq: r.get(2)?,
            role: Role::from_db_str(&r.get::<_, String>(3)?),
            content: r.get(4)?,
            tokens_est: r.get(5)?,
            tool_calls: r.get(6)?,
            tool_call_id: r.get(7)?,
            created_at: r.get(8)?,
        })
    })?;
    let mut out: Vec<Entry> = rows.collect::<Result<_, _>>()?;
    out.reverse(); // 变回正序
    Ok(out)
}

/// 摘要式压缩：把 `seq <= up_to_seq` 的对话压成一条摘要 entry，并把 reset_at
/// 单调推进到 up_to_seq（排除被摘要的原始区间，但保留 transcript 供审计）。
///
/// 事务内完成：插入 System 角色的摘要 entry（seq 在 max+1，落在 reset 之后故会被
/// 后续 load 取到）+ 更新 reset_at。摘要文本由 server 调模型生成后传入。
///
/// reset_at 只前进、不后退：后台压缩的 up_to_seq 可能来自旧快照，若并发的
/// /reset 已把 reset_at 推到更大值，这里用 `MAX(COALESCE(reset_at,0), ?2)` 保证
/// 提交后不会把 reset_at 拉回旧值、重新暴露用户刚清除的历史。
pub fn compact_with_summary(
    conn: &Connection,
    session_id: &str,
    up_to_seq: i64,
    summary_text: &str,
) -> StoreResult<()> {
    let tx = conn.unchecked_transaction()?;
    let content = format!("【上下文摘要】\n{summary_text}");
    let tokens_est = (content.chars().count() as i64 / 4).max(1);
    // 摘要 entry 追加在末尾（新 seq），落在 reset_at 之后。
    let next_seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM entry WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    tx.execute(
        "INSERT INTO entry(session_id, seq, role, content, tokens_est, created_at)
         VALUES(?1, ?2, 'system', ?3, ?4, ?5)",
        params![session_id, next_seq, content, tokens_est, now_millis()],
    )?;
    // 单调推进 reset_at：只前进、不后退。并发 /reset 可能已把 reset_at 推到更大的
    // seq，这里用 MAX(COALESCE(reset_at,0), ?2) 保证旧快照的 up_to_seq 不会把它拉回。
    tx.execute(
        "UPDATE session SET reset_at = MAX(COALESCE(reset_at, 0), ?2) WHERE id = ?1",
        params![session_id, up_to_seq],
    )?;
    tx.commit()?;
    Ok(())
}

/// upsert 一条记忆（按 id），并同步 `memory_fts` 索引。
///
/// **事务内一并做**：索引与表必须同进同退。半途失败若只写了表，那条记忆就检索不到
/// （candidates 走索引），而它明明在库里——比整条写失败更难排查。
///
/// **索引侧先删后插**：contentless FTS 表的 `INSERT` 不认 rowid 冲突，同一 rowid
/// 插两次会**累积**两份 token（旧文本的窗口仍在，改过的记忆按旧内容也能被搜到）。
/// 故 upsert 的更新分支必须先 `DELETE` 再插。
pub fn upsert_memory(conn: &Connection, m: &NewMemory) -> StoreResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO memory(id, tier, origin, text, keywords, importance, created_at, content_hash, pref_key, source)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
           text=excluded.text, keywords=excluded.keywords,
           importance=excluded.importance, content_hash=excluded.content_hash,
           pref_key=excluded.pref_key, source=excluded.source",
        params![
            m.id,
            m.tier.as_str(),
            m.origin.as_str(),
            m.text,
            m.keywords,
            m.importance,
            now_millis(),
            m.content_hash,
            m.pref_key,
            m.source
        ],
    )?;
    // 拿这条记忆的 rowid：`last_insert_rowid` 在 ON CONFLICT 走更新分支时不更新，
    // 故按 id 回查（id 有 UNIQUE 索引）。
    let no: i64 = tx.query_row("SELECT no FROM memory WHERE id = ?1", params![m.id], |r| r.get(0))?;
    index_memory_text(&tx, no, &m.text)?;
    tx.commit()?;
    Ok(())
}

/// 重建单条记忆的 FTS 索引项（先删后插，见 [`upsert_memory`]）。
fn index_memory_text(conn: &Connection, no: i64, text: &str) -> StoreResult<()> {
    conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![no])?;
    conn.execute(
        "INSERT INTO memory_fts(rowid, terms) VALUES(?1, ?2)",
        params![no, crate::fts::encode_doc(text)],
    )?;
    Ok(())
}

/// 更新记忆的使用时间与计数（召回后调用，用于半衰期）。
pub fn touch_memory(conn: &Connection, id: &str, at: i64) -> StoreResult<()> {
    conn.execute(
        "UPDATE memory SET last_used_at = ?2, use_count = use_count + 1 WHERE id = ?1",
        params![id, at],
    )?;
    Ok(())
}

/// 新增一条 cron 定时任务。
pub fn cron_add(conn: &Connection, c: &crate::types::NewCron) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO cron(id, expr, prompt, tz, next_at, enabled) VALUES(?1, ?2, ?3, ?4, ?5, 1)",
        params![c.id, c.expr, c.prompt, c.tz, c.next_at],
    )?;
    Ok(())
}

/// 列出所有 cron 任务。
pub fn cron_list(conn: &Connection) -> StoreResult<Vec<crate::types::CronRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, expr, prompt, tz, next_at, last_fired_at, enabled FROM cron ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::CronRow {
            id: r.get(0)?,
            expr: r.get(1)?,
            prompt: r.get(2)?,
            tz: r.get(3)?,
            next_at: r.get(4)?,
            last_fired_at: r.get(5)?,
            enabled: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条 cron 任务。返回是否删到行。
pub fn cron_rm(conn: &Connection, id: &str) -> StoreResult<bool> {
    let n = conn.execute("DELETE FROM cron WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// 触发后更新：记录 last_fired_at + 抬 fired_count（借 standing_intent 语义？
/// cron 无 fired_count 列，仅更 next_at / last_fired_at）。
pub fn cron_mark_fired(conn: &Connection, id: &str, fired_at: i64, next_at: Option<i64>) -> StoreResult<()> {
    conn.execute(
        "UPDATE cron SET last_fired_at = ?2, next_at = ?3 WHERE id = ?1",
        params![id, fired_at, next_at],
    )?;
    Ok(())
}


/// 当前 unix 秒。standing intent 的时间语义（created/last_fired/expiry）统一用秒，
/// 与 `oc_core::proactive::allow_fire`（秒级）对齐；不用 `now_millis`（那是 entry 语义）。
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// keywords 落库编码：**换行分隔**。空列表 → 空串。
///
/// 不用空格分隔：关键词本身可能含空格（`oc intent add "带插头" "business trip"` 里
/// `business trip` 是**一个**关键词）。空格分隔会在取回时把它劈成两个，于是
/// `intent_prefilter` 单命中 "trip" 就触发——比用户要求的宽得多。换行不会出现在
/// 关键词里（编码时把内部空白归一为单空格兜底），故可安全作分隔符。
fn encode_keywords(kws: &[String]) -> String {
    kws.iter()
        // 内部空白归一为单空格：顺手清掉可能混入的换行，保证分隔符唯一。
        .map(|k| k.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|k| !k.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// keywords 取回解码：按换行切分，去空。含空格的关键词整条保留。
fn decode_keywords(s: Option<String>) -> Vec<String> {
    s.map(|s| {
        s.split('\n')
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// 新增一条 standing intent。created_at = now（秒）。
pub fn intent_add(conn: &Connection, i: &crate::types::NewStandingIntent) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO standing_intent(id, text, keywords, cooldown_secs, budget, fired_count, expiry_at, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        params![
            i.id,
            i.text,
            encode_keywords(&i.keywords),
            i.cooldown_secs,
            i.budget,
            i.expiry_at,
            now_secs(),
        ],
    )?;
    Ok(())
}

/// 列出所有 standing intent，按 created_at 升序（早建的在前）。
pub fn intent_list(conn: &Connection) -> StoreResult<Vec<crate::types::StandingIntentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, text, keywords, cooldown_secs, budget, fired_count, last_fired_at, expiry_at, created_at
         FROM standing_intent ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::types::StandingIntentRow {
            id: r.get(0)?,
            text: r.get(1)?,
            keywords: decode_keywords(r.get::<_, Option<String>>(2)?),
            cooldown_secs: r.get(3)?,
            budget: r.get::<_, i64>(4)? as u32,
            fired_count: r.get::<_, i64>(5)? as u32,
            last_fired_at: r.get(6)?,
            expiry_at: r.get(7)?,
            created_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条 standing intent。返回是否删到行。
pub fn intent_rm(conn: &Connection, id: &str) -> StoreResult<bool> {
    let n = conn.execute("DELETE FROM standing_intent WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// 触发后更新：抬 fired_count + 记 last_fired_at（秒）。
pub fn intent_mark_fired(conn: &Connection, id: &str, fired_at: i64) -> StoreResult<()> {
    conn.execute(
        "UPDATE standing_intent SET fired_count = fired_count + 1, last_fired_at = ?2 WHERE id = ?1",
        params![id, fired_at],
    )?;
    Ok(())
}

/// 取 dreaming 待巩固候选：episodic tier 的记忆（双门判定在 oc-core）。
///
/// 返回字段含 use_count / created_at / last_used_at，供 core 算频次/时间窗门。
/// 按 use_count 降序取前 `limit` 条（先看反复用到的）。
pub fn dream_candidates(conn: &Connection, limit: i64) -> StoreResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash, pref_key, source
         FROM memory WHERE tier = 'episodic'
         ORDER BY use_count DESC, created_at ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        Ok(MemoryRow {
            id: r.get(0)?,
            tier: Tier::from_db_str(&r.get::<_, String>(1)?),
            origin: Origin::from_db_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
            pref_key: r.get(9)?,
            source: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 巩固一条记忆：episodic → curated（dreaming 双门通过后调用）。
///
/// 就地提升 tier 并抬升 importance（下限 0.6），使其进入 curated 自动注入池。
pub fn promote_memory(conn: &Connection, id: &str) -> StoreResult<()> {
    conn.execute(
        "UPDATE memory SET tier = 'curated', importance = MAX(importance, 0.6)
         WHERE id = ?1 AND tier = 'episodic'",
        params![id],
    )?;
    Ok(())
}

/// 取全部 curated 记忆（id + text），供巩固模型轮的「判断动作落点」用（FEAT-4）。
pub fn curated_list(conn: &Connection) -> StoreResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, text FROM memory WHERE tier = 'curated' ORDER BY no")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 把一条新记忆合并进一条既有 curated（FEAT-4 的 corroborate/refine/correct 动作）。
///
/// 事务内：用合并后的正文更新目标行（text/importance/content_hash + 重建 FTS），
/// 再删掉源行（源是 episodic 候选，其内容已并入目标，不再单列）。源 = 目标时
/// 退化为就地更新（不删自己）。
///
/// **顺序刻意先更后删**：中途失败最坏是源行暂存（下一轮重新判定），不会丢目标内容。
pub fn merge_memory(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    merged_text: &str,
    merged_content_hash: &str,
) -> StoreResult<()> {
    let tx = conn.unchecked_transaction()?;

    // 取目标行 rowid（FTS 索引按 rowid 关联）。
    let target_no: Option<i64> = tx
        .query_row("SELECT no FROM memory WHERE id = ?1", params![target_id], |r| r.get(0))
        .optional()?;
    let Some(target_no) = target_no else {
        return Ok(()); // 目标不存在：无落点，安全放弃（源行留待下一轮）。
    };

    // 更新目标：正文、重要度下限、内容哈希。
    tx.execute(
        "UPDATE memory SET text = ?2, importance = MAX(importance, 0.6), content_hash = ?3
         WHERE no = ?1",
        params![target_no, merged_text, merged_content_hash],
    )?;
    // 重建目标 FTS 项（先删后插，见 [`upsert_memory`]）。
    crate::ops::index_memory_text(&tx, target_no, merged_text)?;

    // 源 ≠ 目标时删源（连同 FTS）。
    if source_id != target_id {
        if let Some(src_no) = tx
            .query_row("SELECT no FROM memory WHERE id = ?1", params![source_id], |r| r.get::<_, i64>(0))
            .optional()?
        {
            tx.execute("DELETE FROM memory WHERE no = ?1", params![src_no])?;
            tx.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![src_no])?;
        }
    }

    tx.commit()?;
    Ok(())
}

/// 追加一条审计记录，维护哈希链（设计 §3.3 audit：hash_prev/hash_self）。
///
/// `hash_prev` = 上一条的 `hash_self`（无则空串）；
/// `hash_self` = 链哈希(at, actor, action, payload, hash_prev)。
/// 用非加密的 FNV-1a（tamper-evident 足够；单用户本地库不需抗碰撞）。
pub fn write_audit(
    conn: &Connection,
    actor: &str,
    action: &str,
    payload: Option<&str>,
) -> StoreResult<()> {
    let at = now_millis();
    let hash_prev: String = conn
        .query_row(
            "SELECT hash_self FROM audit ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();

    let material = format!("{at}|{actor}|{action}|{}|{hash_prev}", payload.unwrap_or(""));
    let hash_self = fnv1a_hex(&material);

    conn.execute(
        "INSERT INTO audit(at, actor, action, payload, hash_prev, hash_self)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![at, actor, action, payload, hash_prev, hash_self],
    )?;
    Ok(())
}

/// FNV-1a 64 位哈希，输出 16 位十六进制。非加密，仅用于审计链自洽校验。
fn fnv1a_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// `MemoryRow` 的取列清单，带 `m.` 前缀（各查询统一 `FROM memory m`）。
///
/// 与 [`map_memory_row`] **成对**使用：曾经出过一次「给 mapper 加了 `pref_key`
/// 取值，却漏改动态拼接的 SELECT」，于是下标错位。清单与 mapper 各只有一份，
/// 那类漂移就无从发生。
const MEMORY_COLS: &str = "m.id, m.tier, m.origin, m.text, m.importance, m.created_at,
     m.last_used_at, m.use_count, m.content_hash, m.pref_key, m.source";

/// [`MEMORY_COLS`] 的行映射。下标顺序必须与清单一致。
fn map_memory_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: r.get(0)?,
        tier: Tier::from_db_str(&r.get::<_, String>(1)?),
        origin: Origin::from_db_str(&r.get::<_, String>(2)?),
        text: r.get(3)?,
        importance: r.get(4)?,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
        use_count: r.get(7)?,
        content_hash: r.get(8)?,
        pref_key: r.get(9)?,
        source: r.get(10)?,
    })
}

/// 词法候选检索：取候选集（精确排名在 oc-core）。
///
/// **走 FTS5 索引**（P2-4）：查询词编码成 `memory_fts` 的 MATCH 表达式先收窄，
/// 再用原来的 `LIKE` 复核。索引是 LIKE 结果的超集（见 [`crate::fts`]），
/// 复核负责剔掉「两字窗口都在但并不相邻」的行，故**结果集与纯 LIKE 完全一致**，
/// 只是不必再全表扫。实测 10 万条时最慢查询从 560ms 降到 6ms 内。
///
/// 查询词若无法编码（如纯 emoji）则整体退回全表 LIKE——慢，但不会漏召回。
///
/// **排序用 `no DESC` 而非 `created_at DESC`**：两者都是「最近的在前」
/// （`no` 是自增 rowid，`created_at` 是插入时的 `now_millis()`，同为插入序），
/// 但按 `created_at` 排要把 MATCH 命中的全部行灌进临时 B-tree 再排，实测反而比
/// 纯 LIKE 更慢（80~140ms）；按 `no` 走 rowid 逆序读，是 1~7ms 的那条路径。
/// 本函数只产候选集、最终排名在 oc-core，取「最近 N 条」时用哪个单调量不影响语义。
pub fn search_candidates(
    conn: &Connection,
    query_terms: &[String],
    tier_filter: Option<Tier>,
    limit: i64,
) -> StoreResult<Vec<MemoryRow>> {
    let tier_clause = match tier_filter {
        // tier 来自枚举的 as_str，非用户输入，内联无注入风险。
        Some(t) => format!(" AND m.tier = '{}'", t.as_str()),
        None => String::new(),
    };

    // 无查询词：取最近的若干条（不经索引，本就无 term 可匹配）。
    if query_terms.is_empty() {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory m WHERE 1=1{tier_clause}
             ORDER BY m.no DESC LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit], map_memory_row)?;
        return Ok(rows.collect::<Result<_, _>>()?);
    }

    let like_params: Vec<String> = query_terms.iter().map(|t| format!("%{t}%")).collect();

    match crate::fts::match_expr(query_terms) {
        // 索引路径：MATCH 收窄 + LIKE 复核。
        Some(match_expr) => {
            // ?1 = MATCH 表达式，?2.. = LIKE 模式，最后一个 = limit。
            let recheck = (0..query_terms.len())
                .map(|i| format!("m.text LIKE ?{}", i + 2))
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM memory_fts f JOIN memory m ON m.no = f.rowid
                 WHERE f.memory_fts MATCH ?1{tier_clause} AND ({recheck})
                 ORDER BY f.rowid DESC LIMIT ?{}",
                query_terms.len() + 2
            );
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(query_terms.len() + 2);
            binds.push(&match_expr);
            binds.extend(like_params.iter().map(|s| s as &dyn rusqlite::ToSql));
            binds.push(&limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(binds.as_slice(), map_memory_row)?;
            Ok(rows.collect::<Result<_, _>>()?)
        }
        // 回落：有词编不进索引，只能全表 LIKE。语义与索引路径相同。
        None => {
            let pred = (0..query_terms.len())
                .map(|i| format!("m.text LIKE ?{}", i + 1))
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM memory m
                 WHERE ({pred}){tier_clause}
                 ORDER BY m.no DESC LIMIT ?{}",
                query_terms.len() + 1
            );
            let mut binds: Vec<&dyn rusqlite::ToSql> =
                like_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            binds.push(&limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(binds.as_slice(), map_memory_row)?;
            Ok(rows.collect::<Result<_, _>>()?)
        }
    }
}

/// 按偏好主题取既有偏好（P1-3，供 `supersede` 判同主题冲突）。
///
/// 只看 curated tier：偏好属于 curated（用户显式交代），episodic 的情节记忆
/// 不参与 supersede。按 created_at 升序（早建的在前，替换时优先命中最早那条）。
pub fn memory_by_pref_key(conn: &Connection, key: &str) -> StoreResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, origin, text, importance, created_at, last_used_at, use_count, content_hash, pref_key, source
         FROM memory WHERE pref_key = ?1 AND tier = 'curated'
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![key], |r| {
        Ok(MemoryRow {
            id: r.get(0)?,
            tier: Tier::from_db_str(&r.get::<_, String>(1)?),
            origin: Origin::from_db_str(&r.get::<_, String>(2)?),
            text: r.get(3)?,
            importance: r.get(4)?,
            created_at: r.get(5)?,
            last_used_at: r.get(6)?,
            use_count: r.get(7)?,
            content_hash: r.get(8)?,
            pref_key: r.get(9)?,
            source: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 删除一条记忆（P1-3：supersede 的 Replace 用来清掉被取代的旧偏好）。
///
/// 连带删掉 FTS 索引项——否则被取代的旧偏好仍会被检索命中（`memory_fts` 是
/// contentless 表，SQLite 不会因主表删行而自动清理）。事务包裹，同进同退。
///
/// 返回是否删到行。
pub fn delete_memory(conn: &Connection, id: &str) -> StoreResult<bool> {
    let tx = conn.unchecked_transaction()?;
    // 先取 rowid：删完就查不到了，而索引项要按 rowid 删。
    let no: Option<i64> = tx
        .query_row("SELECT no FROM memory WHERE id = ?1", params![id], |r| r.get(0))
        .optional()?;
    let Some(no) = no else {
        return Ok(false); // 无此行，索引也无从清理。
    };
    let n = tx.execute("DELETE FROM memory WHERE no = ?1", params![no])?;
    tx.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![no])?;
    tx.commit()?;
    Ok(n > 0)
}
