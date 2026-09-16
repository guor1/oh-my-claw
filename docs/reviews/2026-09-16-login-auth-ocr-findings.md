# 登录鉴权改造 — OCR 代码评审结果

## 评审元信息

| 项 | 值 |
|---|---|
| 时间 | 2026-09-16 |
| 命令 | `ocr review --audience agent`（workspace 模式：staged + unstaged + untracked） |
| 范围 | 16 个文件 |
| 结果 | 11 条 finding |
| 耗时 | 23m35s |
| Token | ~4,165,859（input ~4,017,662 / output ~148,197；cache read ~1,807,616） |
| Session | `11f7992d-3293-41ff-91e5-7f02531d84cb` |

被评审的改动即「token 泄露修复」两步工作：

- **第 1 步**（加固）：常数时间比较、空 token 归一化、`?token=` URL 解码并收紧到 `/api/v1/events`、移除 index.html 的 token 注入、`Cache-Control`
- **第 2 步**（登录握手）：`POST /auth/login` 换 HttpOnly cookie、中间件三选一鉴权、`LoginGate.vue`、`checkAuthed()` 探测

## 结论速览

| # | 位置 | 类型·级别 | 一句话 | 我的评估 | 状态 |
|---|---|---|---|---|---|
| 1 | `ui/src/lib/api.js:60-63` | bug·medium | `checkAuthed()` 无 try/catch，非 401 一律 fail-open | **高** 真实可达 | 待处理 |
| 2 | `ui/src/lib/api.js:246-248` | bug·medium | cookie 过期后 EventSource 静默无限重连，不弹登录门 | **高** 真实可达 | 待处理 |
| 3 | `ui/src/App.vue:25-29` | bug·medium | `loadHistory()` fire-and-forget，AuthError 变未处理拒绝 | **高** 真实可达 | 待处理 |
| 4 | `src/auth.rs:161` | security·medium | 会话 cookie 缺 `Secure`，明文 HTTP 下可被嗅探重放 | 中（修复需 TLS 探测） | 待处理 |
| 5 | `src/auth.rs:158` | bug·medium | `login_sessions` 仅懒清理，长期运行无界增长 | 中 | 待处理 |
| 6 | `src/auth.rs:53` | security·medium | `AppState::new` 仍接受 `Some("")`，库边界不变量不成立 | 中（纵深防御，CLI 路径已安全） | 待处理 |
| 7 | `ui/src/App.vue:33-35` | maint·low | 鉴权失效时未 teardown 旧 EventSource | 低（与 #2 同源） | 待处理 |
| 8 | `src/auth.rs:113` | maint·low | `cookie_session` 是 `async fn` 但无 `.await` | 低（clippy lint） | 待处理 |
| 9 | `src/auth.rs:45` | doc·low | 文档链接 `[LOGIN_ROUTE]` 指向已删除的常量 | 低（rustdoc 断链） | 待处理 |
| 10 | `LoginGate.vue:15-17` | bug·low | `submit()` 无重入保护，可并发发起多次登录 | 低 | 待处理 |
| 11 | `src/auth.rs:80` | bug·low | `form_urlencoded` 把 `+` 解成空格，注释表述不准 | 低（注释问题） | 待处理 |

> 路径省略了公共前缀 `crates/oc-http/`。

## 高优先级（真实可达，建议优先修）

### 1. `checkAuthed()` 可致白屏，且非 401 一律 fail-open

`crates/oc-http/ui/src/lib/api.js:60-63` — bug · medium

`App.vue` 的 `bootstrap()` 直接 `await` 它、没有 try/catch。两个后果：

- **白屏**：fetch 被拒（服务器不可达、网络/CORS 错误）时 promise 抛出，`needLogin` 停在 `null`，模板两个分支都不渲染 → 整页空白。
- **fail-open**：任何非 401 状态（例如 daemon 连接失败时 `sessions::status` 返回的 500）都被当成"已认证"，于是展示主界面并触发一连串必然失败的请求，而不是把真正的错误暴露出来。

建议：只把明确的 200 / 401 当作结论，其余视为不确定；并决定传输失败时展示什么。

```js
  export async function checkAuthed() {
+   try {
-   const resp = await fetch('/api/v1/status')
+     const resp = await fetch('/api/v1/status')
-   return resp.status !== 401
+     if (resp.status === 200) return true
+     if (resp.status === 401) return false
+     // 非 200/401（例如 daemon 挂了）不构成结论，不要 fail open
+     throw new Error(`status probe failed: ${resp.status}`)
+   } catch (e) {
+     throw e
+   }
  }
```

### 2. cookie 过期后 ambient 流静默无限重连

`crates/oc-http/ui/src/lib/api.js:246-248` — bug · medium

移除 `?token=` 后 ambient 流完全依赖 cookie，而 `EventSource` **无法读取 401 的响应体/状态码**。当 `oc_session` 在页面打开期间过期：

- `/api/v1/events` 返回 401，只有 `onerror` 触发；
- 而 `state.js` 的 `startAmbientStream` 刻意忽略 `onError`（依赖自动重连）；
- 结果是一个**静默的、持续发未鉴权请求的重连循环**，直到某个别的 `apiFetch` 恰好收到 401 才会弹出 LoginGate。

建议：在流的错误路径里探测鉴权（例如 `onerror` 时调 `checkAuthed()`，若未认证则 dispatch `oc:auth-required`），让过期被主动暴露。

### 3. `loadHistory()` 的 AuthError 变成未处理拒绝

`crates/oc-http/ui/src/App.vue:25-29` — bug · medium

`loadSessions()` 在 `state.js` 内部有 try/catch 兜住错误，但 `loadHistory()` 没有任何错误处理。这里 fire-and-forget 调用它，意味着一旦 reject（例如 `oc_session` 中途过期抛出的 `AuthError`）就变成**未处理的 promise 拒绝**，永远到不了 `oc:auth-required` 监听器，登录门也就不会出现。

建议：捕获并把 `AuthError` 路由到同一个事件出口（或加 `.catch` 直接 dispatch `oc:auth-required`）。

## 中优先级

### 4. 会话 cookie 缺少 `Secure` 属性

`crates/oc-http/src/auth.rs:161` — security · medium

会话 cookie 在权限上**等价于**配置的 token，却没有 `Secure` 属性。由于配了 token 时支持非 loopback 明文 HTTP 绑定，`oc_session` 的值可以在网络上被嗅探，并在整个 12 小时生命周期内重放。

建议：在请求经 HTTPS 到达时加上 `Secure`；或者明确文档化/限制这一暴露面（例如要求非 loopback 绑定必须走 TLS）。

> **实施注意**：不能无条件加 `Secure` —— 明文 HTTP 下浏览器根本不会回传该 cookie，登录会直接失效。需要先有判断请求是否为 TLS 的能力。

### 5. `login_sessions` 只懒清理，长期运行无界增长

`crates/oc-http/src/auth.rs:158` — bug · medium

会话条目仅在 `cookie_session` 内部被惰性淘汰，**且只在某个请求恰好带着那个已过期/未知的 `oc_session` cookie 时**才会清。在 12 小时 TTL 内反复成功登录会不断插入新条目，而没有任何其他清理路径 —— 长期运行的 daemon 里 `login_sessions` 会无界增长。

建议：加周期性清扫（或定时检查淘汰过期项），或者加容量上限 + 淘汰策略。

### 6. `AppState::new` 仍原样接受 `Some("")`

`crates/oc-http/src/auth.rs:53` — security · medium

空/空白 token 只在 CLI 入口（`main.rs`）被归一化为 `None`，`check_bind_requires_token` 里也按"无 token"处理，但 `AppState::new` / `create_app` 仍原样接受 `Some("")`。此时中间件会把 `Some("")` 当作已配置的凭据：空的 `Authorization: Bearer ` 头与空 token `ct_eq` 相等因而被接受 —— 直接调用 `create_app`（或任何跳过 CLI 归一化的路径）的调用方会跑在**实质无鉴权**状态，正是本次改动想消除的失效模式。在 loopback 上 `Some("")` 还会让 Web UI 卡在一个"空头即可满足"的登录门后面，与 `loopback_empty_token_is_fine` 测试的语义相矛盾。

建议：在 `AppState::new` 边界归一化一次，让不变量对每个调用方（含 `login`）都成立。

```rust
- let Some(expected) = &state.token else {
+ let Some(expected) = state.token.as_deref().filter(|t| !t.trim().is_empty()) else {
```

> **我的评估**：属于**纵深防御**而非 bug 修复。生产路径（`main.rs:222`）已归一化，且该处不能省——`check_bind_requires_token` 在构造 `AppState` 之前就要用归一化后的值。缺口只在"把 oc-http 当库用且传 `Some("")`"时成立，当前无此调用方。

## 低优先级

### 7. 鉴权失效时未 teardown 旧 EventSource

`crates/oc-http/ui/src/App.vue:33-35` — maintainability · low

`onAuthRequired()` 翻转了登录门，但没有停掉正在跑的 ambient 流或任何在途请求。`state.js` 里的 `closeAmbient` 句柄是模块私有的、没有导出停止函数，于是鉴权过期后旧 `EventSource` 会在 LoginGate 背后持续重连（反复向 `/api/v1/events` 发未鉴权请求），直到重新登录后 `start()` 再次执行 —— 而 `startAmbientStream()` 是靠副作用关掉上一个流的，而非显式的 teardown 步骤。

建议：导出 `stopAmbientStream()` 并在此调用，既能止住无意义的重连流量，也让鉴权过期这个状态转换变得显式。

### 8. `cookie_session` 是 `async fn` 却无 `.await`

`crates/oc-http/src/auth.rs:113` — maintainability · low

函数体全是同步逻辑。这会触发 `clippy::unused_async`，并且每次请求都把一个普通函数无谓地包进 async 调用。

建议：改成普通 `fn`，调用点去掉 `.await`：`cookie_session(req.headers(), &state)`。

### 9. 文档链接指向已删除的常量

`crates/oc-http/src/auth.rs:45` — documentation · low

文档注释里链接了 `` [`LOGIN_ROUTE`] ``，但该常量（或同名条目）已不存在于本 crate。这是一个无法解析的 rustdoc intra-doc 链接，在 `deny(rustdoc::broken_intra_doc_links)` 下会失败。

建议：要么把常量定义回来并用于下面的路径判断，要么改写注释直接引用字面量路由。

> **背景**：第 2 步实施过程中我曾加过 `LOGIN_ROUTE` 常量并把它错误地放进中间件豁免条件，导致登录接口自己被鉴权拦截返回 401；修复时删掉了常量，但漏改了这条文档注释。

### 10. `submit()` 无重入保护

`crates/oc-http/ui/src/components/LoginGate.vue:15-17` — bug · low

`busy` 只禁用了提交按钮，在输入框里按 Enter 仍会提交表单，而 `submit()` 没有重入保护。握手较慢时用户可以并发打出多个 `/auth/login` 请求（每个都会下发一个新的会话 cookie）。

建议：在 `submit()` 开头加一句 `if (busy.value) return`。

### 11. `+` 被解码成空格，注释表述不准

`crates/oc-http/src/auth.rs:80` — bug · low

`url::form_urlencoded::parse` 会把 `+` 解码为空格，相对之前的裸 split 是**行为变更**：含字面 `+`（标准 base64 字母表）的 token 以前用 `?token=...` 能匹配，现在必须百分号编码成 `%2B`，否则比较会静默失败。上方注释声称 "base64/`+`/`/`-bearing tokens work"，这只对会做 URL 编码的客户端成立。

建议：更新注释以说明编码要求；或者若必须让未编码的 token 继续可用，改用保留裸 `+` 的解码方式。

## 附：一次工具执行错误

```
[ocr] ✘ file_read failed: file "crates/oc-http/src/native.rs" not found
```

OCR 尝试读取 `crates/oc-http/src/native.rs`，但该模块实际是目录形式 `crates/oc-http/src/native/mod.rs`。属于工具侧路径推断问题，不影响其余 finding 的有效性。

