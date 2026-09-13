# Windows exec 执行器切换：cmd.exe → Git Bash

日期：2026-09-13
状态：已确认设计，待写实现计划

## 背景

Windows 上 `exec`/`process` 工具统一走 `cmd.exe /C`（`exec.rs` 与 `process.rs` 各自的
`shell_command`）。`cmd` 是 80 年代批处理解释器，与模型训练语料里占绝对主流的
POSIX/bash 语法存在「代差」：变量、命令替换、循环、转义、`&` 语义（命令分隔符而非
后台符）、取时间等都不同，导致模型生成的脚本在 Windows 上大面积失效或静默做错事
（`docs/development/testing.md` 已记录 `&` 陷阱）。

系统提示词里虽有 `PLATFORM_HINT` 引导「用 cmd 语法」，但 in-context learning 压倒
系统提示词（见 `prompt.rs` 注释），一条提示词压不过模型见过的海量 bash 语料。

**决定**：Windows 上把执行器换成 Git Bash（`bash -c`），把模型熟悉的 POSIX 语法带回来。
本机已装 Git for Windows（`C:\Program Files\Git`，2.55.0），bash 在 PATH 中，`bash -c`
非登录模式的 PATH 已含 `git`/`node`/`python`/`cargo`。

## 目标与范围

- Windows：`exec`/`process` 从 `cmd.exe /C` 换成 `<bash> -c`。
- 非 Windows：保持 `sh -c` 不变，行为零变化。
- 硬依赖 bash：找不到即启动失败（用户决策，非回退 cmd）。
- **不修**（已确认 YAGNI）：GBK(cp936) 输出残留转码、`python3` 别名缺失（本机只有
  `python`）——切 bash 后 coreutils/脚本输出默认 UTF-8，已消 90% 乱码，其余低频场景
  等真反馈再补。

## 组件设计

### `crates/oc-tools/src/shell.rs`（新增）

```rust
pub enum Shell {
    Bash { path: PathBuf },   // Windows：探测到的 bash 绝对路径
    Sh,                       // 非 Windows：sh -c（不变）
}

impl Shell {
    /// 返回用于 Command::new 的程序名/路径 + 参数标志。
    /// Bash 分支：Command::new(path) + arg("-c") + arg(cmd)
    /// Sh   分支：Command::new("sh") + arg("-c") + arg(cmd)
    pub fn command(&self, cmd: &str) -> tokio::process::Command;

    /// Windows 探测：PATH → 常见路径 → 注册表；找不到 Err(明确中文消息)。
    /// 非 Windows：直接 Ok(Shell::Sh)。
    pub fn resolve() -> Result<Shell>;
}
```

`resolve()` 探测优先级（Windows）：

1. PATH 里的 `bash`（`which`/直接 `Command::new("bash")` 探测其可执行）。
2. 常见安装路径：
   - `%ProgramFiles%\Git\bin\bash.exe`
   - `%ProgramFiles(x86)%\Git\bin\bash.exe`
   - `%LocalAppData%\Programs\Git\bin\bash.exe`
3. 注册表 `HKLM\SOFTWARE\GitForWindows\InstallPath` → 追加 `\bin\bash.exe`。

全部失败 → `Err("未检测到 Git Bash（bash.exe），请安装 Git for Windows")`。

用 `std::process::Command`（同步）探测即可：`resolve()` 只在启动时调一次，不在热路径，
且阻塞探测一个本机路径的开销可忽略。路径存在性用 `Path::exists()` 判断。

## 数据流

```
provider_setup::build_tools()            ← 启动时一次
  └─ Shell::resolve()?                   ← 失败即启动失败（fail-fast）
       ├─ ExecTool::new(mode, timeout, approval_timeout, shell)
       └─ ProcessTool::new(handoff_tx, shell)
            └─ 子进程构造统一走 shell.command()
```

`exec.rs` / `process.rs` 里两份重复的 `shell_command` 删除，收编进 `Shell::command()`。

## 错误处理

- **启动时**：`resolve()` 失败 → `build_tools` 返回 Err → daemon 启动失败，stderr 打印
  中文原因。这是唯一一次失败点。
- **运行时**：bash 路径已缓存进工具，`exec` 里 `spawn()` 若仍失败（罕见，如启动后
  bash 被卸载）沿用现有 `?` 传播，走既有 `ToolError` 路径，不改。
- **非 Windows**：`Shell::Sh` 与原 `sh -c` 逐字节一致，零变化。

## 提示词与测试同步

- `crates/oc-server/src/run.rs` 的 `PLATFORM_HINT`（Windows 版）改为：

  > 操作系统：Windows。exec 工具通过 Git Bash（`bash -c`）执行命令，请使用
  > POSIX/bash 语法（例如取当前时间用 `date '+%F %T'`，不要用 `date /T & time /T`）。

- `crates/oc-core/src/prompt.rs` 测试（`platform_in_stable_prefix`，约 L231-244）的断言
  从 `cmd.exe` 改成 `bash`。该测试语义不变（platform 进稳定前缀）。

## 测试

- `shell.rs` 单测：
  - `resolve()` 在 Windows 上返回 `Shell::Bash{..}` 且路径 `exists()`（本机与 CI
    `windows-latest` runner 都装了 Git Bash）。
  - `Shell::Bash` 的 `command("echo hi")` 参数序列正确：program = 路径、args = `["-c", "echo hi"]`。
  - `Shell::Sh` 的 `command()` 仍产 `sh -c`。
- 构造处补参：`ExecTool::new` / `ProcessTool::new` 新增 `shell` 参数，约 8 处测试
  调用点（`oc-tools/tests/tools.rs`、`oc-server/tests/{cwd,ledger,toolloop,
  concurrent_submit,approval_cancel,tool_history_replay}.rs`、`oc-http/tests/native.rs`）
  补 `Shell::resolve().unwrap()` 或测试默认值。
- 现有 `echo` 跨平台测试（`tools.rs`、`ledger.rs` 已注明「cmd.exe 与 POSIX sh 写法
  一致」）继续绿，无需改。

## 改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/oc-tools/src/shell.rs` | 新增：`Shell` + `resolve()` + `command()` |
| `crates/oc-tools/src/exec.rs` | `ExecTool` 加 `shell` 字段；删 `shell_command`，改走 `Shell::command` |
| `crates/oc-tools/src/process.rs` | 同上 |
| `crates/oc-tools/src/lib.rs` | `pub mod shell;` |
| `crates/oc-cli/src/provider_setup.rs` | `build_tools` 里 `Shell::resolve()?`，注入两个工具 |
| `crates/oc-server/src/run.rs` | `PLATFORM_HINT` Windows 版改 bash |
| `crates/oc-core/src/prompt.rs` | 测试断言 `cmd.exe`→`bash` |
| 若干 `tests/*.rs` | 构造处补 `shell` 参数 |

## 决策记录

| 问题 | 决定 | 理由 |
|---|---|---|
| bash 找不到时 | A：硬依赖，启动报错 | 简单可预测，符合「个人助手装在自己机器」定位 |
| GBK 编码残留 | A：不修 | 切 bash 已消 90%，引依赖修低频场景不值 |
| bash 路径定位 | B：启动探测一次并缓存 | fail-fast，工具零运行时开销 |
| 探测放哪个 crate | 方案 1：`oc-tools::shell` + 显式注入 | 可测、显式、消除 `shell_command` 重复 |
