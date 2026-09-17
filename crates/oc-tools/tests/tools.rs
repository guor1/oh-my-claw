//! oc-tools 集成测试：exec 审批门 + file 读写 + 结果净化。

use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_tools::exec::ExecTool;
use oc_tools::file::FileTool;
use oc_tools::shell::Shell;
use oc_tools::types::{ApprovalGate, ApprovalReply, ToolCtx};
use oc_tools::Tool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn exec_safe_command_runs() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap());
    let cx = ToolCtx::detached(CancellationToken::new());
    // echo 在 Git Bash 与 POSIX sh 下写法一致，无需按平台分支。
    let echo = "echo hello";
    let out = tool
        .invoke(serde_json::json!({ "command": echo }), cx)
        .await
        .expect("exec ok");
    assert!(out.success);
    assert!(out.content.contains("hello"));
}

#[tokio::test]
async fn exec_dangerous_needs_approval_and_denied() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap());

    // 建审批门，自动拒绝。
    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            let _ = r.reply.send(ApprovalReply::Deny);
        }
    });

    let res = tool
        .invoke(serde_json::json!({ "command": "rm -rf /tmp/whatever" }), cx)
        .await;
    assert!(res.is_err(), "危险命令被拒应返回 Err");
}

/// 无人回执时，审批等待必须在 `approval_timeout` 后按拒绝收敛。
///
/// 这是本次修复的核心：cron / HTTP 网关没有 TUI 响应审批，若无上限，
/// 该轮会一直占着会话车道直到心跳的卡死诊断兜底（默认 360s）。
///
/// `start_paused` 用虚拟时钟：不真睡 120s，但仍走完整的 timeout 分支。
#[tokio::test(start_paused = true)]
async fn exec_approval_timeout_denies() {
    let tool = ExecTool::new(
        ApprovalMode::Prompt,
        Duration::from_secs(10),
        Duration::from_secs(120),
        Shell::resolve().unwrap(),
    );

    // 建审批门但**永不回执**——模拟无人值守。
    let (req_tx, _req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };

    let started = tokio::time::Instant::now();
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo rm -rf /tmp/whatever" }), cx)
        .await;

    assert!(res.is_err(), "无人回执应超时按拒绝处理，而非挂住");
    // 虚拟时钟：断言确实等满了超时，而不是被别的路径提前否掉。
    assert!(
        started.elapsed() >= Duration::from_secs(120),
        "应等满 approval_timeout，实际 {:?}",
        started.elapsed()
    );
}

/// `Duration::ZERO` = 不设上限（保留给交互式场景显式选择）。
/// 这里验证它不会退化成「立即拒绝」——那会让 TUI 下的审批完全不可用。
#[tokio::test(start_paused = true)]
async fn exec_approval_zero_timeout_waits_for_reply() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::ZERO, Shell::resolve().unwrap());

    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };

    // 拖很久才回执：ZERO 应一直等，最终拿到用户的 Deny（而非超时的 Deny）。
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let _ = r.reply.send(ApprovalReply::Deny);
        }
    });

    let started = tokio::time::Instant::now();
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo rm -rf /tmp/whatever" }), cx)
        .await;

    assert!(res.is_err(), "用户拒绝应返回 Err");
    assert!(
        started.elapsed() >= Duration::from_secs(600),
        "ZERO 应不设上限、一直等到回执，实际 {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn exec_dangerous_approved_runs() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap());
    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };
    // 审批放行；命令本身用无害的 sudo 替身——这里用 echo 触发 needs-approval 的替代。
    // 用 "sudo" 前缀触发审批，但实际执行会因无 sudo 而失败/或在 win 上不识别；
    // 为可移植，改用一个被判定为 NeedsApproval 且能在两平台跑的命令不易得，
    // 故只验证"放行后进入执行路径"（不强求成功）。
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            let _ = r.reply.send(ApprovalReply::Allow);
        }
    });
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo echo hi" }), cx)
        .await;
    // 放行后不应是 Denied；可能因环境无 sudo 而 Failed/非零退出，但不是 Err(Denied)。
    match res {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            assert!(!msg.contains("拒绝"), "放行后不应被拒: {msg}");
        }
    }
}

#[tokio::test]
async fn file_write_then_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let tool = FileTool::new(vec![root.clone()]);

    let target = root.join("note.txt");
    let cx = ToolCtx::detached(CancellationToken::new());
    let w = tool
        .invoke(
            serde_json::json!({ "op": "write", "path": target.to_str().unwrap(), "content": "记住这句" }),
            cx,
        )
        .await
        .expect("write ok");
    assert!(w.success);

    let cx = ToolCtx::detached(CancellationToken::new());
    let r = tool
        .invoke(serde_json::json!({ "op": "read", "path": target.to_str().unwrap() }), cx)
        .await
        .expect("read ok");
    assert!(r.content.contains("记住这句"));
}

#[tokio::test]
async fn file_outside_root_denied() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let tool = FileTool::new(vec![root]);

    // 尝试读一个明显在根外的路径。
    let outside = if cfg!(windows) { "C:\\Windows\\system.ini" } else { "/etc/hosts" };
    let cx = ToolCtx::detached(CancellationToken::new());
    let res = tool
        .invoke(serde_json::json!({ "op": "read", "path": outside }), cx)
        .await;
    assert!(res.is_err(), "根外路径应被拒");
}

/// 回归：allowed_roots 传入**未 canonicalize** 的裸路径（真实 wiring 就是裸
/// current_dir），而 candidate 经 canonicalize 带 Windows `\\?\` 前缀，
/// 两者必须归一后比较，否则合法路径被误拒（截图里的 bug）。
#[tokio::test]
async fn file_raw_root_allows_relative_path_via_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // 故意用裸 path（不 canonicalize），模拟 provider_setup 传入的 current_dir。
    let raw_root = dir.path().to_path_buf();
    let tool = FileTool::new(vec![raw_root.clone()]);

    // 在根内建文件。
    std::fs::write(raw_root.join("hello.txt"), "内容").unwrap();

    // cwd = 裸根；用相对路径读。
    let mut cx = ToolCtx::detached(CancellationToken::new());
    cx.cwd = raw_root.clone();
    let r = tool
        .invoke(serde_json::json!({ "op": "read", "path": "hello.txt" }), cx)
        .await
        .expect("裸 root + 相对路径应放行");
    assert!(r.content.contains("内容"));
}

/// 核心机制回归：`~/.oc/skills/<name>/SKILL.md` 必须以 `~` 开头的绝对路径
/// 解析到主目录，而不是被拼成 `<cwd>/~/.oc/...`。主目录不一定是 allowed_roots
/// 的父目录，所以这里直接放行读 HOME 下的一个真实文件（allowed_roots 含 HOME）。
#[tokio::test]
async fn file_read_expands_tilde_under_home_root() {
    let home = oc_tools::path_guard::home_dir().expect("测试环境应能定位主目录");

    // 在 HOME 下建一个技能式目录 + SKILL.md。
    let skills = home.join(".oc-test-skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("SKILL.md"), "TILDE_BODY_XYZ").unwrap();

    // allowed_roots 含 HOME：展开后的 `~/.oc-test-skills/SKILL.md` 落在根内。
    let tool = FileTool::new(vec![home.clone()]);
    // cwd 故意设到别处，证明 `~` 不是相对 cwd 解析的。
    let mut cx = ToolCtx::detached(CancellationToken::new());
    cx.cwd = std::env::temp_dir();

    let r = tool
        .invoke(
            serde_json::json!({ "op": "read", "path": "~/.oc-test-skills/SKILL.md" }),
            cx,
        )
        .await
        .expect("~/... 路径应解析到主目录并放行");
    assert!(r.content.contains("TILDE_BODY_XYZ"), "读到的不是技能正文: {}", r.content);

    std::fs::remove_dir_all(&skills).ok();
}

// ─────────────────────────────────────────────────────────────────────────
// op=edit / op=append
//
// 动机：file 此前只有 write（整文件覆写）。模型改一行也得重发整个文件，而真机
// 量到的吐字速率约 48 字符/秒——覆写一个 20KB 脚本要 7 分钟，edit 改一处只要
// 几秒。参数长度直接决定耗时，这不是常数级优化。
// ─────────────────────────────────────────────────────────────────────────

/// 建一个临时根 + FileTool，返回 (tool, root)。
fn edit_fixture() -> (FileTool, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let tool = FileTool::new(vec![root.clone()]);
    // TempDir 要一起返回：drop 了目录就没了。
    (tool, root, dir)
}

fn cx() -> ToolCtx {
    ToolCtx::detached(CancellationToken::new())
}

#[tokio::test]
async fn file_edit_replaces_unique_target() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    std::fs::write(&p, "a = 1\nb = 2\nc = 3\n").unwrap();

    let out = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                "old_string": "b = 2", "new_string": "b = 20"
            }),
            cx(),
        )
        .await
        .expect("edit ok");
    assert!(out.success, "唯一匹配应成功: {}", out.content);
    // 只动目标，其余不变。
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 1\nb = 20\nc = 3\n");
}

/// 歧义替换必须报错：默默改错位置比报错更糟，模型不会知道自己改坏了。
#[tokio::test]
async fn file_edit_ambiguous_match_reports_count() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    std::fs::write(&p, "x = 1\ny = 1\n").unwrap();

    let out = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                "old_string": "= 1", "new_string": "= 9"
            }),
            cx(),
        )
        .await
        .expect("不该是硬错误——模型要能看到失败原因并自纠");
    assert!(!out.success, "出现两次应失败");
    assert!(out.content.contains('2'), "应告知出现次数: {}", out.content);
    // 文件不得被改动。
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "x = 1\ny = 1\n");
}

#[tokio::test]
async fn file_edit_replace_all_replaces_every_occurrence() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    std::fs::write(&p, "x = 1\ny = 1\n").unwrap();

    let out = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                "old_string": "= 1", "new_string": "= 9", "replace_all": true
            }),
            cx(),
        )
        .await
        .expect("edit ok");
    assert!(out.success, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "x = 9\ny = 9\n");
    assert!(out.content.contains('2'), "应回报替换处数: {}", out.content);
}

/// 诊断阶梯第 2 条：空白对不上是模型最常见的失败，必须明说，不能只回「未找到」。
#[tokio::test]
async fn file_edit_whitespace_mismatch_says_so() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    // 文件用 4 空格缩进。
    std::fs::write(&p, "def f():\n    return 1\n").unwrap();

    let out = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                // 模型凭记忆写成 2 空格缩进。整段逐字符对不上，但折叠空白后一致。
                "old_string": "def f():\n  return 1", "new_string": "def f():\n  return 2"
            }),
            cx(),
        )
        .await
        .expect("软失败");
    assert!(!out.success);
    assert!(
        out.content.contains("空白") || out.content.contains("缩进"),
        "应指出是空白/缩进问题，而非让模型瞎猜: {}",
        out.content
    );
}

/// 诊断阶梯第 1 条（Windows 回归）：文件是 CRLF、模型给的是 LF，应当仍能替换，
/// 且不得顺带改掉其余行的换行风格。
#[tokio::test]
async fn file_edit_matches_across_crlf() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    std::fs::write(&p, "a = 1\r\nb = 2\r\nc = 3\r\n").unwrap();

    let out = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                // 跨行的 old_string 用 LF。
                "old_string": "a = 1\nb = 2", "new_string": "a = 1\nb = 22"
            }),
            cx(),
        )
        .await
        .expect("edit ok");
    assert!(out.success, "CRLF 文件配 LF old_string 应能匹配: {}", out.content);
    let after = std::fs::read_to_string(&p).unwrap();
    assert!(after.contains("b = 22"), "应完成替换: {after:?}");
    assert!(
        !after.contains("\n\r") && after.matches("\r\n").count() == 3,
        "其余行的 CRLF 必须保持，不能被改成 LF: {after:?}"
    );
}

#[tokio::test]
async fn file_edit_rejects_empty_and_noop() {
    let (tool, root, _d) = edit_fixture();
    let p = root.join("s.py");
    std::fs::write(&p, "a = 1\n").unwrap();

    // 空 old_string：匹配任意位置，语义无意义。
    let e = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                "old_string": "", "new_string": "x"
            }),
            cx(),
        )
        .await;
    assert!(e.is_err(), "空 old_string 应拒");

    // old == new：无操作。
    let e = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": p.to_str().unwrap(),
                "old_string": "a = 1", "new_string": "a = 1"
            }),
            cx(),
        )
        .await;
    assert!(e.is_err(), "old == new 应拒");
}

#[tokio::test]
async fn file_append_extends_and_creates() {
    let (tool, root, _d) = edit_fixture();

    // 已有文件：接在末尾，原内容不动。
    let p = root.join("n.md");
    std::fs::write(&p, "第一段\n").unwrap();
    let out = tool
        .invoke(
            serde_json::json!({ "op": "append", "path": p.to_str().unwrap(), "content": "第二段\n" }),
            cx(),
        )
        .await
        .expect("append ok");
    assert!(out.success, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "第一段\n第二段\n");

    // 不存在的文件：创建。父目录须已存在——path_guard 对不存在的父目录一律拒
    // （write 也是同样约束，见 path_guard::resolve_in_roots）。
    let q = root.join("new.md");
    let out = tool
        .invoke(
            serde_json::json!({ "op": "append", "path": q.to_str().unwrap(), "content": "起头\n" }),
            cx(),
        )
        .await
        .expect("append 应能创建");
    assert!(out.success, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&q).unwrap(), "起头\n");
}

/// 新 op 同样受 allowed_roots 约束——别开出一条绕过路径守卫的新门。
#[tokio::test]
async fn file_edit_append_respect_roots() {
    let (tool, _root, _d) = edit_fixture();
    let outside = if cfg!(windows) { "C:\\Windows\\system.ini" } else { "/etc/hosts" };

    let e = tool
        .invoke(
            serde_json::json!({
                "op": "edit", "path": outside,
                "old_string": "a", "new_string": "b"
            }),
            cx(),
        )
        .await;
    assert!(e.is_err(), "edit 根外路径应被拒");

    let e = tool
        .invoke(
            serde_json::json!({ "op": "append", "path": outside, "content": "x" }),
            cx(),
        )
        .await;
    assert!(e.is_err(), "append 根外路径应被拒");
}

/// 真机乱码回归：Windows Python 默认按 GBK 输出中文，旧实现按 UTF-8 lossy 解
/// 会得到乱码、模型蒙眼 debug。设 PYTHONUTF8/PYTHONIOENCODING 后输出 UTF-8，
/// decode_output 应原样还原中文。
#[cfg(windows)]
#[tokio::test]
async fn exec_python_chinese_roundtrip_clean() {
    use oc_tools::shell::Shell;

    let tool = ExecTool::new(
        ApprovalMode::Allow,
        Duration::from_secs(30),
        Duration::from_secs(30),
        Shell::resolve().unwrap(),
    );
    let cx = ToolCtx::detached(CancellationToken::new());
    let out = tool
        .invoke(serde_json::json!({ "command": "python -c \"print('中文测试')\"" }), cx)
        .await
        .expect("exec ok");
    assert!(out.success, "python 应成功: {}", out.content);
    assert!(
        out.content.contains("中文测试"),
        "中文输出不得乱码: {}",
        out.content
    );
}
