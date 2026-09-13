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

/// 探测 `prog` 是否为 Git Bash（而非 WSL shim）。
///
/// WSL 装过的机器上 `C:\Windows\System32\bash.exe` 是 WSL shim，`--version` 同样退出 0，
/// 但其版本三元组是 `linux-gnu`；Git for Windows 的 bash 是 `pc-cygwin`。
/// 必须读输出区分，否则 exec 会静默跑进 WSL 的 Linux rootfs，`C:\...` 路径与
/// Windows 原生命令全部失效。
#[cfg(windows)]
fn bash_probe(prog: &str) -> bool {
    use std::io::Read;
    let Ok(out) = std::process::Command::new(prog)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let mut s = String::new();
    if std::io::Cursor::new(out.stdout).read_to_string(&mut s).is_err() {
        return false;
    }
    // Git Bash 三元组是 pc-cygwin；WSL shim 是 linux-gnu。宁漏勿错：非 pc-cygwin 一律不收。
    s.contains("pc-cygwin") && !s.contains("linux-gnu")
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
        let command = shell.command("echo hi");
        let std = command.as_std();
        assert_eq!(std.get_program(), std::ffi::OsStr::new("bash"));
        let args: Vec<&std::ffi::OsStr> = std.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("echo hi")]);
    }

    /// `Sh` 分支：非 Windows 行为不变，仍是 `sh -c`。
    #[test]
    fn sh_command_builds_dash_c() {
        let shell = Shell::Sh;
        let command = shell.command("echo hi");
        let std = command.as_std();
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
