# HTTP 接口参考（OpenAI Responses API 兼容）

`oc http` 是独立进程，在 daemon 前面加一层 HTTP 适配器。HTTP 侧崩溃不影响核心会话。

## 启动

```bash
oc http [--port 8080] [--socket <路径>] [--max-conns 32]
```

绑定 `127.0.0.1`。

| 参数 | 默认 | 说明 |
|---|---|---|
| `--port` | `8080` | HTTP 监听端口 |
| `--socket` | 平台默认 | oc-server 的 socket / 管道路径 |
| `--max-conns` | `32` | 到 daemon 的并发连接硬上限，超出后新请求等 10s 再返回 503 |

---

## POST /v1/responses

请求体映射到内部 NDJSON 协议：

| OpenAI 字段 | 处理 |
|---|---|
| `input` | 用户消息（支持字符串或 content 数组，含 base64 文件输入）|
| `instructions` | 注入为系统提示词 |
| `stream` | `true` 走 SSE，`false` 一次性返回 |
| `user` | 会话路由键 |
| `previous_response_id` | 会话延续 |
| 其他不支持的参数 | **显式返回 400**，不静默降级 |

「不支持就报 400 而不是静默忽略」是刻意的——静默降级会让客户端以为参数生效了。

---

## 会话路由

三种方式指定会话，优先级从高到低：

1. `x-openclaw-session-key` 请求头
2. 请求体 `user` 字段
3. `previous_response_id`（延续该响应所属会话）

都不给则落到 `main` 会话。

**自动化脚本建议显式指定 session key**，别挤 `main`——那是你自己在 TUI 里用的会话。

同一会话本就串行执行（车道模型），调大 `--max-conns` 只对多 session key 并行有意义。

---

## SSE 流式

`stream: true` 时返回 `text/event-stream`，事件类型对齐 OpenAI Responses API 的 `response.*` 系列。

---

## 安全边界

绑定 `127.0.0.1`，**没有认证**。

这是单用户本地 daemon 的设计前提。不要暴露到公网或放在反向代理后面——任何能访问该端口的进程都能以你的身份对话、跑工具、读写你的记忆。

---

## 第三方 / app 客户端接入 thinking

本仓库之外的客户端走 **native HTTP API**（`/api/v1/*`），不是 socket——socket 只在本机。
`oc http --bind <非 loopback> --token <t>` 对外提供；绑非 loopback 时 token 强制
（`auth.rs::check_bind_requires_token`，建 socket 前校验）。

### 线上格式

`Event` 是 `#[serde(tag = "event", rename_all = "snake_case")]`，外层 `Frame` 是
`tag = "kind"`：

    # NDJSON（socket，本机客户端）
    {"kind":"event","event":"reasoning","session":"main","run_id":"...","delta":"我需要先确认"}

    # HTTP SSE（POST /api/v1/chat/send 的响应体）
    event: reasoning
    data: {"event":"reasoning","session":"main","run_id":"...","delta":"我需要先确认"}

### 接入步骤

1. `POST /api/v1/chat/send`，带 `Authorization: Bearer <token>`、
   `Accept: text/event-stream`，body `{"session":"...","text":"..."}`。
2. 按 SSE 的 `event:` 名分发：`accepted` / `reasoning` / `assistant` / `tool` /
   `approval` / `user_input` / `lifecycle`。`usage` 也会出现在这条流上（按 session
   归属，非 run 内联），一并忽略或消费皆可。
3. **忽略不认识的 `event:` 名**——前向兼容的义务在客户端侧，服务端不做能力协商，
   新增事件不会事先通知。

### 四条语义契约

- **reasoning 只在 `chat/send` 的 POST 响应里，不在 `GET /api/v1/events` 上。**
  它是 run 的内联事件，经 `RunSink::Conn` 直达发起 turn 的连接，从不进全局广播；
  ambient 流只有 `Usage` / `Proactive` / `Task`。
- **一个 run 内有多段 reasoning，每 step 一段，无显式段起止标记。**
  段边界由「本 step 首个其他事件到达」隐式给出（`assistant` delta 或 `tool` start）。
- **非 thinking 模型永远不发 reasoning。** UI 必须在没有它的情况下也说得通。
- **不落库。** `GET /api/v1/sessions/:id/history` 里没有 reasoning，永远不会有。

`/v1/responses`（OpenAI 兼容层）不提供 thinking——Responses 协议没有对应事件。
需要 thinking 的客户端必须走 native API。
