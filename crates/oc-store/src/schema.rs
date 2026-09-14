//! Schema DDL（设计 §3.3）。每个版本的迁移 SQL 集中在此，供 [`crate::migrate`] 逐步应用。

/// v1：MVP 全表（不含 sqlite-vec 虚表，虚表由 feature 单独创建）。
pub const V1: &str = r#"
-- ── 会话与转写 ──────────────────────────────────────────────
CREATE TABLE session (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  created_at    INTEGER NOT NULL,
  reset_at      INTEGER
);

CREATE TABLE entry (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id    TEXT NOT NULL REFERENCES session(id),
  seq           INTEGER NOT NULL,
  role          TEXT NOT NULL,
  content       TEXT NOT NULL,
  tokens_est    INTEGER NOT NULL,
  -- 工具调用结构（P2-4）。缺了这两列，历史重放只能把工具结果降级成 user 文本，
  -- 模型在自己的上下文里从没见过「我发起工具调用」的样例，于是学会宣布完就等
  -- 用户贴结果（in-context learning 压倒系统提示词）。
  tool_calls    TEXT,          -- assistant 发起的调用（JSON 数组），NULL = 无
  tool_call_id  TEXT,          -- tool 结果关联的调用 id，NULL = 非工具结果
  created_at    INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX idx_entry_session_seq ON entry(session_id, seq);

-- ── 记忆索引 ───────────────────────────────────────────────
-- `no INTEGER PRIMARY KEY` 是显式 rowid：`memory_fts` 是 contentless 表，只能靠
-- rowid 关联回本表。SQLite 只对**声明了** INTEGER PRIMARY KEY 的表保证 rowid
-- 在 VACUUM 后不变；隐式 rowid 允许被重编号，那会让 FTS 的映射整体错位
-- （指向别的记忆，而非查不到）。id 仍是业务主键，改 UNIQUE 保持等价约束。
CREATE TABLE memory (
  no            INTEGER PRIMARY KEY,
  id            TEXT NOT NULL UNIQUE,
  tier          TEXT NOT NULL,
  origin        TEXT NOT NULL,
  text          TEXT NOT NULL,
  keywords      TEXT,
  importance    REAL NOT NULL DEFAULT 0.5,
  created_at    INTEGER NOT NULL,
  last_used_at  INTEGER,
  use_count     INTEGER NOT NULL DEFAULT 0,
  content_hash  TEXT NOT NULL,
  injected_mark INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_memory_tier_origin ON memory(tier, origin);

-- `memory.text` 的全文索引（P2-4）。存的不是原文，而是
-- `crate::fts::encode_doc` 产出的「相邻两字」词流；查询侧用同一函数编码，
-- 于是索引是 `LIKE '%词%'` 结果的**超集**。理由见 fts 模块文档。
--
-- `content=''`（contentless）：原文已在 memory.text，再存一份纯属浪费——
-- 实测 10 万条时 contentless 索引占表的 28%，存内容则到 200~300%。
-- `detail=none`：不存词位，索引只答「这些两字窗口都出现过」，
-- 相邻性由 SQL 里保留的 `LIKE` 复核负责（也因此不支持多 token 短语查询）。
-- `contentless_delete=1`：contentless 表默认删不掉行，而记忆会被
-- supersede 替换和删除，没有它索引只增不减。
CREATE VIRTUAL TABLE memory_fts USING fts5(
  terms,
  tokenize='unicode61',
  detail=none,
  content='',
  contentless_delete=1
);

-- ── 主动性 ─────────────────────────────────────────────────
CREATE TABLE cron (
  id            TEXT PRIMARY KEY,
  expr          TEXT NOT NULL,
  prompt        TEXT NOT NULL,
  tz            TEXT NOT NULL,
  next_at       INTEGER,
  last_fired_at INTEGER,
  enabled       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE standing_intent (
  id            TEXT PRIMARY KEY,
  text          TEXT NOT NULL,
  keywords      TEXT,
  trigger_vec   BLOB,
  scope         TEXT,
  cooldown_secs INTEGER NOT NULL DEFAULT 86400,
  budget        INTEGER NOT NULL DEFAULT 3,
  fired_count   INTEGER NOT NULL DEFAULT 0,
  last_fired_at INTEGER,
  expiry_at     INTEGER,
  created_at    INTEGER NOT NULL
);

-- ── 后台任务台账 ───────────────────────────────────────────
CREATE TABLE task (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  state         TEXT NOT NULL,
  detail        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- ── 审批 / 审计 ────────────────────────────────────────────
CREATE TABLE audit (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at            INTEGER NOT NULL,
  actor         TEXT NOT NULL,
  action        TEXT NOT NULL,
  payload       TEXT,
  hash_prev     TEXT,
  hash_self     TEXT
);

-- ── 配置状态 ───────────────────────────────────────────────
CREATE TABLE kv (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
"#;

/// v2：偏好主题列（P1-3，设计 §4.5(e) User model supersede）。
///
/// `supersede` 要按主题查同类既有偏好（"编辑器" 下已记了什么），故加 `pref_key`。
/// NULL = 非偏好类记忆（走原有的内容哈希去重路径），既有行升级后即为 NULL。
///
/// `ALTER TABLE ADD COLUMN` 是 SQLite 最安全的 DDL：不重建表、不动既有数据。
pub const V2: &str = r#"
ALTER TABLE memory ADD COLUMN pref_key TEXT;
CREATE INDEX idx_memory_pref_key ON memory(pref_key);
"#;

/// v3：来源追溯列（FEAT-3，设计 §4.1 来源追溯链）。
///
/// `source` 记这条记忆的出处（如 flush 出它的 session_id），与 `origin`（信任
/// 分级：谁写的）正交。`origin` 答「可信不可信」，`source` 答「从哪来」。可空：
/// 用户显式「记住…」与历史既有行不强制带来源。
pub const V3: &str = r#"
ALTER TABLE memory ADD COLUMN source TEXT;
"#;

/// sqlite-vec 虚表（feature `sqlite-vec`）。维度随 embedding 模型，暂定 768。
#[cfg(feature = "sqlite-vec")]
pub const V1_VEC: &str = r#"
CREATE VIRTUAL TABLE memory_vec USING vec0(
  memory_id TEXT PRIMARY KEY,
  embedding FLOAT[768]
);
"#;
