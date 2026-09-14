//! Dreaming 巩固调度（设计 §4.5(d)、§7.4、§11.4）。
//!
//! 心跳 tick 周期性触发：从 store 取 episodic 候选 → oc-core 双门判定 →
//! 通过者就地巩固为 curated + 写审计 →（P1-4）跑**巩固模型轮**重写 MEMORY.md。
//! **判定纯在 core，读写在 store/本模块，调度在此**。
//! 失败绝不阻塞主会话（本函数只告警，不向上传播错误）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_core::dreaming::{
    build_consolidate_decision_prompt, build_consolidation_prompt, decide_write, dreaming_gate,
    parse_consolidations, ConsolidateAction, DreamCandidate, DreamCfg, WritePlan,
    CONSOLIDATE_JSON_SYSTEM_PROMPT, CONSOLIDATION_SYSTEM_PROMPT,
};
use oc_core::memory::{Origin as CoreOrigin, Tier as CoreTier};
use oc_llm::{Delta, Message, ModelRequest, MsgRole, Provider};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 单次巩固候选拉取上限。
const SCAN_LIMIT: i64 = 64;

/// 巩固模型轮的墙钟上限（夜间后台任务，给足时间但不无限等）。
const CONSOLIDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// 巩固模型轮所需上下文。`None` 时只做 DB 内 tier 提升（保持 P1-4 之前的行为）。
#[derive(Clone)]
pub struct ConsolidateCtx {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// `~/.oc/soul/` 目录；MEMORY.md 写在其下。
    pub soul_dir: PathBuf,
}

/// 执行一轮 dreaming 巩固扫描（不含模型轮）。返回本轮巩固的记忆条数。
///
/// 保留此签名供既有调用方/测试使用；要跑 MEMORY.md 重写请用 [`scan_with`]。
pub async fn scan(store: &oc_store::Store, now_secs: i64, cfg: &DreamCfg) -> usize {
    scan_with(store, now_secs, cfg, None).await
}

/// 执行一轮 dreaming 巩固扫描，`ctx` 非空时追加**巩固模型轮**（§11.4）。
///
/// FEAT-4：巩固模型轮不再自由重写，而是对每条双门通过的新候选输出结构化动作
/// （CREATE/CORROBORATE/REFINE/CORRECT），逐条落库 + 审计，再用各动作产出的
/// `merged_text` 拼成 MEMORY.md。模型调用失败 / 输出无法解析时回落到「全部
/// CREATE」（就地 promote，等价旧行为），能力不因解析失败而丢。
pub async fn scan_with(
    store: &oc_store::Store,
    now_secs: i64,
    cfg: &DreamCfg,
    ctx: Option<&ConsolidateCtx>,
) -> usize {
    let rows = match store.dream_candidates(SCAN_LIMIT).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "dreaming：取候选失败，跳过本轮");
            return 0;
        }
    };
    if rows.is_empty() {
        return 0;
    }

    // 映射为 core 候选（age 用 now - created_at；毫秒转秒）。
    let cands: Vec<DreamCandidate> = rows
        .iter()
        .map(|r| DreamCandidate {
            id: r.id.clone(),
            tier: match r.tier {
                oc_store::Tier::Curated => CoreTier::Curated,
                oc_store::Tier::Episodic => CoreTier::Episodic,
                oc_store::Tier::Prospective => CoreTier::Prospective,
                oc_store::Tier::Review => CoreTier::Review,
            },
            origin: match r.origin {
                oc_store::Origin::Owner => CoreOrigin::Owner,
                oc_store::Origin::Agent => CoreOrigin::Agent,
                oc_store::Origin::Untrusted => CoreOrigin::Untrusted,
                oc_store::Origin::System => CoreOrigin::System,
            },
            importance: r.importance,
            use_count: r.use_count.max(0) as u32,
            age_secs: (now_secs - r.created_at / 1000).max(0),
        })
        .collect();

    let consolidations = dreaming_gate(&cands, cfg);
    if consolidations.is_empty() {
        return 0;
    }

    // id → 原文，供动作执行与 MEMORY.md 重写用。
    let passed: Vec<(String, String)> = consolidations
        .iter()
        .filter_map(|c| rows.iter().find(|r| r.id == c.id))
        .map(|r| (r.id.clone(), r.text.clone()))
        .collect();

    // 既有 curated（id + text），供模型判断「新候选落到哪条旧条目」。
    let curated = match store.curated_list().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "dreaming：取既有 curated 失败，回退全部 CREATE");
            Vec::new()
        }
    };

    // 巩固模型轮：让模型对每条新候选输出结构化动作。无 ctx / 调用失败 / 解析失败
    // 都回落到「全部 CREATE」（就地 promote，等价旧行为）。
    let items = match ctx {
        Some(c) => {
            let prompt = build_consolidate_decision_prompt(&curated, &passed);
            match run_model_json(c, &prompt).await.and_then(|raw| parse_consolidations(&raw)) {
                Some(items) => items,
                None => {
                    warn!("dreaming：巩固模型轮无输出或解析失败，回退全部 CREATE");
                    passed
                        .iter()
                        .map(|(id, text)| oc_core::dreaming::ConsolidateItem {
                            source_id: id.clone(),
                            action: ConsolidateAction::Create,
                            target_id: None,
                            merged_text: text.clone(),
                        })
                        .collect()
                }
            }
        }
        None => passed
            .iter()
            .map(|(id, text)| oc_core::dreaming::ConsolidateItem {
                source_id: id.clone(),
                action: ConsolidateAction::Create,
                target_id: None,
                merged_text: text.clone(),
            })
            .collect(),
    };

    // 逐条执行动作 + 审计。容错：单条失败不拖垮整轮。
    let mut merged_texts: Vec<String> = Vec::new();
    for item in &items {
        // 源 id 必须是本轮通过双门的候选之一（防模型编造出库里不存在的 id）。
        if !passed.iter().any(|(id, _)| id == &item.source_id) {
            warn!(source = %item.source_id, "dreaming：模型输出了未知 source_id，跳过");
            continue;
        }
        let action = item.action;
        let merged_hash = content_hash(&item.merged_text);
        match action {
            ConsolidateAction::Create => {
                match store.writer().promote_memory(item.source_id.clone()).await {
                    Ok(()) => merged_texts.push(item.merged_text.clone()),
                    Err(e) => {
                        warn!(id = %item.source_id, error = %e, "dreaming：create 巩固失败");
                        continue;
                    }
                }
            }
            _ => {
                // 非 create：target_id 必须存在；否则退化为 create。
                match &item.target_id {
                    Some(t) if curated.iter().any(|(id, _)| id == t) => {
                        match store
                            .writer()
                            .merge_memory(
                                item.source_id.clone(),
                                t.clone(),
                                item.merged_text.clone(),
                                merged_hash.clone(),
                            )
                            .await
                        {
                            Ok(()) => merged_texts.push(item.merged_text.clone()),
                            Err(e) => {
                                warn!(source = %item.source_id, target = %t, error = %e, "dreaming：合并失败");
                                continue;
                            }
                        }
                    }
                    _ => {
                        // 无合法落点 → 退化为 create。
                        match store.writer().promote_memory(item.source_id.clone()).await {
                            Ok(()) => merged_texts.push(item.merged_text.clone()),
                            Err(e) => {
                                warn!(id = %item.source_id, error = %e, "dreaming：回退 create 失败");
                                continue;
                            }
                        }
                    }
                }
            }
        }
        let audit_action = format!("consolidate:{}", action.as_str());
        if let Err(e) = store
            .writer()
            .write_audit("dreaming".into(), audit_action, Some(item.source_id.clone()))
            .await
        {
            warn!(id = %item.source_id, error = %e, "dreaming：审计写入失败");
        }
    }

    let promoted = merged_texts.len();
    if promoted > 0 {
        info!(promoted, "dreaming：本轮巩固完成");
        // 用各动作产出的合并正文重写 MEMORY.md（§11.4）。
        if let Some(ctx) = ctx {
            rewrite_memory_md(ctx, store, &merged_texts).await;
        }
    }
    promoted
}

/// 跑一轮巩固模型调用，返回结构化 JSON 文本（FEAT-4）。超时/失败/空返回 None。
async fn run_model_json(ctx: &ConsolidateCtx, prompt: &str) -> Option<String> {
    run_model_with_system(ctx, CONSOLIDATE_JSON_SYSTEM_PROMPT, prompt).await
}

/// 跑一轮巩固模型调用，指定系统提示词。超时/失败/空返回 None。
async fn run_model_with_system(ctx: &ConsolidateCtx, system: &str, prompt: &str) -> Option<String> {
    let req = ModelRequest {
        model: ctx.model.clone(),
        system: Some(system.to_string()),
        messages: vec![Message {
            role: MsgRole::User,
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        }],
        tools: Vec::new(), // 巩固轮不带工具。
        max_tokens: None,
        temperature: None,
    };
    let cancel = CancellationToken::new();
    let run = async {
        let mut stream = match ctx.provider.stream_chat(req, cancel.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "dreaming：巩固模型调用失败");
                return String::new();
            }
        };
        let mut acc = String::new();
        while let Some(delta) = stream.next().await {
            match delta {
                Ok(Delta::Text(t)) => acc.push_str(&t),
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "dreaming：巩固流中断");
                    break;
                }
            }
        }
        acc
    };
    let out = match tokio::time::timeout(CONSOLIDATE_TIMEOUT, run).await {
        Ok(s) => s,
        Err(_) => {
            cancel.cancel();
            warn!("dreaming：巩固模型轮超时");
            return None;
        }
    };
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 巩固模型轮 + 乐观并发写 MEMORY.md（设计 §11.4）。
///
/// 流程：读文件算 hash → 模型重写 → **再读一次**算 hash → `core::decide_write` 判定
/// → 未变则原子 rename 覆盖；变了则退化 append-only（不吞掉用户/他人的改动）。
async fn rewrite_memory_md(ctx: &ConsolidateCtx, store: &oc_store::Store, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let path = ctx.soul_dir.join("MEMORY.md");

    // 生成前读一次：既作为模型输入（要合并而非丢弃旧内容），也作为并发基线。
    let before = read_or_empty(&path);
    let hash_before = content_hash(&before);

    let refs: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
    let prompt = build_consolidation_prompt(&before, &refs);
    let Some(new_body) = run_model_with_system(ctx, CONSOLIDATION_SYSTEM_PROMPT, &prompt).await else {
        warn!("dreaming：巩固模型轮无输出，MEMORY.md 保持不变");
        return;
    };

    // 落盘前再读一次，比对哈希判有无并发修改。
    let hash_now = content_hash(&read_or_empty(&path));
    let plan = decide_write(&hash_before, &hash_now);

    let result = match plan {
        WritePlan::Overwrite => {
            debug!("dreaming：MEMORY.md 未被并发修改，整体重写");
            atomic_write(&path, &ensure_trailing_newline(&new_body))
        }
        WritePlan::AppendOnly => {
            // 期间有人改过：覆盖会丢掉对方的修改，改为追加。
            warn!("dreaming：MEMORY.md 期间被修改，退化为追加（不覆盖）");
            append_section(&path, &new_body)
        }
    };

    match result {
        Ok(()) => {
            let action = match plan {
                WritePlan::Overwrite => "rewrite_memory_md",
                WritePlan::AppendOnly => "append_memory_md",
            };
            info!(?plan, items = items.len(), "dreaming：MEMORY.md 已更新");
            if let Err(e) = store
                .writer()
                .write_audit("dreaming".into(), action.into(), None)
                .await
            {
                warn!(error = %e, "dreaming：MEMORY.md 写入审计失败");
            }
        }
        Err(e) => warn!(error = %e, path = %path.display(), "dreaming：MEMORY.md 写入失败"),
    }
}

/// 读文件，不存在/读失败均返回空串（首次巩固时文件可能还没建）。
fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 原子写：先写同目录 `.tmp`，再 rename 覆盖。
///
/// 同目录是关键——跨盘/跨文件系统的 rename 不保证原子。中途崩溃时原文件仍完整。
/// Windows 上 `fs::rename` 覆盖已存在文件会失败，故先删目标再 rename。
fn atomic_write(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // 别留垃圾。
            Err(e)
        }
    }
}

/// 追加一节到文末（并发退化路径）。带分隔标记，便于人工辨认与后续合并。
fn append_section(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "\n<!-- dreaming 追加（检测到并发修改，未覆盖原文） -->")?;
    writeln!(f, "{}", body.trim())?;
    Ok(())
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// 内容哈希（FNV-1a，16 位十六进制）。仅用于「变没变」的比对，非安全用途。
fn content_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
