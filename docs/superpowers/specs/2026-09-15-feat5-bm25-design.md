# FEAT-5 BM25 词级相关性打分 — 设计

日期：2026-09-15
状态：已批准（用户逐节确认）
来源：BOARD.md FEAT-5 条目

## 背景与目标

现状：召回靠 FTS5 布尔命中 + LIKE 复核，排序 `rank = 词法重合度 × 半衰期 × 重要度`，
其中「词法重合度」是命中词数计数（`lexical_relevance`）。两条各命中 1 词的记忆分不出
强弱相关——命中「罕见词」与命中「常见词」同分。

目标：查询时对命中候选做 tf-idf 打分（BM25），让「罕见词命中」的强相关信号浮上来。

**已确认的四个决策**（用户逐项选定）：

1. **接入两条路径**：`rank()`（`/memory search` + RPC `memory.search`）与
   `trigger_prefilter()`（Lane1 自动注入主路径）都换 BM25。只改 CLI 的话 FEAT-5
   基本是装饰性的。
2. **不考虑老库**：项目在开发阶段，用户可直接删库重建。迁移不回填数据。
3. **归一后相乘**：BM25 除以查询内最大值归一到 0..1，再乘半衰期与重要度。
   公式结构不变、量纲不变。
4. **查询内 max 归一**：分母用「该查询下所有候选的 bm25 最大值」。排序是查询内
   的事，跨查询可比性不需要；实现零额外查询。

## 硬边界（不改变的东西）

1. **不引入分词器**——直接在现有 2-gram token 空间上算（`fts.rs::windows`），
   character-bigram BM25 是 CJK 标准做法，零新依赖、零 OOV 维护。
2. **结果集不变**——MATCH 收窄 + LIKE 复核的候选集逻辑原样保留。BM25 只影响
   候选集内部的**排序**，不影响谁进候选集。
3. **半衰期 × 重要度公式结构保留**——BM25 替换的是 `lexical_relevance()`
   （命中计数）这一项，该项退为回落路径。
4. **回落路径不变**——查询词编不进索引（如纯 emoji）时回落全表 LIKE，此时无
   bm25 可用，`MemCandidate.bm25 = None`，排序退化为现有行为（降级而非错误）。

## 数据层（oc-store）

### Schema：外部内容表

`memory_fts` 从 contentless + `detail=none` 改为外部内容表（BOARD 已实测验证：
contentless 配 `detail=col` 建表合法但 `bm25()` 恒返回 0——contentless 强制
columnsize=0，算不出 tf；正确形态是外部内容表）：

```sql
CREATE VIRTUAL TABLE memory_fts USING fts5(
  terms,
  tokenize='unicode61',
  detail=col,              -- 存词频（docsize shadow 表），bm25() 可用
  content='memory',        -- 原文留在 memory 表，不存副本
  content_rowid='no'       -- memory.no 是显式 rowid，正好当关联键
);
```

原文仍留在 `memory.text`（不存副本、不碰 200~300% 线），词频交给 FTS5 docsize
shadow 表。索引体积从 28% 略涨（存词频，非存原文）。

**删除语义变化**：外部内容表用 FTS5 特殊命令而非普通 DELETE：

```sql
INSERT INTO memory_fts(memory_fts, rowid, terms) VALUES('delete', ?1, ?2);  -- 删旧
INSERT INTO memory_fts(rowid, terms) VALUES(?1, ?2);                         -- 插新
```

`'delete'` 命令要求传入**该行旧版本的完整 token 串**（与当初插入时逐字节一致），
才能从索引和 docsize 里正确扣除。`index_memory_text()` 从「先 DELETE 再插」改为
三步：**查旧 token → delete 命令 → 插入新 token**。旧 token 从 `memory.text`
现存值重新编码即可（索引与表同事务同步，二者一致——这正是现有不变量）。

环境前提：bundled SQLite 3.46.0（rusqlite 0.32 + libsqlite3-sys 0.30.1 实测），
外部内容表 + `detail=col` 的 `bm25()` 在此版本可用。

### 迁移：V4，不回填

迁移步进 V4：drop 旧 `memory_fts`（若存在）→ 建新形态表。不回填数据（决策 2）。
新库 `user_version` 从 0 逐级升到 4，V1 建的还是旧形态，V4 重建——两条路径
（全新库 / 删过的旧库）最终形态一致。

`check_shape` 不改：它查 `memory_fts` 存在性 + `memory.no` 列，新形态下两者都在。

### `search_candidates` 返回 bm25

索引路径 SELECT 追加 `bm25(memory_fts) AS b`（取负归正——SQLite 惯例越相关越负）：

```sql
SELECT {MEMORY_COLS}, -bm25(memory_fts) AS b
FROM memory_fts f JOIN memory m ON m.no = f.rowid
WHERE f.memory_fts MATCH ?1 {tier_clause} AND ({recheck})
ORDER BY f.rowid DESC LIMIT ?
```

`MemoryRow` 加 `bm25: Option<f64>`：索引路径为 `Some`；回落路径（LIKE）与无查询词
路径为 `None`。`MEMORY_COLS` 与 `map_memory_row` 成对更新（下标错位是出过的 bug，
清单与 mapper 各一份的约定继续守——bm25 列单独接在清单之后，不插中间）。

BM25 的 IDF 语义在 2-gram 词流上依然成立：「简洁」的 token 在语料里越罕见，
命中它得分越高。

## 策略层（oc-core）

### `MemCandidate` 加字段

```rust
pub bm25: Option<f64>,   // None = 无 bm25 可用（回落路径），排序退化为现有行为
```

### `rank()`：归一后相乘

```rust
let max_bm25 = cands.iter().filter_map(|c| c.bm25).fold(0.0f64, f64::max);
let rel = match c.bm25 {
    Some(b) if max_bm25 > 0.0 => (b / max_bm25).clamp(0.0, 1.0),
    _ => lexical_relevance(&c.text, query_terms),  // 回落：保持旧行为
};
score = rel * halflife_factor * importance;
```

`lexical_relevance` 从主路径退为回落路径，函数保留。

### `trigger_prefilter()`：排序键换 BM25

过滤条件不变（curated only + 命中 ≥1 词），排序从「命中词数降序」换成
「bm25 降序」（无 bm25 的行按命中数排在其后同段内，`.then()` 链）。

## 接线层（oc-server）

`dispatch.rs` 两处（`handle_memory_search`、`memory_text`）+ `session.rs` 一处
（`lane1_bootstrap`）的候选映射各加 `bm25: r.bm25`。无查询词路径不碰。

## 测试策略

- **oc-core `rank()`**：构造「两条各命中 1 词、但其中一条命中的是罕见词」的候选集，
  断言罕见词命中者排前——FEAT-5 的核心验收。回落路径（`bm25=None`）行为与旧版
  一致（现有单测语义保留，构造器补 `bm25: None`）。
- **oc-core `trigger_prefilter`**：同形状断言（过滤条件与 cap 不变）。
- **oc-store 迁移**：V4 后新库 `bm25()` 不恒为 0（造两条不同相关度的行，断言
  bm25 有区分）；`bm25` 字段在索引路径往返、回落路径为 None。
- **oc-store 回归**：`memory_fts.rs` 现有全部测试保持绿（结果集不变量的守门人）。
  特别是 `repeated_upsert_leaves_only_current_tokens_in_index` 与
  `delete_removes_tokens_from_index`——删除语义改为 `'delete'` 命令后的关键回归点。
- **性能验收**：检索仍 ms 级（沿用 `search_stays_fast_at_100k_rows` 的比值判据）；
  索引体积从 28% 略涨（存词频），不触碰 200~300% 原文副本线。

## 改动面一览

| 层 | 文件 | 改动 |
|---|---|---|
| oc-store | `schema.rs` | V4：重建 `memory_fts` 为外部内容表 |
| oc-store | `migrate.rs` | 加 step 4；TARGET_VERSION = 4 |
| oc-store | `ops.rs` | `index_memory_text` 改三步删除语义；`search_candidates` SELECT 加 `bm25()`；mapper |
| oc-store | `types.rs` | `MemoryRow.bm25` |
| oc-core | `memory.rs` | `MemCandidate.bm25`、`rank()` 归一、`trigger_prefilter()` 排序键 |
| oc-server | `dispatch.rs`、`session.rs` | 三处映射各加一行 |

验收（BOARD 原文对齐）：检索仍 ms 级；罕见词命中排序浮上来；索引体积略涨不碰
副本线；全量 `cargo test --workspace` 绿。
