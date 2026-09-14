//! `memory.text` FTS5 索引的回归（P2-4）。
//!
//! 关注点不是"快"，而是**换索引没换语义**：候选检索走 `memory_fts` 收窄后，
//! 结果集必须与原来的全表 `LIKE` 一致，且索引要随记忆的增删改保持同步。
//!
//! 用**内存库**：这些用例只验证语义，读回落写线程即可（见 `Store` 文档）。
//! 索引与表的同步发生在写线程的同一个事务里，与走不走读池无关。

use std::path::PathBuf;

use oc_store::{NewMemory, Origin, Store, Tier};

fn temp_db(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("oc-fts-{}-{tag}.sqlite", std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &PathBuf) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(p.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(p.with_extension("sqlite-shm"));
}

/// 直接读 `memory_fts` 的词表（`fts5vocab`），看索引里实际留着哪些 token。
fn index_tokens(db: &PathBuf) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).expect("开库");
    conn.execute_batch("CREATE VIRTUAL TABLE temp.vocab USING fts5vocab(main, memory_fts, row);")
        .expect("建 fts5vocab");
    let mut stmt = conn.prepare("SELECT term FROM temp.vocab ORDER BY term").expect("查词表");
    let out = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("取词")
        .map(|x| x.unwrap())
        .collect();
    out
}

fn mem(id: &str, text: &str) -> NewMemory {
    NewMemory {
        id: id.into(),
        tier: Tier::Curated,
        origin: Origin::Owner,
        text: text.into(),
        keywords: None,
        importance: 0.8,
        content_hash: format!("h-{id}"),
        pref_key: None,
        source: None,
    }
}

async fn store_with(rows: &[(&str, &str)]) -> Store {
    let s = Store::open_memory().expect("开库");
    for (id, text) in rows {
        s.writer().upsert_memory(mem(id, text)).await.expect("写记忆");
    }
    s
}

async fn search(s: &Store, terms: &[&str]) -> Vec<String> {
    let terms: Vec<String> = terms.iter().map(|t| t.to_string()).collect();
    let mut ids: Vec<String> = s
        .search_candidates(terms, None, 64)
        .await
        .expect("检索")
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.sort();
    ids
}

/// 中文能被检索到——这条是本次改动最容易悄悄坏掉的地方。
///
/// 计划书原本推荐 `tokenize='porter unicode61'`，而 `unicode61` 把一整段中文当
/// **一个** token：索引里是 `用户喜欢简洁的回复`，查 `简洁` 会得零行。若哪天有人
/// 把 fts 模块的编码换成"直接把原文交给 unicode61"，这条会当场变红。
#[tokio::test]
async fn chinese_substring_is_searchable() {
    let s = store_with(&[
        ("m1", "用户喜欢简洁的回复"),
        ("m2", "周三下午有例会"),
    ])
    .await;

    assert_eq!(search(&s, &["简洁"]).await, vec!["m1"], "中文子串必须能命中");
    assert_eq!(search(&s, &["例会"]).await, vec!["m2"]);
    assert!(search(&s, &["登山"]).await.is_empty(), "不相关的词不该命中");
}

/// 索引只是**收窄**候选，不是判定结果：AND 到的两字窗口都在、但并不相邻的行
/// 必须被 SQL 里保留的 `LIKE` 复核剔掉。
///
/// `detail=none` 不存词位，索引无法判相邻性——若哪天有人"优化"掉那句 LIKE 复核，
/// 这条会变红。
#[tokio::test]
async fn index_false_positives_are_rejected_by_recheck() {
    // "洁简" 含窗口 洁简；查 "简洁" 的窗口是 简洁 —— 不同 token，本就不会命中。
    // 造一个真会撞的：查询词 abcd 的窗口是 ab/bc/cd，
    // 而 "cdxbcxab" 三个窗口全有却不含子串 abcd。
    let s = store_with(&[("hit", "abcd"), ("fp", "cdxbcxab")]).await;
    assert_eq!(
        search(&s, &["abcd"]).await,
        vec!["hit"],
        "窗口都在但不相邻的行必须被 LIKE 复核剔掉"
    );
}

/// 改写一条记忆后，**旧内容不该还能搜到**。
///
/// contentless FTS 表的 INSERT 不认 rowid 冲突：同 rowid 插两次会累积两份 token。
/// upsert 必须先删索引项再插。
///
/// 注意这条**单独还抓不住**漏删：LIKE 复核会把「索引里有旧 token 但正文已改」的行
/// 剔掉，结果集照样对。漏删的真实代价由
/// [`stale_tokens_do_not_survive_delete_after_repeated_upsert`] 钉住——
/// 累积的 token 连 `DELETE` 都清不净，会串到复用该 rowid 的新记忆上。
#[tokio::test]
async fn upsert_replaces_old_index_entry() {
    let s = store_with(&[("m1", "我用 VS Code")]).await;
    assert_eq!(search(&s, &["VS Code"]).await, vec!["m1"]);

    // 同 id 改文本。
    s.writer().upsert_memory(mem("m1", "我改用 Neovim 了")).await.unwrap();

    assert_eq!(search(&s, &["Neovim"]).await, vec!["m1"], "新内容应可检索");
    assert!(
        search(&s, &["VS Code"]).await.is_empty(),
        "旧内容不该再被搜到——索引项没被替换"
    );
}

/// 反复改写同一条记忆后，索引里**只应剩当前版本的 token**。
///
/// 这条是「先删后插」的守门人，直接查索引词表而不绕经检索结果。为什么不绕：
/// contentless 表的 `DELETE` 只清得掉**最后一次**插入的那批 token，早先累积的
/// 会永久留在索引里；而 `search_candidates` 的 LIKE 复核会把「索引有旧词、
/// 正文已改」的行剔掉，于是结果集看起来正常——症状被复核掩盖，只在
/// rowid 被新记忆复用时才偶发暴露成「搜到不相干的记忆」。查词表能稳定地
/// 抓住根因。
///
/// `fts5vocab` 是 FTS5 自带的词表虚表，列出索引里实际存在的 token。用**文件库**
/// 才能另开一条连接去查它（内存库每连接一个独立库）。
#[tokio::test]
async fn repeated_upsert_leaves_only_current_tokens_in_index() {
    let db = temp_db("vocab");
    let s = Store::open_path(db.clone()).unwrap();

    // 同一条记忆改写 6 版，每版带一个独有的拉丁特征词（编码后是独立 token，好辨认）。
    for i in 0..6 {
        s.writer()
            .upsert_memory(mem("m1", &format!("version genx{i}")))
            .await
            .unwrap();
    }
    drop(s); // 让写线程退出、WAL 落盘

    let tokens = index_tokens(&db);

    // 末版的特征词该在；前 5 版的都不该留。
    // genx5 编码出的窗口含 "x5"；旧版本的是 "x0".."x4"。
    assert!(
        tokens.iter().any(|t| t == "x5"),
        "当前版本的 token 应在索引里，实际词表 = {tokens:?}"
    );
    for i in 0..5 {
        assert!(
            !tokens.iter().any(|t| t == &format!("x{i}")),
            "第{i}版的 token 仍留在索引里——upsert 漏了先删索引项（词表 = {tokens:?}）"
        );
    }

    cleanup(&db);
}

/// 删掉的记忆不该还能被搜到，其他记忆不受影响。
#[tokio::test]
async fn delete_clears_index_entry() {
    let s = store_with(&[("m1", "我对花生过敏"), ("m2", "周五复盘")]).await;
    assert!(s.writer().delete_memory("m1".into()).await.unwrap());

    assert!(
        search(&s, &["花生"]).await.is_empty(),
        "删掉的记忆不该还能被搜到"
    );
    assert_eq!(search(&s, &["复盘"]).await, vec!["m2"], "其他记忆不受影响");
    // 删不存在的 id 仍返回 false（且不炸）。
    assert!(!s.writer().delete_memory("nope".into()).await.unwrap());
}

/// 删除必须连带清掉索引项，否则索引**只增不减**。
///
/// `memory_fts` 是 contentless 表，SQLite 不会因主表删行而自动清理它。
/// 光看检索结果发现不了这个漏洞：候选查询 `JOIN memory`，孤立的索引行没有对应
/// 主表行、自然被 JOIN 丢掉，结果集看着完全正常。代价是长期运行下索引里堆满
/// 已删记忆的 token（本项目要 7×24 挂着跑，这类只增不减的结构正是 P2 在收的）。
/// 故直接查词表。
#[tokio::test]
async fn delete_removes_tokens_from_index() {
    let db = temp_db("del-vocab");
    let s = Store::open_path(db.clone()).unwrap();
    s.writer().upsert_memory(mem("m1", "peanut allergy")).await.unwrap();
    s.writer().upsert_memory(mem("m2", "friday review")).await.unwrap();
    s.writer().delete_memory("m1".into()).await.unwrap();
    drop(s);

    let tokens = index_tokens(&db);
    // "peanut" 的窗口含 "pe"/"ea"/"nu"/"ut"；删干净后这些都不该留。
    // 用 "nu" 判定：它只出现在 peanut 里，不在 friday review 里。
    assert!(
        !tokens.contains(&"nu".to_string()),
        "已删记忆的 token 仍留在索引里（索引只增不减）：词表 = {tokens:?}"
    );
    // 未删的那条仍应完整在索引里（别把整张索引清了）。
    assert!(
        tokens.contains(&"fr".to_string()),
        "未删记忆的 token 不该被连带清掉：词表 = {tokens:?}"
    );

    cleanup(&db);
}

/// 删除后 rowid 可能被后续插入复用（SQLite 的 rowid 分配）——复用时索引不得串味。
#[tokio::test]
async fn rowid_reuse_does_not_leak_old_tokens() {
    let s = store_with(&[("m1", "第一条关于花生的记忆")]).await;
    s.writer().delete_memory("m1".into()).await.unwrap();
    // 新记忆可能拿到刚释放的 rowid。
    s.writer().upsert_memory(mem("m2", "第二条关于登山的记忆")).await.unwrap();

    assert_eq!(search(&s, &["登山"]).await, vec!["m2"]);
    assert!(search(&s, &["花生"]).await.is_empty(), "旧 token 不该借新行复活");
}

/// tier 提升只改 tier/importance，不动 text，故无需重建索引——但检索仍要正常。
///
/// 钉住这一点：若哪天 `promote_memory` 开始改写 text 而忘了同步索引，这条会红。
#[tokio::test]
async fn promote_keeps_index_intact() {
    let s = Store::open_memory().unwrap();
    s.writer()
        .upsert_memory(NewMemory {
            tier: Tier::Episodic,
            ..mem("e1", "用户常在周五复盘")
        })
        .await
        .unwrap();

    s.writer().promote_memory("e1".into()).await.unwrap();

    let hits = s
        .search_candidates(vec!["复盘".into()], Some(Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "巩固后应能在 curated 里搜到");
    assert_eq!(hits[0].id, "e1");
}

/// 无法编码的查询词（纯 emoji 之类）要整体退回全表 LIKE，**不能漏召回**。
///
/// 关键在混合情形：一个可编码词 + 一个不可编码词。若实现只把不可编码的词丢掉，
/// OR 语义下「只含该词」的行就会被静默漏掉。
#[tokio::test]
async fn unencodable_terms_fall_back_without_losing_rows() {
    let s = store_with(&[("m1", "简洁一点"), ("m2", "开心 😀😀 的一天")]).await;

    // 纯不可编码词：回落路径照样要能搜到。
    assert_eq!(search(&s, &["😀😀"]).await, vec!["m2"]);
    // 混合：两条都该出现（m1 靠"简洁"，m2 靠 emoji）。
    assert_eq!(
        search(&s, &["简洁", "😀😀"]).await,
        vec!["m1", "m2"],
        "回落必须保留全部查询词的召回"
    );
}

/// 无查询词时取最近的若干条（该分支不经索引）。
#[tokio::test]
async fn empty_query_returns_recent_rows() {
    let s = store_with(&[("m1", "第一条"), ("m2", "第二条"), ("m3", "第三条")]).await;
    let rows = s.search_candidates(vec![], None, 2).await.unwrap();
    assert_eq!(rows.len(), 2, "应受 limit 限制");
    // 最近插入的在前。
    assert_eq!(rows[0].id, "m3");
    assert_eq!(rows[1].id, "m2");
}

/// tier 过滤与索引路径叠加时仍生效。
#[tokio::test]
async fn tier_filter_applies_on_index_path() {
    let s = Store::open_memory().unwrap();
    s.writer().upsert_memory(mem("c1", "关于复盘的偏好")).await.unwrap();
    s.writer()
        .upsert_memory(NewMemory { tier: Tier::Episodic, ..mem("e1", "关于复盘的情节") })
        .await
        .unwrap();

    let curated = s
        .search_candidates(vec!["复盘".into()], Some(Tier::Curated), 10)
        .await
        .unwrap();
    assert_eq!(curated.len(), 1, "只应回 curated");
    assert_eq!(curated[0].id, "c1");
}

/// P2-4 的性能验收：10 万条记忆下候选检索 <100ms。
///
/// **用比值而非只看绝对耗时**：绝对阈值依赖机器（CI 上的共享 runner 可能慢几倍），
/// 单看它容易变成随机红。故同时量「同一批查询走全表 LIKE」作对照——索引的意义是
/// **不随行数线性增长**，比值把机器速度约掉。绝对阈值仍留着（验收条款就是 100ms），
/// 但放宽到 500ms 以容忍慢机器，真正的判据交给比值。
///
/// 标 `#[ignore]`：灌 10 万条要几秒，不适合每次 `cargo test` 都跑。
/// 跑它：`cargo test -p oc-store --test memory_fts -- --ignored --nocapture`
#[tokio::test]
#[ignore = "灌 10 万行较慢，按需手动跑"]
async fn search_stays_fast_at_100k_rows() {
    use std::time::Instant;

    const N: usize = 100_000;
    let db = temp_db("bench");
    let s = Store::open_path(db.clone()).unwrap();

    // 造真实体量的记忆：每条数百字的中文情节，混入拉丁词。
    let topics = [
        "借用检查", "并发写入", "命名管道", "记忆分层", "半衰期排名", "向量检索",
    ];
    for i in 0..N {
        let t = topics[i % topics.len()];
        s.writer()
            .upsert_memory(mem(
                &format!("m{i}"),
                &format!(
                    "我在看 {t} 的时候遇到一个问题，具体表现是程序在高并发下偶发失败，\
                     日志里只有一行含糊的错误。这个现象通常来自共享状态没有被正确隔离，\
                     建议先确认写入路径是否串行化，再检查读连接是否复用了同一个事务快照。\
                     如果确认是锁竞争，可以把 busy_timeout 调高。第 {i} 号案例已记录。"
                ),
            ))
            .await
            .unwrap();
    }

    // 覆盖几类形状：命中多的、一条都不命中的、长消息切出的一大堆词。
    let queries: Vec<Vec<String>> = vec![
        vec!["借用检查".into()],
        vec!["登山滑雪装备".into()],
        vec!["锁竞争".into(), "busy_timeout".into()],
        // 长消息：模拟上层 tokenize() 产出的整词 + 2-gram 混合
        "我想知道并发写入在高并发下偶发失败是怎么回事"
            .chars()
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w.iter().collect())
            .collect(),
    ];

    let mut worst = std::time::Duration::ZERO;
    for q in &queries {
        let t0 = Instant::now();
        let rows = s.search_candidates(q.clone(), Some(Tier::Curated), 32).await.unwrap();
        let dt = t0.elapsed();
        worst = worst.max(dt);
        println!("  {} 个词 -> {} 条候选，{dt:?}", q.len(), rows.len());
    }
    println!("索引路径最慢：{worst:?}");

    // 对照：同样的查询绕开索引走全表 LIKE。
    let reader = s.reader().expect("文件库有读池").clone();
    let mut worst_scan = std::time::Duration::ZERO;
    for q in &queries {
        let q = q.clone();
        let t0 = Instant::now();
        let _ = reader
            .read(move |c| {
                let pred = (0..q.len())
                    .map(|i| format!("text LIKE ?{}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let sql = format!(
                    "SELECT id FROM memory WHERE tier='curated' AND ({pred})
                     ORDER BY created_at DESC LIMIT 32"
                );
                let pats: Vec<String> = q.iter().map(|t| format!("%{t}%")).collect();
                let refs: Vec<&dyn rusqlite::ToSql> =
                    pats.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
                let mut st = c.prepare(&sql)?;
                let n = st.query_map(refs.as_slice(), |r| r.get::<_, String>(0))?.count();
                Ok(n)
            })
            .await
            .unwrap();
        worst_scan = worst_scan.max(t0.elapsed());
    }
    println!("全表 LIKE 最慢：{worst_scan:?}");

    assert!(
        worst < std::time::Duration::from_millis(500),
        "10 万条下最慢查询 {worst:?} 超过 500ms（验收目标 100ms）"
    );
    assert!(
        worst * 5 < worst_scan,
        "索引路径 {worst:?} 相对全表扫描 {worst_scan:?} 没有明显优势——索引可能没被用上"
    );

    drop(s);
    cleanup(&db);
}

/// `MemoryRow` 的每个字段都要正确取回——列清单与 mapper 的下标错位是出过的 bug。
#[tokio::test]
async fn all_row_fields_round_trip_through_index_path() {
    let s = Store::open_memory().unwrap();
    s.writer()
        .upsert_memory(NewMemory {
            id: "p1".into(),
            tier: Tier::Curated,
            origin: Origin::Agent,
            text: "我用 Neovim".into(),
            keywords: Some("编辑器".into()),
            importance: 0.75,
            content_hash: "hash-p1".into(),
            pref_key: Some("编辑器".into()),
            source: Some("sess-p1".into()),
        })
        .await
        .unwrap();

    let rows = s.search_candidates(vec!["Neovim".into()], None, 10).await.unwrap();
    let r = rows.first().expect("应命中");
    assert_eq!(r.id, "p1");
    assert_eq!(r.tier, Tier::Curated);
    assert_eq!(r.origin, Origin::Agent, "origin 取错列会破坏抗投毒判定");
    assert_eq!(r.text, "我用 Neovim");
    assert!((r.importance - 0.75).abs() < 1e-9);
    assert!(r.created_at > 0);
    assert_eq!(r.use_count, 0);
    assert_eq!(r.content_hash, "hash-p1");
    assert_eq!(r.pref_key.as_deref(), Some("编辑器"));
    assert_eq!(r.source.as_deref(), Some("sess-p1"), "source 取回列");
}
