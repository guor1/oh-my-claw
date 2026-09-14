//! 记忆系统纯策略（设计 §4.1–4.5）。
//!
//! **纯函数集合**：排名公式、trigger 预筛、provenance 分类、User model supersede、
//! standing intent 预筛。所有外部量（now、候选集、查询）作为参数传入；
//! SQL/向量执行在 oc-store，编排在 oc-server。
//!
//! M5：相关性用**词法**（关键词重合）算；向量语义检索留接口后补。

/// 记忆分层（设计 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// AGENTS/SOUL/USER 指令类，会话起始注入。
    Curated,
    /// 情节记忆，按需搜，从不自动注入。
    Episodic,
    /// 待办/意图，触发时注入。
    Prospective,
    /// 给人读的回顾（DREAMS.md）。
    Review,
}

/// 记忆来源（provenance，抗投毒，设计 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Owner,
    Agent,
    Untrusted,
    System,
}

/// 写记忆的来源上下文，用于分类 origin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSource {
    /// 用户显式"记住…"。
    UserExplicit,
    /// 主会话 agent 推断。
    MainAgent,
    /// web_fetch/web_search 等外部内容。
    ExternalContent,
    /// cron/heartbeat/subagent 会话。
    BackgroundSession,
    /// 无法判定。
    Unknown,
}

/// provenance 分类（设计 §4.2）：**绝不默认 Owner**，无法判定 → 保守。
pub fn classify_origin(src: WriteSource) -> Origin {
    match src {
        WriteSource::UserExplicit => Origin::Owner,
        WriteSource::MainAgent => Origin::Agent,
        WriteSource::ExternalContent => Origin::Untrusted,
        // 后台会话不产生 Owner 候选；无法判定保守归 System/Untrusted。
        WriteSource::BackgroundSession => Origin::System,
        WriteSource::Unknown => Origin::Untrusted,
    }
}

/// 后台会话（cron/heartbeat/subagent）是否应产生持久记忆候选（设计 §4.2）。
pub fn produces_persistent_candidate(src: WriteSource) -> bool {
    !matches!(src, WriteSource::BackgroundSession)
}

/// 一条记忆候选（从 store 取回的字段映射而来）。
#[derive(Debug, Clone)]
pub struct MemCandidate {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    pub text: String,
    pub importance: f64,
    /// 最近使用时间（unix 秒）；None 用 created_at 兜底由调用方保证。
    pub last_used_secs: i64,
    /// 来源追溯（FEAT-3）；`None` = 无出处。
    pub source: Option<String>,
}

/// 排名结果。
#[derive(Debug, Clone)]
pub struct Ranked {
    pub id: String,
    pub score: f64,
}

/// 排名配置。
#[derive(Debug, Clone)]
pub struct RankCfg {
    /// 半衰期（秒）。默认 30 天。
    pub halflife_secs: f64,
}

impl Default for RankCfg {
    fn default() -> Self {
        Self { halflife_secs: 30.0 * 86400.0 }
    }
}

/// 词法相关性：查询词与文本的重合比例（0..1）。
///
/// 简单稳健：命中的查询词数 / 查询词总数。大小写不敏感。
pub fn lexical_relevance(text: &str, query_terms: &[String]) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    let hits = query_terms
        .iter()
        .filter(|t| !t.is_empty() && lower.contains(&t.to_lowercase()))
        .count();
    hits as f64 / query_terms.len() as f64
}

/// 30 天半衰期因子：2^(-Δ/halflife)，Δ 为距今秒数（设计 §4.3）。
pub fn halflife_factor(now_secs: i64, last_used_secs: i64, halflife_secs: f64) -> f64 {
    let delta = (now_secs - last_used_secs).max(0) as f64;
    2f64.powf(-delta / halflife_secs)
}

/// Lane1 排名公式（设计 §4.3）：相关性 × 半衰期 × importance。
///
/// 纯函数：候选集 + 查询词 + now → 按 score 降序排列的结果。
pub fn rank(
    cands: &[MemCandidate],
    query_terms: &[String],
    now_secs: i64,
    cfg: &RankCfg,
) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = cands
        .iter()
        .map(|c| {
            let rel = lexical_relevance(&c.text, query_terms);
            let hl = halflife_factor(now_secs, c.last_used_secs, cfg.halflife_secs);
            Ranked {
                id: c.id.clone(),
                score: rel * hl * c.importance,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// trigger 注入预筛（设计 §4.3）。
///
/// 词法相关性 ≥ 阈值、**仅 curated tier**、每轮最多 `max` 条，
/// 且排除已注入过的（防召回环）。返回命中的记忆 id（按相关性降序）。
pub fn trigger_prefilter(
    msg: &str,
    cands: &[MemCandidate],
    query_terms: &[String],
    threshold: f64,
    max: usize,
) -> Vec<String> {
    let _ = (msg, threshold); // 词法模式下用命中判定；msg/threshold 留待向量预筛
    // 词法命中数：记忆文本包含多少个 query term。
    let hit_count = |text: &str| -> usize {
        let lower = text.to_lowercase();
        query_terms
            .iter()
            .filter(|t| !t.is_empty() && lower.contains(&t.to_lowercase()))
            .count()
    };
    let mut scored: Vec<(usize, &MemCandidate)> = cands
        .iter()
        .filter(|c| c.tier == Tier::Curated)
        .map(|c| (hit_count(&c.text), c))
        .filter(|(hits, _)| *hits >= 1) // 至少命中一个有意义的词
        .collect();
    // 命中数多的优先，其次 importance。
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.importance.partial_cmp(&a.1.importance).unwrap_or(std::cmp::Ordering::Equal))
    });
    scored.into_iter().take(max).map(|(_, c)| c.id.clone()).collect()
}

/// 自动注入的 tier 白名单（设计 §4.3：自动注入仅限 curated）。
pub fn is_auto_injectable(tier: Tier) -> bool {
    matches!(tier, Tier::Curated)
}

/// User model supersede（设计 §4.5）：新偏好就地替换矛盾项，不 append。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupersedePlan {
    /// 替换已有偏好（同 key）。
    Replace { existing_id: String },
    /// 全新偏好，新增。
    Add,
    /// 与已有完全相同，忽略。
    Ignore,
}

/// 一条用户偏好。
#[derive(Debug, Clone)]
pub struct Pref {
    pub id: String,
    /// 归一化的主题 key（如 "回复风格"）。
    pub key: String,
    pub value: String,
}

/// 判定新偏好该 replace / add / ignore（设计 §4.5）。
pub fn supersede(existing: &[Pref], incoming: &Pref) -> SupersedePlan {
    for e in existing {
        if e.key == incoming.key {
            return if e.value == incoming.value {
                SupersedePlan::Ignore
            } else {
                SupersedePlan::Replace { existing_id: e.id.clone() }
            };
        }
    }
    SupersedePlan::Add
}

/// 偏好主题词表：`(归一化 key, 触发词)`。命中任一触发词即归入该主题。
///
/// 词表**刻意小而明确**。见 [`extract_pref_key`] 的保守性说明。
/// 触发词一律小写（匹配前把输入也转小写）。
const PREF_TOPICS: &[(&str, &[&str])] = &[
    (
        "编辑器",
        &[
            "vs code", "vscode", "neovim", "nvim", "vim", "emacs", "sublime",
            "jetbrains", "intellij", "编辑器", "ide",
        ],
    ),
    (
        "操作系统",
        &[
            "windows", "macos", "mac os", "linux", "ubuntu", "debian",
            "操作系统", "系统是", "系统用",
        ],
    ),
    (
        "编程语言",
        &[
            "rust", "python", "java", "golang", "go 语言", "typescript",
            "javascript", "c++", "编程语言", "主力语言", "写代码用",
        ],
    ),
    (
        "回复风格",
        &["简洁", "详细", "回复风格", "别啰嗦", "长话短说", "说重点"],
    ),
    (
        "回复语言",
        &["中文", "英文", "english", "回复语言", "用中文", "用英文"],
    ),
    ("称呼", &["叫我", "称呼我", "我的名字"]),
];

/// 从自由文本抽偏好主题 key（设计 §4.5(e) 的前置步骤）。
///
/// [`supersede`] 需要 key 才能判「同主题冲突」，但用户说的是自由文本
/// （"我改用 Neovim 了"）。本函数把文本归到预置主题上，让
/// "我用 VS Code" 与 "我改用 Neovim" 落到同一个 key（"编辑器"）从而互相替换。
///
/// **保守**：只认词表内的明确主题；未命中返回 `None`，调用方应退回普通记忆写入
/// （append + 内容哈希去重），**不要**猜。因为 [`SupersedePlan::Replace`] 会删掉
/// 旧条目——误判两条无关记忆为「同主题」会真的丢信息，代价远高于漏判
/// （漏判只是多留一条冗余记忆）。
///
/// **纯词法**：不调模型。确定性（同输入必得同 key，可重现、可单测）、零延迟、
/// 零 token。代价是覆盖面有限；漏判的主题后续扩词表即可，或等向量语义（P2）。
///
/// 多主题同时命中时，返回**词表中靠前**的那个（词表顺序即优先级），保证确定性。
pub fn extract_pref_key(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    PREF_TOPICS
        .iter()
        .find(|(_, triggers)| triggers.iter().any(|t| lower.contains(t)))
        .map(|(key, _)| key.to_string())
}

/// 一条 standing intent（事件型待办）。
#[derive(Debug, Clone)]
pub struct StandingIntent {
    pub id: String,
    /// 词法触发关键词。
    pub keywords: Vec<String>,
}

/// standing intent 预筛（设计 §4.5）：入站消息命中任一关键词即触发。
///
/// anti-nagging（cooldown/budget/expiry）由 proactive 判定，这里只做词法命中。
pub fn intent_prefilter(msg: &str, intents: &[StandingIntent]) -> Vec<String> {
    let lower = msg.to_lowercase();
    intents
        .iter()
        .filter(|i| i.keywords.iter().any(|k| !k.is_empty() && lower.contains(&k.to_lowercase())))
        .map(|i| i.id.clone())
        .collect()
}

/// 显式记忆意图识别（设计 §4.1 写入路径）：用户说"记住…/别忘了…"等 → curated。
///
/// 纯词法：命中前缀触发词则剥离触发词、返回要记的正文。**保守**：只认明确的
/// 显式指令，不猜；未命中返回 None（走沉淀路径，不是 curated）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitMemory {
    /// 要记住的正文（已剥离触发词）。
    pub content: String,
}

/// 显式"记住"触发前缀（中英）。命中且正文非空 → 用户显式 curated 写入。
const REMEMBER_TRIGGERS: &[&str] = &[
    "记住", "记一下", "记下", "别忘了", "帮我记", "请记住",
    "remember that ", "remember ", "note that ", "please remember ",
];

/// 识别显式记忆意图（设计 §4.1）。命中返回剥离触发词后的正文。
///
/// 触发词只在**消息开头**（去除前导空白后）匹配，避免把"我记住了你说的"这类
/// 陈述误判为写入指令。正文剥离后去除前导标点/空白；为空则视为未命中。
pub fn detect_explicit_memory(msg: &str) -> Option<ExplicitMemory> {
    let trimmed = msg.trim_start();
    let lower = trimmed.to_lowercase();
    for trig in REMEMBER_TRIGGERS {
        if lower.starts_with(trig) {
            // 用字符数切分，兼容中英（trig 是 ASCII 或纯中文，字节前缀等价）。
            let rest = &trimmed[trig.len()..];
            let content = rest
                .trim_start_matches(|c: char| {
                    c.is_whitespace() || c == '：' || c == ':' || c == ',' || c == '，'
                })
                .trim()
                .to_string();
            if !content.is_empty() {
                return Some(ExplicitMemory { content });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, tier: Tier, origin: Origin, text: &str, imp: f64, last: i64) -> MemCandidate {
        MemCandidate { id: id.into(), tier, origin, text: text.into(), importance: imp, last_used_secs: last, source: None }
    }

    #[test]
    fn origin_never_defaults_to_owner() {
        assert_eq!(classify_origin(WriteSource::UserExplicit), Origin::Owner);
        assert_eq!(classify_origin(WriteSource::MainAgent), Origin::Agent);
        assert_eq!(classify_origin(WriteSource::ExternalContent), Origin::Untrusted);
        assert_eq!(classify_origin(WriteSource::Unknown), Origin::Untrusted);
        assert_ne!(classify_origin(WriteSource::BackgroundSession), Origin::Owner);
        assert!(!produces_persistent_candidate(WriteSource::BackgroundSession));
    }

    #[test]
    fn lexical_relevance_counts_hits() {
        let terms = vec!["车".into(), "保险".into()];
        assert_eq!(lexical_relevance("我的车保险到期了", &terms), 1.0);
        assert_eq!(lexical_relevance("我的车很好", &terms), 0.5);
        assert_eq!(lexical_relevance("今天天气不错", &terms), 0.0);
        assert_eq!(lexical_relevance("任意", &[]), 0.0);
    }

    #[test]
    fn halflife_decays_over_time() {
        let hl = 30.0 * 86400.0;
        assert!((halflife_factor(0, 0, hl) - 1.0).abs() < 1e-9); // 刚用过
        let thirty_days = 30 * 86400;
        assert!((halflife_factor(thirty_days, 0, hl) - 0.5).abs() < 1e-6); // 半衰
        assert!(halflife_factor(60 * 86400, 0, hl) < 0.26); // 两个半衰期
    }

    #[test]
    fn rank_orders_by_combined_score() {
        let now = 0;
        let terms = vec!["车".into()];
        let cands = vec![
            // 相关但很旧 → 半衰期压低
            cand("old", Tier::Episodic, Origin::Owner, "车", 1.0, -(60 * 86400)),
            // 相关且新鲜 → 分高
            cand("fresh", Tier::Episodic, Origin::Owner, "车", 1.0, 0),
            // 不相关 → 分 0
            cand("irrel", Tier::Episodic, Origin::Owner, "天气", 1.0, 0),
        ];
        let ranked = rank(&cands, &terms, now, &RankCfg::default());
        assert_eq!(ranked[0].id, "fresh");
        assert_eq!(ranked.last().unwrap().id, "irrel");
        assert!(ranked.last().unwrap().score.abs() < 1e-9);
    }

    #[test]
    fn trigger_only_curated_and_capped() {
        let terms = vec!["偏好".into()];
        let cands = vec![
            cand("c1", Tier::Curated, Origin::Owner, "用户偏好简洁", 1.0, 0),
            cand("c2", Tier::Curated, Origin::Owner, "另一条偏好设置", 1.0, 0),
            cand("c3", Tier::Curated, Origin::Owner, "第三条偏好", 1.0, 0),
            // episodic 不应被自动注入
            cand("e1", Tier::Episodic, Origin::Owner, "偏好偏好", 1.0, 0),
        ];
        let hits = trigger_prefilter("聊到偏好", &cands, &terms, 0.5, 2);
        assert_eq!(hits.len(), 2, "应受 max 限制");
        assert!(!hits.contains(&"e1".to_string()), "episodic 不应自动注入");
        assert!(!is_auto_injectable(Tier::Episodic));
        assert!(is_auto_injectable(Tier::Curated));
    }

    #[test]
    fn supersede_replaces_conflicting_pref() {
        let existing = vec![
            Pref { id: "p1".into(), key: "回复风格".into(), value: "详细".into() },
        ];
        // 同 key 不同值 → 替换
        let inc = Pref { id: "new".into(), key: "回复风格".into(), value: "简洁".into() };
        assert_eq!(supersede(&existing, &inc), SupersedePlan::Replace { existing_id: "p1".into() });
        // 同 key 同值 → 忽略
        let same = Pref { id: "new".into(), key: "回复风格".into(), value: "详细".into() };
        assert_eq!(supersede(&existing, &same), SupersedePlan::Ignore);
        // 新 key → 新增
        let novel = Pref { id: "new".into(), key: "语言".into(), value: "中文".into() };
        assert_eq!(supersede(&existing, &novel), SupersedePlan::Add);
    }

    #[test]
    fn extract_pref_key_matches_known_topics() {
        // 同主题的不同表述必须归到同一 key——这正是 supersede 能判冲突的前提。
        assert_eq!(extract_pref_key("我用 VS Code 写代码").as_deref(), Some("编辑器"));
        assert_eq!(extract_pref_key("我改用 Neovim 了").as_deref(), Some("编辑器"));
        // 大小写不敏感。
        assert_eq!(extract_pref_key("我用 VSCODE").as_deref(), Some("编辑器"));
        assert_eq!(extract_pref_key("我现在用 macOS").as_deref(), Some("操作系统"));
        assert_eq!(extract_pref_key("主力语言是 Rust").as_deref(), Some("编程语言"));
        assert_eq!(extract_pref_key("回复简洁一点").as_deref(), Some("回复风格"));
        assert_eq!(extract_pref_key("叫我老王").as_deref(), Some("称呼"));
    }

    #[test]
    fn extract_pref_key_returns_none_for_non_pref() {
        // 非偏好类内容不得误判——误判会让无关记忆互相覆盖而丢信息。
        assert_eq!(extract_pref_key("周三下午有例会"), None);
        assert_eq!(extract_pref_key("房东电话 138xxxx"), None);
        assert_eq!(extract_pref_key("出差要带转换插头"), None);
        assert_eq!(extract_pref_key(""), None);
    }

    #[test]
    fn extract_pref_key_is_deterministic_on_multi_hit() {
        // 多主题命中时按词表顺序取靠前的（编辑器 在 操作系统 之前），保证可重现。
        let k = extract_pref_key("我在 Windows 上用 VS Code");
        assert_eq!(k.as_deref(), Some("编辑器"));
        // 重复调用结果稳定。
        assert_eq!(extract_pref_key("我在 Windows 上用 VS Code"), k);
    }

    #[test]
    fn intent_prefilter_matches_keywords() {
        let intents = vec![
            StandingIntent { id: "i1".into(), keywords: vec!["周报".into()] },
            StandingIntent { id: "i2".into(), keywords: vec!["生日".into(), "礼物".into()] },
        ];
        let hits = intent_prefilter("帮我准备生日礼物", &intents);
        assert_eq!(hits, vec!["i2".to_string()]);
        assert!(intent_prefilter("今天写代码", &intents).is_empty());
    }

    #[test]
    fn explicit_memory_detection() {
        // 中文触发 + 剥离触发词/标点。
        assert_eq!(
            detect_explicit_memory("记住：我喜欢简洁的回复"),
            Some(ExplicitMemory { content: "我喜欢简洁的回复".into() })
        );
        assert_eq!(
            detect_explicit_memory("别忘了 我对花生过敏"),
            Some(ExplicitMemory { content: "我对花生过敏".into() })
        );
        // 英文触发。
        assert_eq!(
            detect_explicit_memory("remember that I use vim"),
            Some(ExplicitMemory { content: "I use vim".into() })
        );
        // 只在开头触发：陈述句不误判。
        assert_eq!(detect_explicit_memory("我记住了你说的话"), None);
        // 触发词后为空 → 未命中。
        assert_eq!(detect_explicit_memory("记住"), None);
        // 普通消息。
        assert_eq!(detect_explicit_memory("帮我搜索今日头条"), None);
    }
}

/// 情节记忆提取（设计 §11.5）：从会话历史识别值得沉淀为 episodic tier 的候选。
///
/// 过滤掉：
/// - 纯寒暄/空响应（无实质内容）
/// - 过短的（用户提问 <10 字符 或 助手回复 <20 字符）
/// - 显式触发词"记住"开头的（已走 persist_explicit_memory → curated，不重复入 episodic）
///
/// 保留：
/// - 用户分享的事实/背景
/// - 问题+解决方案的成对交互
/// - 有上下文的多轮对话片段
///
/// **纯函数**：不读库、不调模型，确定性（同输入必得同候选）；IO 与编排在 server 侧。
#[derive(Debug, Clone)]
pub struct EpisodeCandidate {
    /// 从历史 entry 拼成的正文（user + assistant 成对，或单条 user/assistant）。
    pub content: String,
    /// 估算重要度 0..1（启发式：长度、是否成对、是否含问号/感叹号等）。
    pub importance: f64,
}

/// 一条历史记录的简化结构（只含提取所需字段）。
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    pub role: &'static str,  // "user" | "assistant" | "tool" | "system"
    pub content: String,
}

/// 从会话历史提取 episodic 候选（设计 §11.5）。
///
/// 策略：用户提问 + 助手回复配对成一条候选（记住的是"我问了X，助手答了Y"这个情节）；
/// 无配对时长度达标的单独保留。工具调用/系统消息忽略（它们不是"人际情节"）。
pub fn extract_episode_candidates(entries: &[ConversationEntry]) -> Vec<EpisodeCandidate> {
    let mut out = Vec::new();
    let mut pending_user: Option<&ConversationEntry> = None;

    // 刷出一条待配对的 user（没等到 assistant 就遇到下一个 user 或历史结束）。
    let flush_lone_user = |out: &mut Vec<EpisodeCandidate>, u: &ConversationEntry| {
        if u.content.chars().count() < MIN_LONE_USER_CHARS {
            return;
        }
        // "记住…" 已由 persist_explicit_memory 写成 curated，不重复沉淀。
        if detect_explicit_memory(&u.content).is_some() {
            return;
        }
        out.push(EpisodeCandidate {
            content: u.content.clone(),
            importance: estimate_importance_single(&u.content, "user"),
        });
    };

    for entry in entries {
        match entry.role {
            "user" => {
                if let Some(u) = pending_user.take() {
                    flush_lone_user(&mut out, u);
                }
                pending_user = Some(entry);
            }
            // 空文本的 assistant 是纯工具调用轮（模型一个字不说直接调工具，P2-4 起
            // 这种轮次也会落库，因为历史重放需要那条 tool_calls）。它不是「人际
            // 情节」的一半：跳过且**保持 pending_user 挂起**，让待配对的提问去等
            // 工具轮结束后真正回答它的那条 assistant。取走它只会配出一条空回答。
            "assistant" if entry.content.trim().is_empty() => {}
            "assistant" => {
                match pending_user.take() {
                    Some(u) => {
                        // 配对：记住的是「问了 X，答了 Y」这个完整情节。
                        // 两条过滤与单条路径一致，否则寒暄与显式记忆会从这里漏进来。
                        let combined =
                            u.content.chars().count() + entry.content.chars().count();
                        if combined >= MIN_PAIR_CHARS
                            && detect_explicit_memory(&u.content).is_none()
                        {
                            out.push(EpisodeCandidate {
                                content: format!("{}\n\n{}", u.content, entry.content),
                                importance: estimate_importance_pair(&u.content, &entry.content),
                            });
                        }
                    }
                    // 无配对的 assistant（如主动消息）：够长才留。
                    None if entry.content.chars().count() >= MIN_LONE_ASSISTANT_CHARS => {
                        out.push(EpisodeCandidate {
                            content: entry.content.clone(),
                            importance: estimate_importance_single(&entry.content, "assistant"),
                        });
                    }
                    None => {}
                }
            }
            // 工具输出/系统注入不是「人际情节」，跳过；待配对的 user 保持挂起，
            // 等工具轮结束后的那条 assistant 与它配对。
            _ => {}
        }
    }

    if let Some(u) = pending_user {
        flush_lone_user(&mut out, u);
    }

    out
}

/// 成对候选的最小合计字数。寒暄天然短（「你好/你好！」5 字），用长度就能滤掉，
/// 不必维护一张寒暄词表——词表永远漏，且会误伤「谢谢，那问题解决了吗」这类有内容的话。
const MIN_PAIR_CHARS: usize = 12;
/// 无配对 user 的最小字数：单方发言信息量低于成对，门槛相应提高。
const MIN_LONE_USER_CHARS: usize = 15;
/// 无配对 assistant 的最小字数。
const MIN_LONE_ASSISTANT_CHARS: usize = 40;

/// 单条消息的重要度估算（启发式）。
fn estimate_importance_single(text: &str, role: &str) -> f64 {
    let len = text.chars().count();
    let mut score: f64 = 0.3; // 单条基准低

    // 长度加分（上限 0.2）。
    if len > 50 {
        score += 0.1;
    }
    if len > 150 {
        score += 0.1;
    }

    // user 提问（含"？"或"怎么/如何"）→ 有价值的情节。
    if role == "user" && (text.contains('?') || text.contains('？') || text.contains("怎么") || text.contains("如何")) {
        score += 0.15;
    }

    // 特定名词/动词（分享背景、报错、决策）→ 情节价值。
    if text.contains("我的")
        || text.contains("遇到")
        || text.contains("出现")
        || text.contains("报错")
        || text.contains("问题")
        || text.contains("决定")
        || text.contains("选择")
    {
        score += 0.1;
    }

    score.min(0.7) // 单条最高 0.7（成对才能到 0.9）
}

/// 成对交互的重要度估算（user + assistant）。
fn estimate_importance_pair(user: &str, assistant: &str) -> f64 {
    let total_len = user.chars().count() + assistant.chars().count();
    let mut score: f64 = 0.55; // 成对基准比单条高

    // 总长度加分。
    if total_len > 100 {
        score += 0.15;
    }
    if total_len > 300 {
        score += 0.1;
    }

    // user 提问 + assistant 回答 → 典型情节。
    if (user.contains('?') || user.contains('？') || user.contains("怎么") || user.contains("如何"))
        && assistant.chars().count() > 10
    {
        score += 0.15;
    }

    // 助手给了具体步骤/代码/清单（实质性回复）→ 有价值。
    if assistant.contains("```") || assistant.contains("1. ") || assistant.contains("- ") {
        score += 0.1;
    }

    score.min(0.9) // 成对上限 0.9
}

#[cfg(test)]
mod episode_tests {
    use super::*;

    fn user(s: &str) -> ConversationEntry {
        ConversationEntry { role: "user", content: s.into() }
    }
    fn asst(s: &str) -> ConversationEntry {
        ConversationEntry { role: "assistant", content: s.into() }
    }
    fn tool(s: &str) -> ConversationEntry {
        ConversationEntry { role: "tool", content: s.into() }
    }

    #[test]
    fn extract_pairs_user_assistant() {
        let entries = vec![user("怎么安装 Rust？"), asst("可以从官网下载 rustup，然后运行安装。")];
        let cands = extract_episode_candidates(&entries);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].content.contains("怎么安装"));
        assert!(cands[0].content.contains("rustup"));
        assert!(cands[0].importance > 0.5, "成对交互重要度应较高");
    }

    /// 纯工具调用轮（空文本 assistant，P2-4 起会落库）不该抢走待配对的提问，
    /// 否则配出「问题 + 空回答」，而真正的回答反倒成了无配对的孤立条目。
    #[test]
    fn empty_assistant_tool_dispatch_does_not_consume_pending_user() {
        let entries = vec![
            user("帮我看看这个项目的依赖装全了没？"),
            asst(""), // 一个字不说直接调工具
            tool("Successfully installed 42 packages"),
            asst("依赖都装全了，一共 42 个包，没有缺失。"),
        ];
        let cands = extract_episode_candidates(&entries);
        assert_eq!(cands.len(), 1, "应只出一条完整情节: {cands:?}");
        assert!(cands[0].content.contains("依赖都装全了"), "配的应是真正的回答");
    }

    #[test]
    fn unpaired_long_user_kept() {
        // 长 user 单条，无对应 assistant → 单独保留。
        let entries = vec![user("我遇到一个问题：编译时报错 cannot find type `Foo` in this scope")];
        let cands = extract_episode_candidates(&entries);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].importance < 0.7, "单条上限 0.7");
    }

    #[test]
    fn short_messages_filtered() {
        // 短寒暄不保留。
        let entries = vec![user("你好"), asst("你好！"), user("谢谢"), asst("不客气")];
        let cands = extract_episode_candidates(&entries);
        assert!(cands.is_empty(), "短寒暄应被过滤：{:?}", cands);
    }

    #[test]
    fn explicit_memory_not_duplicated() {
        // "记住"开头的走了 persist_explicit_memory（curated），不该重复进 episodic。
        let entries = vec![user("记住我喜欢简洁回复"), asst("好的，已记住。")];
        let cands = extract_episode_candidates(&entries);
        // user 被过滤（显式记忆），assistant 太短也被过滤。
        assert!(cands.is_empty(), "显式记忆不应重复入 episodic：{:?}", cands);
    }

    #[test]
    fn tool_messages_ignored() {
        // 工具输出不是情节记忆。
        let entries = vec![
            user("查一下今天天气"),
            tool("天气查询结果：晴天 22°C"),
            asst("今天天气不错，晴天 22°C。"),
        ];
        let cands = extract_episode_candidates(&entries);
        // user + assistant 成对保留，tool 被跳过。
        assert_eq!(cands.len(), 1);
        assert!(cands[0].content.contains("查一下今天天气"));
        assert!(!cands[0].content.contains("天气查询结果"), "tool 内容不该进入情节");
    }

    #[test]
    fn multiple_pairs_extracted() {
        let entries = vec![
            user("什么是 ownership？"),
            asst("Rust 的所有权系统..."),
            user("那 borrow 呢？"),
            asst("借用是暂时获得引用..."),
        ];
        let cands = extract_episode_candidates(&entries);
        assert_eq!(cands.len(), 2, "两对对话应产生两条候选");
    }

    #[test]
    fn importance_estimates_deterministic() {
        let entries = vec![user("我的项目报错了"), asst("可以检查一下日志")];
        let c1 = extract_episode_candidates(&entries);
        let c2 = extract_episode_candidates(&entries);
        assert_eq!(c1[0].importance, c2[0].importance, "同输入必得同 importance");
    }
}
