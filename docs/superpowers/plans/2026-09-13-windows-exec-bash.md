# Windows exec 执行器切换 cmd→Git Bash 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 Windows 上 `exec`/`process` 工具的执行 shell 从 `cmd.exe /C` 换成 Git Bash（`bash -c`），消除 cmd 语法代差导致的脚本不可靠。

**Architecture:** 新增 `oc-tools::shell::Shell` 枚举（`Bash{path}` / `Sh`），启动时探测一次 bash 路径并注入 `ExecTool`/`ProcessTool`，两个工具的子进程构造统一走 `Shell::command()`；`resolve()` 失败即 daemon 启动失败。非 Windows 分支保持 `sh -c` 不变。

**Tech Stack:** Rust 1.90（edition 2021）、tokio `process`、winreg（Windows 仅，注册表探测）。

## Global Constraints

- Windows **硬依赖** Git Bash，`resolve()` 找不到即启动失败，**不回退 cmd**。
- 非 Windows 保持 `sh -c` 逐字节不变，行为零变化。
- **不新增** `config.toml` 配置项；探测规则写死在代码里。
- GBK(cp936) 输出转码、`python3` 别名缺失**本次不修**（YAGNI，见 spec）。
- 错误消息、注释用中文，风格对齐现有代码（`exec.rs`/`process.rs`/`path_guard.rs`）。
- 版本：`cargo add` 解析依赖，`Cargo.lock` 由 cargo 更新。

---

### Task 1: 新增 `Shell` 模块（探测 + 命令构造）

**Files:**
- Create: `crates/oc-tools/src/shell.rs`
- Modify: `crates/oc-tools/src/lib.rs`（加 `pub mod shell;`）
- Modify: `crates/oc-tools/Cargo.toml`（加 winreg Windows-only 依赖）

**Interfaces:**
- Consumes: 无（自包含）。
- Produces: `pub enum Shell { Bash { path: PathBuf }, Sh }`；`impl Shell { pub fn command(&self, cmd: &str) -> tokio::process::Command; pub fn resolve() -> Result<Shell, String>; }`。`Shell` 派生 `Debug, Clone, PartialEq, Eq`（后续任务要 `Clone` 传给两个工具、测试要 `assert_eq!`）。

- [ ] **Step 1: 加 winreg 依赖**

```bash
cargo add winreg --target 'cfg(windows)' -p oc-tools
```

`crates/oc-tools/Cargo.toml` 会新增：

```toml
[target.'cfg(windows)'.dependencies]
winreg = "0.52"
```

- [ ] **Step 2: 写 `shell.rs` 与单测**

`crates/oc-tools/src/shell.rs` 全文：

```rust
//! 执行 shell 的选择与探测（Windows：Git Bash；非 Windows：sh）。
//!
//! 设计见 docs/superpowers/specs/2026-09-13-windows-exec-bash-design.md。
//! exec/process 工具据此构造子进程；探测只在启动时调一次，失败即启动失败。
//!
//! Windows 上选 Git Bash 而非 `cmd.exe`：cmd 与模型训练语料里占绝对主流的
//! POSIX/bash 语法存在「代差」（变量、命令替换、`&` 语义、取时间等都不同），
//! 模型生成的脚本大面积失效或静默做错事。

use std::path::PathBuf;

/// exec/process 用于执行命令的 shell。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// Git Bash：`bash -c`（Windows）。`path` 为绝对路径，或裸 `"bash"`（依赖 PATH）。
    Bash { path: PathBuf },
    /// `sh -c`（非 Windows，行为与历史一致）。
    Sh,
}

impl Shell {
    /// 构造执行 `cmd` 的子进程命令。只设程序与 `-c` 参数；`current_dir` 由调用方另设。
    pub fn command(&self, cmd: &str) -> tokio::process::Command {
        match self {
            Shell::Bash { path } => {
                let mut c = tokio::process::Command::new(path.as_path());
                c.arg("-c").arg(cmd);
                c
            }
            Shell::Sh => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(cmd);
                c
            }
        }
    }

    /// 启动时探测一次本机可用 shell。
    ///
    /// Windows：PATH → 常见安装路径 → 注册表，全失败返回中文错误。
    /// 非 Windows：恒为 `Shell::Sh`。
    pub fn resolve() -> Result<Shell, String> {
        #[cfg(windows)]
        {
            resolve_windows()
        }
        #[cfg(not(windows))]
        {
            Ok(Shell::Sh)
        }
    }
}

/// Windows 探测 bash：Tier 1 裸 `bash`（PATH）→ Tier 2 常见路径 → Tier 3 注册表。
#[cfg(windows)]
fn resolve_windows() -> Result<Shell, String> {
    // Tier 1：PATH 里的裸 `bash`（Git for Windows 装到 usr/bin 且上了 PATH 的情形）。
    if bash_probe("bash") {
        return Ok(Shell::Bash { path: PathBuf::from("bash") });
    }

    // Tier 2：常见安装路径。
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            candidates.push(PathBuf::from(pf).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs").join("Git").join("bin").join("bash.exe"));
    }
    for p in &candidates {
        if p.exists() {
            return Ok(Shell::Bash { path: p.clone() });
        }
    }

    // Tier 3：注册表 InstallPath（覆盖非默认安装目录）。
    if let Some(path) = registry_bash() {
        return Ok(Shell::Bash { path });
    }

    Err("未检测到 Git Bash（bash.exe），请安装 Git for Windows".to_string())
}

/// 探测 `prog` 是否可执行：spawn `--version` 并看退出码。
#[cfg(windows)]
fn bash_probe(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 读 `HKLM\SOFTWARE\GitForWindows` 的 `InstallPath`，拼 `\bin\bash.exe` 且需存在。
#[cfg(windows)]
fn registry_bash() -> Option<PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\GitForWindows").ok()?;
    let install: String = key.get_value("InstallPath").ok()?;
    let p = PathBuf::from(install).join("bin").join("bash.exe");
    p.exists().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参数序列校验：`Bash` 分支应产出 program=路径 + `["-c", cmd]`。
    #[test]
    fn bash_command_builds_dash_c() {
        let shell = Shell::Bash { path: PathBuf::from("bash") };
        let std = shell.command("echo hi").as_std();
        assert_eq!(std.get_program(), std::ffi::OsStr::new("bash"));
        let args: Vec<&std::ffi::OsStr> = std.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("echo hi")]);
    }

    /// `Sh` 分支：非 Windows 行为不变，仍是 `sh -c`。
    #[test]
    fn sh_command_builds_dash_c() {
        let shell = Shell::Sh;
        let std = shell.command("echo hi").as_std();
        assert_eq!(std.get_program(), std::ffi::OsStr::new("sh"));
        let args: Vec<&std::ffi::OsStr> = std.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("echo hi")]);
    }

    /// Windows 上 `resolve()` 必须找到 bash（本机与 CI windows-latest 都装了 Git）。
    #[cfg(windows)]
    #[test]
    fn resolve_finds_bash_on_windows() {
        let shell = Shell::resolve().expect("本机/CI 已装 Git for Windows，应能探测到 bash");
        assert!(matches!(shell, Shell::Bash { .. }));
    }

    /// 非 Windows 上恒为 `Sh`。
    #[cfg(not(windows))]
    #[test]
    fn resolve_is_sh_on_non_windows() {
        assert_eq!(Shell::resolve().unwrap(), Shell::Sh);
    }
}
```

- [ ] **Step 3: 注册模块**

`crates/oc-tools/src/lib.rs` 的模块列表（当前第 6-19 行之间）加一行，按字母序放在 `pub mod sanitize;` 之后、`pub mod sys;` 之前：

```rust
pub mod shell;
```

- [ ] **Step 4: 跑测试**

```bash
cargo test -p oc-tools shell
```

Expected: PASS（4 个用例：`bash_command_builds_dash_c`、`sh_command_builds_dash_c`、`resolve_finds_bash_on_windows` 或 `resolve_is_sh_on_non_windows` 按平台各 1 个）。

- [ ] **Step 5: 提交**

```bash
git add crates/oc-tools/src/shell.rs crates/oc-tools/src/lib.rs crates/oc-tools/Cargo.toml Cargo.lock
git commit -m "feat(tools): 新增 Shell 模块（Windows 探测 Git Bash，非 Windows 保持 sh）"
```

---

### Task 2: `exec.rs` 切到 `Shell`，修全部 exec 调用点

**Files:**
- Modify: `crates/oc-tools/src/exec.rs`
- Modify: `crates/oc-tools/tests/tools.rs`（5 处 `ExecTool::new`）
- Modify: `crates/oc-server/tests/cwd.rs`（1 处）
- Modify: `crates/oc-server/tests/toolloop.rs`（2 处）
- Modify: `crates/oc-server/tests/concurrent_submit.rs`（1 处）
- Modify: `crates/oc-server/tests/approval_cancel.rs`（1 处）
- Modify: `crates/oc-server/tests/tool_history_replay.rs`（1 处）
- Modify: `crates/oc-http/tests/native.rs`（1 处）

**Interfaces:**
- Consumes: `Shell`（Task 1）。
- Produces: `ExecTool::new(mode: ApprovalMode, timeout: Duration, approval_timeout: Duration, shell: Shell)`（4 参数）；`run_command(cmd, cx, shell)`（私有）。

> 注意：本任务改动 `ExecTool::new` 签名，会连带破坏 `crates/oc-cli/src/provider_setup.rs`（生产调用点）——它在 oc-cli，本任务**不编译** oc-cli，留到 Task 4 一起修。本任务验证只跑 `-p oc-tools` / `-p oc-server` / `-p oc-http`（这些 crate 都不依赖 oc-cli）。

- [ ] **Step 1: 改 `exec.rs`——加字段、改 `new`、删 `shell_command`**

`crates/oc-tools/src/exec.rs` 顶部 import 区（`use crate::Tool;` 之后）加：

```rust
use crate::shell::Shell;
```

结构体与构造（第 20-37 行）改为：

```rust
pub struct ExecTool {
    pub mode: ApprovalMode,
    pub timeout: Duration,
    /// 等待用户审批回执的上限；`ZERO` = 不设上限。超时按拒绝处理。
    pub approval_timeout: Duration,
    /// 执行 shell（启动时探测，见 `crate::shell`）。
    pub shell: Shell,
}
```

```rust
impl ExecTool {
    pub fn new(mode: ApprovalMode, timeout: Duration, approval_timeout: Duration, shell: Shell) -> Self {
        Self { mode, timeout, approval_timeout, shell }
    }
}
```

`invoke` 里（第 101 行）`let child_fut = run_command(&cmd, &cx);` 改为：

```rust
let child_fut = run_command(&cmd, &cx, &self.shell);
```

`run_command`（第 121-165 行）签名与首两行改为（其余 `spawn`/读管道/lossy 解码逻辑**不动**）：

```rust
async fn run_command(cmd: &str, cx: &ToolCtx, shell: &Shell) -> ToolResult<ToolOutput> {
    let mut command = shell.command(cmd);
    command.current_dir(&cx.cwd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
```

删除整个 `shell_command` 函数（第 167-183 行，含 `#[cfg(windows)]` / `#[cfg(not(windows))]` 两个分支与注释）。同时把第 116-120 行注释里「Windows `cmd` 的本地化输出常是 GBK」一句微调为「bash 下模型若调用原生 Windows 命令（`ipconfig` 等）仍可能输出 GBK」——只改措辞，`String::from_utf8_lossy` 保留（spec 决定不修 GBK）。

- [ ] **Step 2: 修 `oc-tools/tests/tools.rs`（5 处）**

顶部 import 加：

```rust
use oc_tools::shell::Shell;
```

5 处 `ExecTool::new(A, B, C)` 各补第 4 参 `, Shell::resolve().unwrap()`。以第 15 行为例：

```rust
let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap());
```

其余 4 处（原第 29、61-65、96、131 行）同样在末尾参数后追加 `, Shell::resolve().unwrap()`。

- [ ] **Step 3: 修 `oc-server/tests/cwd.rs`（1 处）**

import 加 `use oc_tools::shell::Shell;`；第 20 行：

```rust
reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
```

- [ ] **Step 4: 修 `oc-server/tests/toolloop.rs`（2 处）**

import 加 `use oc_tools::shell::Shell;`；第 23 行与第 103 行各补 `, Shell::resolve().unwrap()`：

```rust
reg.register(Arc::new(ExecTool::new(ApprovalMode::Allow, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
```

```rust
reg.register(StdArc::new(ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30), Shell::resolve().unwrap())));
```

- [ ] **Step 5: 修 `oc-server/tests/concurrent_submit.rs`（1 处）**

import 加 `use oc_tools::shell::Shell;`；第 29 行补 `, Shell::resolve().unwrap()`。

- [ ] **Step 6: 修 `oc-server/tests/approval_cancel.rs`（1 处）**

import 加 `use oc_tools::shell::Shell;`；第 45-49 行改为：

```rust
reg.register(Arc::new(ExecTool::new(
    ApprovalMode::Prompt,
    Duration::from_secs(30),
    Duration::from_secs(30),
    Shell::resolve().unwrap(),
)));
```

- [ ] **Step 7: 修 `oc-server/tests/tool_history_replay.rs`（1 处）**

函数内 `use` 块（第 183-184 行附近）加 `use oc_tools::shell::Shell;`；第 190-194 行改为：

```rust
reg.register(Arc::new(ExecTool::new(
    ApprovalMode::Allow,
    Duration::from_secs(10),
    Duration::from_secs(30),
    Shell::resolve().unwrap(),
)));
```

- [ ] **Step 8: 修 `oc-http/tests/native.rs`（1 处）**

函数内 `use` 块（第 409-410 行附近）加 `use oc_tools::shell::Shell;`；第 431-435 行改为：

```rust
reg.register(Arc::new(ExecTool::new(
    ApprovalMode::Allow,
    Duration::from_secs(10),
    Duration::from_secs(30),
    Shell::resolve().unwrap(),
)));
```

- [ ] **Step 9: 跑测试**

```bash
cargo test -p oc-tools
cargo test -p oc-server
cargo test -p oc-http
```

Expected: 全 PASS。`oc-tools` 的 `exec_safe_command_runs`（`echo hello`）在 Windows 上现在经 bash 执行，`echo` 在 bash 下同样输出 `hello`，语义不变。

- [ ] **Step 10: 提交**

```bash
git add crates/oc-tools/src/exec.rs crates/oc-tools/tests/tools.rs crates/oc-server/tests/cwd.rs crates/oc-server/tests/toolloop.rs crates/oc-server/tests/concurrent_submit.rs crates/oc-server/tests/approval_cancel.rs crates/oc-server/tests/tool_history_replay.rs crates/oc-http/tests/native.rs
git commit -m "feat(tools): exec 工具 Windows 上用 Git Bash 执行"
```

---

### Task 3: `process.rs` 切到 `Shell`，修 process 调用点

**Files:**
- Modify: `crates/oc-tools/src/process.rs`
- Modify: `crates/oc-server/tests/ledger.rs`（1 处 `ProcessTool::new`）

**Interfaces:**
- Consumes: `Shell`（Task 1）。
- Produces: `ProcessTool::new(handoff: mpsc::UnboundedSender<BackgroundHandoff>, shell: Shell)`；`spawn_background(cmd, cwd, out_tx, done_tx, shell)`（私有）。

> 本任务改 `ProcessTool::new` 签名，同样连带破坏 `provider_setup.rs`（oc-cli），留到 Task 4 修。本任务验证只跑 `-p oc-tools` 与 `-p oc-server`（不依赖 oc-cli）。

- [ ] **Step 1: 改 `process.rs`**

顶部 import 区（`use crate::Tool;` 之后）加：

```rust
use crate::shell::Shell;
```

结构体与构造（第 26-40 行）改为：

```rust
pub struct ProcessTool {
    handoff: mpsc::UnboundedSender<BackgroundHandoff>,
    /// 执行 shell（启动时探测，见 `crate::shell`）。
    shell: Shell,
}
```

```rust
impl ProcessTool {
    pub fn new(handoff: mpsc::UnboundedSender<BackgroundHandoff>, shell: Shell) -> Self {
        Self { handoff, shell }
    }
}
```

`invoke` 里（第 77 行）`spawn_background(&cmd, &cx.cwd, out_tx, done_tx)?;` 改为：

```rust
spawn_background(&cmd, &cx.cwd, out_tx, done_tx, &self.shell)?;
```

`spawn_background`（第 93-118 行）签名与首两行改为（其余 stdout 读取/退出码逻辑**不动**）：

```rust
fn spawn_background(
    cmd: &str,
    cwd: &std::path::Path,
    out_tx: mpsc::UnboundedSender<String>,
    done_tx: tokio::sync::oneshot::Sender<i32>,
    shell: &Shell,
) -> ToolResult<()> {
    let mut command = shell.command(cmd);
    command.current_dir(cwd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
```

删除整个 `shell_command` 函数（第 120-133 行）。

- [ ] **Step 2: 修 `oc-server/tests/ledger.rs`（1 处）**

import 加 `use oc_tools::shell::Shell;`；第 20 行改为：

```rust
let tool = ProcessTool::new(handoff_tx, Shell::resolve().unwrap());
```

- [ ] **Step 3: 跑测试**

```bash
cargo test -p oc-tools
cargo test -p oc-server --test ledger
```

Expected: 全 PASS。`ledger.rs` 的 `process_tool_registers_and_completes`（`echo bg-done`）在 Windows 上经 bash 执行，`echo` 语义不变。

- [ ] **Step 4: 提交**

```bash
git add crates/oc-tools/src/process.rs crates/oc-server/tests/ledger.rs
git commit -m "feat(tools): process 工具 Windows 上用 Git Bash 执行"
```

---

### Task 4: 生产接线 + 提示词 + 收尾

**Files:**
- Modify: `crates/oc-cli/src/provider_setup.rs`（`Shell::resolve()` + 注入两工具）
- Modify: `crates/oc-server/src/run.rs`（`PLATFORM_HINT`）
- Modify: `crates/oc-core/src/prompt.rs`（测试断言）
- Modify: `CHANGELOG.md`（Unreleased 加 Changed 条目）

**Interfaces:**
- Consumes: `Shell`（Task 1）、`ExecTool::new` 4 参（Task 2）、`ProcessTool::new` 2 参（Task 3）。
- Produces: 无新公共接口；`build_tools` 在启动时 `resolve()` 失败即返回 Err。

- [ ] **Step 1: 改 `provider_setup.rs`**

import 区（`use oc_tools::sys::SysTool;` 附近）加：

```rust
use oc_tools::shell::Shell;
```

`build_tools` 里，在 `let approval_timeout = ...;` 之后、`let mut roots = Vec::new();` 之前加探测：

```rust
    // Windows 上执行 shell 用 Git Bash；探测失败即启动失败（硬依赖，不回退 cmd）。
    let shell = Shell::resolve().map_err(|e| anyhow::anyhow!("{e}"))?;
```

`registry.register(Arc::new(ExecTool::new(...)))`（第 140 行）改为传 shell 的 clone：

```rust
    registry.register(Arc::new(ExecTool::new(mode, exec_timeout, approval_timeout, shell.clone())));
```

`registry.register(Arc::new(oc_tools::process::ProcessTool::new(handoff_tx)))`（第 148 行）改为：

```rust
    registry.register(Arc::new(oc_tools::process::ProcessTool::new(handoff_tx, shell)));
```

- [ ] **Step 2: 改 `run.rs` 的 `PLATFORM_HINT`**

`crates/oc-server/src/run.rs` 第 834-837 行，Windows 分支改为：

```rust
#[cfg(windows)]
const PLATFORM_HINT: &str = "操作系统：Windows。exec 工具通过 Git Bash（`bash -c`）执行命令，\
请使用 POSIX/bash 语法（例如取当前时间用 `date '+%F %T'`，不要用 `date /T & time /T`）。";
```

非 Windows 分支（第 838-840 行）不动。

- [ ] **Step 3: 改 `prompt.rs` 测试断言**

`crates/oc-core/src/prompt.rs` 第 228-245 行 `platform_in_stable_prefix`：

```rust
    #[test]
    fn platform_in_stable_prefix() {
        let with = PromptInputs {
            soul: "你是 oc。",
            platform: "操作系统：Windows。exec 工具通过 Git Bash（bash -c）执行命令。",
            model: "",
            provider: "",
            endpoint: None,
            bootstrap: &[],
            skills: &[],
            tools: &[],
            now: "NOW",
        };
        let r = render_system_prompt(&with);
        assert!(r.stable_prefix.contains("# 运行环境"));
        assert!(r.stable_prefix.contains("bash"));
        // 平台是稳定信息，不应进易变尾部。
        assert!(!r.volatile_suffix.contains("bash"));
    }
```

（只把 `cmd.exe` 断言换成 `bash`，测试语义「platform 进稳定前缀」不变。）

- [ ] **Step 4: 更新 CHANGELOG**

`CHANGELOG.md` 的 `## [Unreleased]` 下，在 `### Fixed` 之后加：

```markdown
### Changed
- Windows 上 `exec`/`process` 工具的执行 shell 从 `cmd.exe /C` 切换为 Git Bash（`bash -c`），模型可用 POSIX 语法，消除 cmd 语法代差导致的脚本不可靠。
```

- [ ] **Step 5: 全量跑测试**

```bash
cargo test --workspace
```

Expected: 全绿。这一步会首次编译 oc-cli（`provider_setup` 的新签名对齐），并覆盖 oc-core 的 prompt 测试改动。

- [ ] **Step 6: 提交**

```bash
git add crates/oc-cli/src/provider_setup.rs crates/oc-server/src/run.rs crates/oc-core/src/prompt.rs CHANGELOG.md
git commit -m "feat(server): 接线 Shell 探测 + 提示词切 bash；Windows exec 全链路切 Git Bash"
```

---

## 完成后的手动验证（可选，非测试门）

实现全部提交后，本机可跑一次真机冒烟确认模型侧体验：

```bash
cargo build --bin oc
# 起一个隔离 daemon（避免撞开发机已有的 daemon）
OC_SOCKET='\\.\pipe\oc-bashsmoke' cargo run --bin oc -- serve &
# 让模型跑一条 bash 语法命令，确认经 bash 执行（如 date '+%F %T'、ls -la、git status）
```

## 自审结论

- **Spec 覆盖**：spec 七节全部有对应任务——探测（T1）、exec 切换（T2）、process 切换（T3）、数据流/fail-fast（T4 provider_setup）、PLATFORM_HINT+prompt 测试（T4）、测试面（各任务 Step 内）、改动清单（各任务 Files）。
- **占位符**：无 TBD/TODO；每个代码步骤给全量可编译代码。
- **类型一致**：`Shell::resolve() -> Result<Shell, String>`、`Shell::command() -> tokio::process::Command`、`ExecTool::new(…, shell: Shell)`、`ProcessTool::new(…, shell: Shell)` 在 T1 定义、T2/T3/T4 使用处一致。
- **跨 crate 编译顺序**：T2 改 `ExecTool::new`、T3 改 `ProcessTool::new` 会暂时破坏 oc-cli 的 `provider_setup`，但 T2/T3 只验证 `-p oc-tools/-p oc-server/-p oc-http`（均不依赖 oc-cli），T4 一并修复并跑 `--workspace` 收口。
