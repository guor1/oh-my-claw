# 修复 exec 工具在 Windows 上的中文乱码

> 状态：实现中。

## 根因

真机（Windows + Git Bash + Python 3.14）暴露两个编码问题，都指向同一根源——**子进程输出用了系统代码页 GBK(cp936)，而我们的 exec 工具按 UTF-8 处理**：

1. **读方向乱码**：`python build_ppt.py` 的 stdout 按 GBK 编码，`run_command` 里 `String::from_utf8_lossy` 按 UTF-8 解 → 中文变 `ϲ������ɽ`。模型在乱码里 debug。
2. **写方向崩溃**：Python 在 Windows 上默认按控制台代码页（GBK）编码 stdout，遇到 GBK 里没有的字符（如 `▪` U+25AA）直接 `UnicodeEncodeError`，脚本以退出码 1 挂掉——这就是 seq 41 那轮「打印 PPT 大纲」失败的原因。

## 修法（`crates/oc-tools/src/exec.rs` 的 `run_command`）

两处、各管一个方向：

1. **写方向**：给子进程设 `PYTHONUTF8=1` + `PYTHONIOENCODING=utf-8`。让 Python 输出 UTF-8（PEP 540 的 `PYTHONUTF8` 覆盖 stdio 编码）。这两个 env 对非 Python 命令无副作用（只是被继承、被忽略）。
2. **读方向**：解码从「无条件 UTF-8 lossy」改成「UTF-8 严格解码，失败则按 GBK 解码」——覆盖 `ipconfig` 等原生 Windows 命令输出的 GBK 字节。新增 `encoding_rs` 依赖（纯 Rust，`GBK` 常量）。

## 影响面（按规范提前报备）

| 功能 | 怎么变 | 风险 |
|---|---|---|
| exec 输出解码 | 合法 UTF-8 输出**零变化**（严格解码成功即原样）。只有「非 UTF-8 字节」才走 GBK 分支 | 非 UTF-8 且非 GBK 的字节流（罕见）在 GBK 解码下仍是乱码，但不会比现在更糟（现在也是乱码 `�`） |
| 子进程环境 | 多两个 env 变量 | 对非 Python 命令无副作用 |
| 新增依赖 | `encoding_rs 0.8` | 纯 Rust、无 C 依赖、极成熟（Servo 出品，Firefox 在用） |

**不受影响**：非 Windows（`Shell::Sh` 路径）、exec 的审批/超时/取消/sanitize 逻辑。

## 测试

- 纯函数单测 `decode_output`：GBK 字节（`喜马拉雅山` = `cf b2 c2 ed c0 ad d1 c5 c9 bd`）→ 正确中文；UTF-8 字节 → 原样。
- 集成测试（`python` 可用时）：`python -c "print('中文')"` 输出含中文、无乱码。
