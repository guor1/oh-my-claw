//! Dreaming 双门判定纯策略（设计 §4.5(d)、§9 巩固）。
//!
//! 夜间/空闲时，server 把 episodic 沉淀候选喂进 `dreaming_gate`，通过双门的候选
//! 交给"巩固模型轮"重写 MEMORY.md（该轮由 server 发起，本函数不做任何 IO）。
//!
//! - **门1（确定性排名门）**：分数（importance）/ 频次（use_count）/ 时间窗（age）三条硬阈值。
//! - **门2（结构排除门）**：`origin ∈ {Untrusted, System}` 直接排除，绝不巩固进 curated。
//!
//! 纯函数：相同输入 → 相同输出，100% 可单测。失败/空集绝不阻塞主会话（由 server 保证）。

use crate::memory::{Origin, Tier};

/// 一条待巩固候选（从 store 取回的字段映射而来）。
#[derive(Debug, Clone)]
pub struct DreamCandidate {
    pub id: String,
    pub tier: Tier,
    pub origin: Origin,
    /// 重要度 0..1。
    pub importance: f64,
    /// 被引用/命中的累计次数（频次门）。
    pub use_count: u32,
    /// 距创建时间的秒数（时间窗门）。
    pub age_secs: i64,
}

/// dreaming 判定配置。
#[derive(Debug, Clone)]
pub struct DreamCfg {
    /// 门1 分数下限：importance ≥ 此值。
    pub min_importance: f64,
    /// 门1 频次下限：use_count ≥ 此值（反复被用到才值得沉淀为长期记忆）。
    pub min_use_count: u32,
    /// 门1 时间窗下限：太新（还没沉淀稳定）不巩固。
    pub min_age_secs: i64,
    /// 门1 时间窗上限：太旧（已过时）不巩固；0 表示不设上限。
    pub max_age_secs: i64,
    /// 每次巩固轮的候选上限（成本/预算控制，设计 §9）。
    pub max_consolidations: usize,
}

impl Default for DreamCfg {
    fn default() -> Self {
        Self {
            min_importance: 0.5,
            min_use_count: 2,
            min_age_secs: 3 * 86400,      // 沉淀 ≥3 天
            max_age_secs: 180 * 86400,    // ≤180 天（更旧的让它自然半衰）
            max_consolidations: 8,
        }
    }
}

/// 被排除的原因（用于审计/可解释性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReject {
    /// tier 非 episodic（curated 已在册，无需巩固）。
    NotEpisodic,
    /// 门2：origin 属于 Untrusted/System，结构性排除。
    UntrustedOrigin,
    /// 门1：importance 低于阈值。
    LowImportance,
    /// 门1：频次不足。
    LowUseCount,
    /// 门1：太新，还没沉淀。
    TooRecent,
    /// 门1：太旧，已过时。
    TooOld,
}

/// 通过双门、待交给巩固模型轮的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Consolidation {
    pub id: String,
    /// 巩固优先级 = importance × use_count，越大越先重写。
    pub priority: f64,
}

/// 双门判定（设计 §4.5(d)）。
///
/// 依次过门1（分数/频次/时间窗）与门2（结构排除 Untrusted/System），
/// 通过者按 `priority = importance × use_count` 降序，截断到 `max_consolidations`。
pub fn dreaming_gate(cands: &[DreamCandidate], cfg: &DreamCfg) -> Vec<Consolidation> {
    let mut passed: Vec<Consolidation> = cands
        .iter()
        .filter(|c| gate_check(c, cfg).is_ok())
        .map(|c| Consolidation {
            id: c.id.clone(),
            priority: c.importance * c.use_count as f64,
        })
        .collect();
    passed.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    passed.truncate(cfg.max_consolidations);
    passed
}

/// 巩固模型轮的系统提示词（设计 §11.4「巩固模型轮重写 MEMORY.md」）。
///
/// 要求模型**重写**而非追加：把零散记忆归并成条目清晰、无重复、无矛盾的清单。
/// 明确禁止编造——只允许重组输入里已有的事实，否则夜间无人监督的重写会往
/// 长期记忆里掺入幻觉，而 MEMORY.md 是每轮都注入的 curated 核心。
pub const CONSOLIDATION_SYSTEM_PROMPT: &str = "\
你在整理一份长期记忆清单（MEMORY.md）。把输入的零散记忆条目重写成一份干净的 Markdown 清单。

规则：
1. 只使用输入中已有的事实。**绝对不要**推断、扩写或编造任何未出现的信息。
2. 合并重复或高度相似的条目；同一主题有矛盾时保留**更具体/更近期**的表述。
3. 按主题分组，每组一个 `## 小标题`，组内用 `- ` 列出条目。
4. 每条尽量短，一行一件事。不要加前言、结语、解释或元评论。
5. 直接输出 Markdown 正文，不要包在代码块里。";

/// 组装巩固模型轮的用户提示（纯函数：给定条目 → 确定性 prompt）。
///
/// `existing` 是当前 MEMORY.md 正文（可空）；`items` 是本轮双门通过的记忆文本。
/// 既有内容一并交给模型，让它**合并**而不是只看新条目——否则重写会丢掉旧记忆。
pub fn build_consolidation_prompt(existing: &str, items: &[&str]) -> String {
    let mut s = String::new();
    if !existing.trim().is_empty() {
        s.push_str("## 当前 MEMORY.md 内容\n\n");
        s.push_str(existing.trim());
        s.push_str("\n\n");
    }
    s.push_str("## 本轮新巩固的记忆条目\n\n");
    for it in items {
        let t = it.trim();
        if !t.is_empty() {
            s.push_str("- ");
            s.push_str(t);
            s.push('\n');
        }
    }
    s.push_str("\n请把以上内容重写成一份合并去重后的完整清单。");
    s
}

// ── FEAT-4：巩固四动作（CREATE/CORROBORATE/REFINE/CORRECT）───────────────
//
// 借鉴 ReMe auto_dream Integrate 阶段的四种动作语义。原来巩固是「把新旧内容
// 一起丢给模型自由重写 MEMORY.md」，合并黑盒、不可审计。这里把「每条新记忆
// 该以何种动作落到哪条旧 curated」提炼成受约束的显式决策，落库 + 审计。

/// 一条新记忆该对旧 curated 采取的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidateAction {
    /// 没有相同抽象，新建一条。
    Create,
    /// 同一记忆再次出现，追加来源、强化表述。
    Corroborate,
    /// 新材料补充了边界/步骤/前提/适用范围。
    Refine,
    /// 新材料修正了旧节点的错误/遗漏/冲突。
    Correct,
}

impl ConsolidateAction {
    /// 审计用的短标签（如 `consolidate:create`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ConsolidateAction::Create => "create",
            ConsolidateAction::Corroborate => "corroborate",
            ConsolidateAction::Refine => "refine",
            ConsolidateAction::Correct => "correct",
        }
    }
}

/// 模型针对一条新记忆做出的巩固决策。
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ConsolidateItem {
    /// 新记忆的 id（`mem-<hash>`）。
    pub source_id: String,
    /// 动作。
    pub action: ConsolidateAction,
    /// 动作落到哪条旧 curated（`Create` 时为 `None`）。
    pub target_id: Option<String>,
    /// 合并后的正文（`Create` 时为新记忆正文本身）。
    pub merged_text: String,
}

/// 单次巩固轮的输出模型：一轮模型调用返回若干条决策。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConsolidateResponse {
    pub actions: Vec<ConsolidateItem>,
}

/// 巩固模型轮的结构化系统提示词（FEAT-4）。
///
/// 要求模型**只输出 JSON**，每个动作的语义被约束为四种之一。与 [`CONSOLIDATION_SYSTEM_PROMPT`]
/// 的「防编造」是同一路数的防御：把夜间无人监督的合并从「自由发挥」收进四个标准动作，
/// 每条都留痕可审计。
pub const CONSOLIDATE_JSON_SYSTEM_PROMPT: &str = "\
你在整合一批新的长期记忆候选（episodic）到既有的长期记忆（curated）里。

对每一条新候选，判断它与既有 curated 的关系，并输出一个 JSON 数组。动作只有四种：
- create：既有记忆里没有相同抽象，新建一条。target_id 为 null。
- corroborate：同一事实再次出现，印证并强化旧条目。target_id 指向旧条目。
- refine：新材料补充了边界、步骤、前提或适用范围。target_id 指向旧条目。
- correct：新材料修正了旧条目的错误、遗漏或冲突。target_id 指向旧条目。

规则：
1. 只使用输入里已有的事实，**绝对不要**推断、扩写或编造任何未出现的信息。
2. merged_text 是合并后的最终正文（对 create，等于候选原文；对其它动作，是
   候选与旧条目合并、去重、修正后的表述）。
3. 每条候选必须产生一条决策；找不到对应旧条目时用 create。
4. 只输出 JSON，不要任何解释、前言或 Markdown 代码块。

输出格式：
{\"actions\":[{\"source_id\":\"...\",\"action\":\"create|corroborate|refine|correct\",\"target_id\":\"...\"|\"target_id\":null,\"merged_text\":\"...\"}]}";

/// 组装巩固模型轮的结构化用户提示（FEAT-4，纯函数）。
///
/// `existing_curated` 是既有 curated 记忆（id + 正文）；`items` 是本轮双门通过的
/// 新候选（id + 正文）。既有一并交给模型，让它能在旧条目上判断动作与落点。
pub fn build_consolidate_decision_prompt(
    existing_curated: &[(String, String)],
    items: &[(String, String)],
) -> String {
    let mut s = String::new();
    if !existing_curated.is_empty() {
        s.push_str("## 既有长期记忆（curated）\n\n");
        for (id, text) in existing_curated {
            s.push_str(&format!("- [{id}] {}\n", text.trim()));
        }
        s.push('\n');
    }
    s.push_str("## 本轮新巩固候选（episodic）\n\n");
    for (id, text) in items {
        s.push_str(&format!("- [{id}] {}\n", text.trim()));
    }
    s.push_str("\n请按系统要求输出 JSON。");
    s
}

/// 解析模型输出的结构化决策（FEAT-4，纯函数）。
///
/// 宽容解析：剥掉可能的 Markdown 代码块围栏，再从正文里找第一个 `{` 起的 JSON 对象。
/// 失败返回 `None`，调用方回落到旧的自由重写路径（能力不因解析失败而丢）。
pub fn parse_consolidations(raw: &str) -> Option<Vec<ConsolidateItem>> {
    let trimmed = raw.trim();
    // 模型常无视「不要包代码块」，剥掉 ```json ... ``` 围栏。
    let json = if let Some(stripped) = trimmed
        .strip_prefix("```")
        .and_then(|s| s.strip_prefix("json").or(Some(s)))
        .map(str::trim)
        .and_then(|s| s.strip_suffix("```"))
    {
        stripped
    } else {
        trimmed
    };
    // 从第一个 '{' 起，到最后一个 '}' 止，容忍前后夹带文字。
    let start = json.find('{')?;
    let end = json.rfind('}')?;
    if end <= start {
        return None;
    }
    let obj = &json[start..=end];
    let resp: ConsolidateResponse = serde_json::from_str(obj).ok()?;
    Some(resp.actions)
}

/// MEMORY.md 的写入决策（设计 §11.4「写安全：乐观并发」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePlan {
    /// 文件未被并发修改 → 可安全整体覆盖（原子 rename）。
    Overwrite,
    /// 文件在本轮生成期间被改动（用户手编 / 另一进程）→ **不覆盖**，
    /// 退化为追加到文末，避免吞掉别人的修改。
    AppendOnly,
}

/// 乐观并发判定（纯函数）：比对读取时与写入前的内容哈希。
///
/// 调用方在**生成前**读一次文件算 `hash_before`，模型生成完、落盘前**再读一次**算
/// `hash_now`。两者相同说明期间无人动过，可以整体重写；不同则说明有并发写入，
/// 此时覆盖会丢掉对方的改动，故退化为 append-only。
///
/// 纯函数不做 IO：哈希由调用方（server）算好传入。
pub fn decide_write(hash_before: &str, hash_now: &str) -> WritePlan {
    if hash_before == hash_now {
        WritePlan::Overwrite
    } else {
        WritePlan::AppendOnly
    }
}

/// 单候选双门检查：通过返回 `Ok(())`，否则返回**首个**未过的门（便于审计）。
pub fn gate_check(c: &DreamCandidate, cfg: &DreamCfg) -> Result<(), GateReject> {
    // 只巩固 episodic 沉淀（curated 已在册）。
    if c.tier != Tier::Episodic {
        return Err(GateReject::NotEpisodic);
    }
    // 门2：结构排除 —— 先于门1，来源不可信一票否决。
    if matches!(c.origin, Origin::Untrusted | Origin::System) {
        return Err(GateReject::UntrustedOrigin);
    }
    // 门1：分数 / 频次 / 时间窗。
    if c.importance < cfg.min_importance {
        return Err(GateReject::LowImportance);
    }
    if c.use_count < cfg.min_use_count {
        return Err(GateReject::LowUseCount);
    }
    if c.age_secs < cfg.min_age_secs {
        return Err(GateReject::TooRecent);
    }
    if cfg.max_age_secs > 0 && c.age_secs > cfg.max_age_secs {
        return Err(GateReject::TooOld);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, tier: Tier, origin: Origin, imp: f64, uses: u32, age: i64) -> DreamCandidate {
        DreamCandidate { id: id.into(), tier, origin, importance: imp, use_count: uses, age_secs: age }
    }

    fn good() -> DreamCandidate {
        // 满足所有门的基准候选。
        cand("ok", Tier::Episodic, Origin::Owner, 0.8, 5, 10 * 86400)
    }

    #[test]
    fn baseline_passes_both_gates() {
        let cfg = DreamCfg::default();
        assert!(gate_check(&good(), &cfg).is_ok());
        let out = dreaming_gate(&[good()], &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ok");
    }

    #[test]
    fn gate2_excludes_untrusted_and_system() {
        let cfg = DreamCfg::default();
        let untrusted = cand("u", Tier::Episodic, Origin::Untrusted, 0.9, 9, 10 * 86400);
        let system = cand("s", Tier::Episodic, Origin::System, 0.9, 9, 10 * 86400);
        assert_eq!(gate_check(&untrusted, &cfg), Err(GateReject::UntrustedOrigin));
        assert_eq!(gate_check(&system, &cfg), Err(GateReject::UntrustedOrigin));
        // 高分高频也无法绕过结构门。
        assert!(dreaming_gate(&[untrusted, system], &cfg).is_empty());
    }

    #[test]
    fn gate1_thresholds() {
        let cfg = DreamCfg::default();
        // 分数不足
        assert_eq!(
            gate_check(&cand("a", Tier::Episodic, Origin::Owner, 0.4, 5, 10 * 86400), &cfg),
            Err(GateReject::LowImportance)
        );
        // 频次不足
        assert_eq!(
            gate_check(&cand("b", Tier::Episodic, Origin::Owner, 0.8, 1, 10 * 86400), &cfg),
            Err(GateReject::LowUseCount)
        );
        // 太新
        assert_eq!(
            gate_check(&cand("c", Tier::Episodic, Origin::Owner, 0.8, 5, 86400), &cfg),
            Err(GateReject::TooRecent)
        );
        // 太旧
        assert_eq!(
            gate_check(&cand("d", Tier::Episodic, Origin::Owner, 0.8, 5, 365 * 86400), &cfg),
            Err(GateReject::TooOld)
        );
    }

    #[test]
    fn non_episodic_never_consolidated() {
        let cfg = DreamCfg::default();
        let curated = cand("cur", Tier::Curated, Origin::Owner, 0.9, 9, 10 * 86400);
        assert_eq!(gate_check(&curated, &cfg), Err(GateReject::NotEpisodic));
    }

    #[test]
    fn priority_orders_and_caps() {
        let cfg = DreamCfg { max_consolidations: 2, ..DreamCfg::default() };
        let cands = vec![
            cand("low", Tier::Episodic, Origin::Owner, 0.6, 2, 10 * 86400),   // prio 1.2
            cand("high", Tier::Episodic, Origin::Owner, 0.9, 8, 10 * 86400),  // prio 7.2
            cand("mid", Tier::Episodic, Origin::Owner, 0.8, 4, 10 * 86400),   // prio 3.2
        ];
        let out = dreaming_gate(&cands, &cfg);
        assert_eq!(out.len(), 2, "受 max_consolidations 截断");
        assert_eq!(out[0].id, "high");
        assert_eq!(out[1].id, "mid");
    }

    #[test]
    fn max_age_zero_means_no_upper_bound() {
        let cfg = DreamCfg { max_age_secs: 0, ..DreamCfg::default() };
        let ancient = cand("old", Tier::Episodic, Origin::Owner, 0.8, 5, 3650 * 86400);
        assert!(gate_check(&ancient, &cfg).is_ok());
    }

    #[test]
    fn decide_write_detects_concurrent_change() {
        // 哈希未变 → 期间无人动过，可整体重写。
        assert_eq!(decide_write("abc", "abc"), WritePlan::Overwrite);
        // 哈希变了 → 有并发写入，覆盖会吞掉对方改动，退化为追加。
        assert_eq!(decide_write("abc", "xyz"), WritePlan::AppendOnly);
    }

    #[test]
    fn consolidation_prompt_includes_existing_and_new() {
        let p = build_consolidation_prompt("## 旧\n- 老条目", &["新条目 A", "新条目 B"]);
        // 既有内容必须带上，否则重写会丢掉旧记忆。
        assert!(p.contains("老条目"), "应包含当前 MEMORY.md 内容: {p}");
        assert!(p.contains("新条目 A") && p.contains("新条目 B"));
        assert!(p.contains("重写"), "应给出重写指令");
    }

    #[test]
    fn consolidation_prompt_handles_empty_existing() {
        // 首次巩固（MEMORY.md 为空模板）不应出现空的"当前内容"节。
        let p = build_consolidation_prompt("   \n  ", &["条目"]);
        assert!(!p.contains("当前 MEMORY.md 内容"), "空既有内容应跳过该节: {p}");
        assert!(p.contains("条目"));
    }

    #[test]
    fn consolidation_prompt_is_deterministic() {
        let a = build_consolidation_prompt("x", &["1", "2"]);
        let b = build_consolidation_prompt("x", &["1", "2"]);
        assert_eq!(a, b, "同输入必须得同 prompt（可重现）");
    }

    #[test]
    fn consolidation_prompt_skips_blank_items() {
        let p = build_consolidation_prompt("", &["有效", "   ", ""]);
        assert!(p.contains("- 有效"));
        // 空条目不该产生空的 "- " 行。
        assert!(!p.contains("- \n"), "空白条目应被跳过: {p:?}");
    }

    // ── FEAT-4：四动作决策解析 ─────────────────────────────────

    #[test]
    fn parse_consolidations_parses_plain_json() {
        let raw = r#"{"actions":[{"source_id":"e1","action":"create","target_id":null,"merged_text":"新条目"}]}"#;
        let items = parse_consolidations(raw).expect("应解析成功");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_id, "e1");
        assert_eq!(items[0].action, ConsolidateAction::Create);
        assert_eq!(items[0].target_id, None);
        assert_eq!(items[0].merged_text, "新条目");
    }

    #[test]
    fn parse_consolidations_strips_code_fence() {
        let raw = "```json\n{\"actions\":[{\"source_id\":\"e1\",\"action\":\"refine\",\"target_id\":\"c1\",\"merged_text\":\"补全后\"}]}\n```";
        let items = parse_consolidations(raw).expect("应剥掉代码块围栏");
        assert_eq!(items[0].action, ConsolidateAction::Refine);
        assert_eq!(items[0].target_id.as_deref(), Some("c1"));
    }

    #[test]
    fn parse_consolidations_tolerates_surrounding_text() {
        let raw = "好的，结果如下：\n{\"actions\":[{\"source_id\":\"e1\",\"action\":\"correct\",\"target_id\":\"c1\",\"merged_text\":\"修正后\"}]}\n以上。";
        let items = parse_consolidations(raw).expect("应容忍前后夹带文字");
        assert_eq!(items[0].action, ConsolidateAction::Correct);
    }

    #[test]
    fn parse_consolidations_returns_none_on_garbage() {
        assert!(parse_consolidations("这不是 JSON").is_none());
        assert!(parse_consolidations("").is_none());
        assert!(parse_consolidations("{}").is_none(), "缺 actions 字段应失败");
    }

    #[test]
    fn parse_consolidations_rejects_unknown_action() {
        // 未知动作值应解析失败（serde 严格枚举），而非静默映射。
        let raw = r#"{"actions":[{"source_id":"e1","action":"hack","target_id":null,"merged_text":"x"}]}"#;
        assert!(parse_consolidations(raw).is_none());
    }

    #[test]
    fn decision_prompt_includes_both_sides() {
        let p = build_consolidate_decision_prompt(
            &[("c1".into(), "旧条目".into())],
            &[("e1".into(), "新候选".into())],
        );
        assert!(p.contains("[c1] 旧条目"), "既有 curated 必须带 id 进 prompt: {p}");
        assert!(p.contains("[e1] 新候选"), "新候选必须带 id 进 prompt: {p}");
    }

    #[test]
    fn decision_prompt_skips_empty_curated_section() {
        let p = build_consolidate_decision_prompt(&[], &[("e1".into(), "新候选".into())]);
        assert!(!p.contains("既有长期记忆"), "无既有 curated 应跳过该节: {p}");
        assert!(p.contains("[e1] 新候选"));
    }
}
