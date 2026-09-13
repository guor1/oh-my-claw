//! 会话工作目录（cd）测试：验证每会话 cwd 独立、cd 后 exec/pwd 跟随。
//!
//! 直接驱动 ToolExecutor.run（绕过模型），断言 cwd 状态按 session 隔离。

use std::sync::Arc;
use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_proto::{Event, RunId, SessionId, ToolCallId};
use oc_server::sink::RunSink;
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::exec::ExecTool;
use oc_tools::shell::Shell;
use oc_tools::sys::SysTool;
use oc_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn executor(roots: Vec<std::path::PathBuf>) -> ToolExecutor {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
    reg.register(Arc::new(SysTool::new(roots, "UTC")));
    ToolExecutor::new(Arc::new(reg))
}

async fn call(ex: &ToolExecutor, name: &str, args: &str, session: &SessionId) -> String {
    let (tx, _rx) = broadcast::channel::<Event>(64);
    let sink = RunSink::Broadcast(tx);
    let (_status, content) = ex
        .run(
            name,
            args,
            session,
            &RunId::new("r"),
            &ToolCallId::new("c"),
            CancellationToken::new(),
            &sink,
        )
        .await;
    content
}

#[tokio::test]
async fn cd_changes_pwd_within_session() {
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    let sub = tmp.join(format!("oc-cwd-a-{}", std::process::id()));
    std::fs::create_dir_all(&sub).unwrap();

    let ex = executor(vec![tmp.clone()]);
    let s = SessionId::new("s1");

    // cd 到子目录，随后 pwd 应反映新目录。
    let cd = call(&ex, "sys", &format!(r#"{{"op":"cd","path":"{}"}}"#, sub.to_string_lossy().replace('\\', "\\\\")), &s).await;
    assert!(cd.contains("已切换"), "cd 应成功: {cd}");

    let pwd = call(&ex, "sys", r#"{"op":"pwd"}"#, &s).await;
    assert_eq!(
        std::path::Path::new(pwd.trim()).canonicalize().unwrap(),
        sub.canonicalize().unwrap(),
        "pwd 应反映 cd 后的目录"
    );

    std::fs::remove_dir_all(&sub).ok();
}

/// 未 `cd` 过的会话，pwd 应报注入的初始 cwd（生产上 = 配置里的工作区），
/// 而不是 daemon 的启动目录。历史上这里取 `std::env::current_dir()`，
/// 于是在 `/root` 下 `oc serve` 就把整个 `/root` 变成了 file 工具的允许根。
#[tokio::test]
async fn initial_cwd_comes_from_injection_not_process_dir() {
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    let ws = tmp.join(format!("oc-cwd-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();

    let ex = executor(vec![tmp.clone()]).with_initial_cwd(ws.clone());
    let pwd = call(&ex, "sys", r#"{"op":"pwd"}"#, &SessionId::new("fresh")).await;

    assert_eq!(
        std::path::Path::new(pwd.trim()).canonicalize().unwrap(),
        ws.canonicalize().unwrap(),
        "新会话应从注入的工作区起步"
    );
    let proc_dir = std::env::current_dir().unwrap();
    assert_ne!(
        std::path::Path::new(pwd.trim()),
        proc_dir.as_path(),
        "不应回落到进程启动目录"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn cwd_is_isolated_between_sessions() {
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    let sub = tmp.join(format!("oc-cwd-b-{}", std::process::id()));
    std::fs::create_dir_all(&sub).unwrap();

    let ex = executor(vec![tmp.clone()]);
    let s1 = SessionId::new("alpha");
    let s2 = SessionId::new("beta");

    // s1 cd 到子目录；s2 不动。
    let esc = sub.to_string_lossy().replace('\\', "\\\\");
    call(&ex, "sys", &format!(r#"{{"op":"cd","path":"{esc}"}}"#), &s1).await;

    let pwd1 = call(&ex, "sys", r#"{"op":"pwd"}"#, &s1).await;
    let pwd2 = call(&ex, "sys", r#"{"op":"pwd"}"#, &s2).await;

    assert_eq!(
        std::path::Path::new(pwd1.trim()).canonicalize().unwrap(),
        sub.canonicalize().unwrap(),
        "s1 应在子目录"
    );
    assert_ne!(
        std::path::Path::new(pwd2.trim()).canonicalize().unwrap(),
        sub.canonicalize().unwrap(),
        "s2 不应受 s1 的 cd 影响"
    );

    std::fs::remove_dir_all(&sub).ok();
}

#[tokio::test]
async fn exec_follows_cwd() {
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    let sub = tmp.join(format!("oc-cwd-c-{}", std::process::id()));
    std::fs::create_dir_all(&sub).unwrap();

    let ex = executor(vec![tmp.clone()]);
    let s = SessionId::new("s");

    let esc = sub.to_string_lossy().replace('\\', "\\\\");
    call(&ex, "sys", &format!(r#"{{"op":"cd","path":"{esc}"}}"#), &s).await;

    // 打印当前目录的命令。exec 统一经 bash 执行后 `pwd` 两平台通用
    // （旧 Windows 分支用 `cd`：cmd 下打印目录，bash 下却切到 $HOME 且无输出）。
    let cmd = "pwd";
    let out = call(&ex, "exec", &format!(r#"{{"command":"{cmd}"}}"#), &s).await;

    // 输出应包含子目录名（exec 子进程 current_dir 跟随会话 cwd）。
    let marker = sub.file_name().unwrap().to_string_lossy();
    assert!(out.contains(marker.as_ref()), "exec 工作目录应跟随 cd: {out}");

    std::fs::remove_dir_all(&sub).ok();
}
