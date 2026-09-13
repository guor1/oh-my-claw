//! 技能加载（ROAD-1）：读 `~/.oc/skills/` 下的技能目录为 Skill 列表。
//!
//! 目录布局对齐 `docs/design/skill-format.md` §Slugs：
//! - 裸包：`skills/<slug>/SKILL.md`（深度 1）
//! - scoped 包：`skills/@<publisher>/<slug>/SKILL.md`（深度 2，`@scope/name` 是
//!   npm 风格 scoped slug，`/` 在磁盘上即嵌套目录）
//!
//! 每个技能一个目录，`SKILL.md` 带 YAML frontmatter。缺目录/读失败返回空列表
//! （不阻塞启动）；旧平铺 `*.md` 忽略。渲染时由 oc-core::prompt 稳定排序。

use oc_core::config::SkillsConfig;
use oc_core::skill::{gated_skills, parse_skill, Skill};

use crate::paths;

/// 加载所有技能文档并做门控。失败静默返回空。
pub fn load(cfg: &SkillsConfig, host_os: &str) -> Vec<Skill> {
    let dir = match paths::oc_home() {
        Ok(h) => h.join("skills"),
        Err(_) => return Vec::new(),
    };
    load_from_dir(&dir, cfg, host_os)
}

/// 从指定 skills 目录加载（抽出以便测试注入临时目录）。
fn load_from_dir(dir: &std::path::Path, cfg: &SkillsConfig, host_os: &str) -> Vec<Skill> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue; // 旧平铺 *.md 直接忽略。
        }
        // 裸包：`skills/<slug>/SKILL.md`。
        if let Some(skill) = load_one(&path, "") {
            out.push(skill);
            continue;
        }
        // scoped 包：`skills/@<publisher>/<slug>/SKILL.md`。仅当该目录自身不是技能
        // （无 SKILL.md）时才下钻一层——避免把带 SKILL.md 的裸包误当成 scope 根。
        let scoped = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for sub in scoped.flatten() {
            let sub_path = sub.path();
            if !sub_path.is_dir() {
                continue;
            }
            let Some(publisher) = path.file_name().and_then(|s| s.to_str()) else {
                tracing::warn!(path = %path.display(), "skill scope 目录名非 UTF-8，跳过");
                continue;
            };
            let Some(leaf) = sub_path.file_name().and_then(|s| s.to_str()) else {
                tracing::warn!(path = %sub_path.display(), "skill 目录名非 UTF-8，跳过");
                continue;
            };
            let slug = format!("{publisher}/{leaf}");
            if let Some(skill) = load_one(&sub_path, &slug) {
                out.push(skill);
            }
        }
    }
    gated_skills(out, &cfg.allowlist, &cfg.denylist, host_os)
}

/// 读取一个技能目录（含 `SKILL.md`）为 `Skill`。`slug` 为相对 `skills/` 的完整
/// 可路由名（裸包 = leaf，scoped = `@publisher/leaf`）；无 `SKILL.md` 或解析失败返回
/// `None`。
fn load_one(dir: &std::path::Path, slug: &str) -> Option<Skill> {
    let raw = read_skill_md(dir)?;
    let leaf = dir.file_name().and_then(|s| s.to_str())?;
    let slug = if slug.is_empty() { leaf } else { slug };
    match parse_skill(slug, leaf, &raw) {
        Some(s) => Some(s),
        None => {
            tracing::warn!(path = %dir.display(), "skill frontmatter 解析失败，跳过");
            None
        }
    }
}

/// 读目录下的 `SKILL.md`（回退 `skill.md`）。
fn read_skill_md(dir: &std::path::Path) -> Option<String> {
    for name in ["SKILL.md", "skill.md"] {
        let p = dir.join(name);
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    // 每次调用返回唯一临时目录：Rust 测试默认并行，若三个用例共用 per-PID 的
    // 同一个目录、又各自 remove_dir_all，会互相删掉对方正在用的目录（flaky）。
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> std::path::PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("oc-skill-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_directories_and_parses_frontmatter() {
        let base = tmp();
        let skills = base.join("skills");
        fs::create_dir_all(skills.join("todoist")).unwrap();
        fs::write(skills.join("todoist").join("SKILL.md"), "---\nname: todoist\ndescription: Manage todos\n---\nbody").unwrap();
        // 旧平铺 *.md 应被忽略。
        fs::write(skills.join("legacy.md"), "old flat skill").unwrap();
        // 坏 frontmatter 目录跳过。
        fs::create_dir_all(skills.join("broken")).unwrap();
        fs::write(skills.join("broken").join("SKILL.md"), "---\nname: [unclosed\n---\nbody").unwrap();

        let cfg = SkillsConfig::default();
        let out = load_from_dir(&skills, &cfg, "linux");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "todoist");
        fs::remove_dir_all(&base).ok();
    }

    /// slug 必须等于目录名，而非 frontmatter `name`——门控与路由键的是 slug。
    #[test]
    fn slug_is_directory_name_not_frontmatter_name() {
        let base = tmp();
        // 用独立子目录名，避免与同进程其它用例共用 per-PID 临时根而互相踩踏。
        let skills = base.join("skills-slug");
        // 目录名 "pdf" 与 frontmatter name 故意不同。
        fs::create_dir_all(skills.join("pdf")).unwrap();
        fs::write(
            skills.join("pdf").join("SKILL.md"),
            "---\nname: PDF 生成器\ndescription: 生成 PDF\n---\nbody",
        )
        .unwrap();

        let cfg = SkillsConfig::default();
        let out = load_from_dir(&skills, &cfg, "linux");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].slug, "pdf", "slug 应是目录名");
        assert_eq!(out[0].name, "PDF 生成器", "name 仍是 frontmatter 展示名");
        fs::remove_dir_all(&base).ok();
    }

    /// scoped 包（`@publisher/slug`）下钻一层，slug 为完整 `@publisher/slug`，
    /// name 回退叶子目录名。
    #[test]
    fn scans_scoped_package_and_sets_full_slug() {
        let base = tmp();
        let skills = base.join("skills-scoped");
        fs::create_dir_all(skills.join("@pskoett/self-improving-agent")).unwrap();
        fs::write(
            skills.join("@pskoett/self-improving-agent/SKILL.md"),
            "---\ndescription: 自改进\n---\nbody",
        )
        .unwrap();

        let cfg = SkillsConfig::default();
        let out = load_from_dir(&skills, &cfg, "linux");
        assert_eq!(out.len(), 1, "scoped 技能应被加载: {out:?}");
        assert_eq!(out[0].slug, "@pskoett/self-improving-agent");
        assert_eq!(out[0].name, "self-improving-agent", "name 回退叶子目录名");
        fs::remove_dir_all(&base).ok();
    }

    /// 非 UTF-8 目录名跳过（带 warn），不得加载成 name/slug 为空的技能。
    /// 非 UTF-8 目录名应被 loader 跳过。
    ///
    /// 排除 macOS：APFS/HFS+ **强制文件名为 UTF-8**，`create_dir_all` 遇到 `0xFF`
    /// 直接返回 `EILSEQ`（errno 92, Illegal byte sequence），测试的前提在那里根本
    /// 无法构造——不是 loader 有问题，是这种目录名在 macOS 上不可能存在。
    /// Linux 的文件名是任意字节串，才有这条路径要防。
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn non_utf8_dir_name_is_skipped() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let base = tmp();
        let skills = base.join("skills-nonutf8");
        // 目录名含非法 UTF-8 字节 0xFF。
        let bad = skills.join(OsString::from_vec(vec![b'b', b'a', b'd', 0xFF]));
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "just a body").unwrap();

        let cfg = SkillsConfig::default();
        let out = load_from_dir(&skills, &cfg, "linux");
        assert!(out.is_empty(), "非 UTF-8 目录名应跳过: {out:?}");
        fs::remove_dir_all(&base).ok();
    }
}
