# Changelog

所有版本变更记录。格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

---

## [Unreleased]

### Added
- `scripts/test-openai-sdk.py`：真 `openai` Python SDK 指向本网关的 live 探针（TC-H12），验证非流式与 SSE 流式均能被标准客户端解析。

### Fixed
- cron day-of-month / day-of-week 字段都非 `*` 时由 AND 改为 OR（BUG-1）。「每月 1 号或每周一」这类表达式现按标准 cron 语义解释，而非等到两条件同时满足。

### Changed
- Windows 上 `exec`/`process` 工具的执行 shell 从 `cmd.exe /C` 切换为 Git Bash（`bash -c`），模型可用 POSIX 语法，消除 cmd 语法代差导致的脚本不可靠。

---

## [0.2.2] - 2026-09-10

### Added
- `memory.text` 字段加 FTS5 全文索引，10 万条查询从 243ms 降至 2.7ms（提升约 90 倍）。中文采用 trigram 预切词策略，查询结果与纯 LIKE 完全一致。
- `oc doctor` 加 `check_shape` 检查：在开发阶段直接改 DDL 不写迁移时，能检出「表结构过旧」并给出删库重建指引。

### Fixed
- macOS Intel CI runner 从已下线的 `macos-13` 换为 `macos-15-intel`，修复 job 永久排队问题。

---

## [0.2.1] - 2026-09-09

### Fixed
- CI release job 缺少 `contents: write` 权限，导致发布 GitHub Release 时 403。

---

## [0.2.0] - 2026-09-09

### Added
- **读写分离（P2-1）**：新增独立读连接池（`oc-store/src/reader.rs`），读路径走 `spawn_blocking` + `PRAGMA query_only=ON`，慢查询不再阻塞全体会话。
- **写线程自愈（P2-2）**：`HealthGuard` RAII 自动翻健康位，写线程 panic 后读路径仍可用，写操作返回明确的 `WriterDead` 错误而非进程僵死。
- **内存淘汰（P2-3）**：`IdempotencyCache` 加 TTL，冷键定期清扫；`SessionActor` 加空闲淘汰探针，`main` 会话永不淘汰。GC 挂在现有 60s 心跳上，不另起定时任务。
- **双平台 CI（P2-7）**：每次 push/PR 触发三平台（ubuntu/windows/macos）矩阵测试 + clippy；另有独立 smoke job。
- **oc-http OpenAI Responses API 兼容层**：绑定 127.0.0.1，支持 `x-openclaw-session-key` 会话路由，默认并发上限 32，超限返回 503。
- **测试自动化**：原 2658 行手册式真机验证改为 `cargo test --workspace` 自动化 e2e + 进程级冒烟脚本，同时修复了 SSE 双层 `data:` 前缀（流式内容对标准客户端不可解析）和 Windows 并发 `ERROR_PIPE_BUSY` 未重试两个缺陷。
- **配置热更定案（P2-5）**：不做热更，修改 `config.toml` 后重启 `oc serve` 即可，移除相关设计文档中的 ArcSwap/ReloadKind 描述。

### Changed
- `oc status` 新增后台任务数显示。
- `oc debug` 新增 `idem`（幂等表条目数）字段，方便观察内存泄漏。

### Fixed
- 卡死判据改为「多久没动静」（`last_delta_at`），不再按 run 总时长误杀长任务（如连续调工具生成大型文件的 157 秒 run）。
- 输出被 `max_tokens` 截断时改为工具参数拆小重做，不再续写截断的参数串。
- `oc status` 接上活跃 run / 排队数 / 后台任务数。
- agent 工作区固定为 `~/.oc/workspace`，不再继承 daemon 启动目录。
- `oc-tui` Linux 退格键输入字符 `h` 而非删字符。
- 输出上限默认取 `min(context_window, 8192)`，小 provider 不再因默认 4k 上限截断工具参数。
- `max_completion_tokens` 字段名默认值修正（原错用 `max_tokens`，部分 provider 不认识）。

---

## [0.1.1] - 2026-09-08

### Added
- **episodic 记忆产出（P1-6）**：会话结束前将高频访问记忆 flush 为 episodic 候选，供 dreaming 阶段晋升为 curated 核心记忆。
- **代码质量（P1-7）**：Clippy 警告清零，TODO 标记归档，全仓零新增告警。
- `file` 工具加 `edit`（精确字符串替换）和 `append` 操作，改一行不必重发整个文件。
- `oc serve --socket` 与 `OC_SOCKET` 环境变量，允许指定自定义 socket/管道路径以隔离测试端点。
- 并发连接上限（`oc http`），超限返回 503。
- Windows 打包脚本（`scripts/build-windows.ps1`）。

### Changed
- 审批等待超时后不再静默占用车道，改为明确返回拒绝并收敛 run。

### Fixed
- SSE 双层 `data:` 前缀：标准 OpenAI 客户端收不到任何事件。
- Windows 命名管道并发 `ERROR_PIPE_BUSY` 未重试，导致随机 500。
- 客户端断连期间 ask_user / 审批等待不收敛，卡到看门狗超时（约 34s）才释放车道。
- 长回复不截断（P0-1 RunSink 背压方案）：broadcast 队列满时 `Lagged` 导致事件永久丢失，改为 per-run 专属有界通道。
- abort 打断正在等待审批的 run（P0-2）。
- 审批 registry entry 泄漏（P0-3）：引入 RAII 守卫覆盖所有退出路径。
- `grep`/`glob` 工具在 async 上下文中同步阻塞 tokio worker（P0-4），改用 `spawn_blocking`。
- `ask_user` 工具全链路回答回喂 + registry 不泄漏（P1-1）。
- Standing intent 触发链接线（P1-2）：关键词命中时注入待办提醒，含 cooldown/budget/expiry 控制。
- 偏好 supersede（P1-3）：「记住我改用 Neovim」正确顶掉「我用 VS Code」而非并存矛盾。
- dreaming 重写 MEMORY.md（P1-4）：巩固模型轮 + 乐观并发写入，进程被杀不清空长期记忆。
- cron 工具化（P1-5）：模型可直接调用 `cron add/delay/list/rm`，时区按 IANA 解释，≤1h 延时另挂精确 timer。
- 模型注入身份信息，止住「你用什么模型」的编造。

---

## 更早历史

v0.1.1 之前的工作包括 M1–M6 六个主干里程碑（存储骨架 / 常驻进程与交互协议 / Agent 循环与 LLM / 工具与审批 / 完整记忆系统 / 主动性与 CLI），以及 P0 核心闭环稳定化。详细工程日志见 `docs/archive/plan/下一阶段计划.md`。
