# thinking 可见化 + 协议前向兼容 设计

日期：2026-09-15
状态：已确认，待写实现计划

## 背景与问题

Web UI 发完消息后长时间没有任何中间状态。探查下来是**四层静默叠加**：

| # | 静默段 | 位置 | 原因 |
|---|---|---|---|
| 1 | 模型输出思维链全程 | `oc-server/src/run.rs:591-595` | `Delta::Reasoning` 只累积，不推事件流 |
| 2 | 点发送 → 第一个事件到达 | `oc-http/ui/src/components/ChatPane.vue:59-70` | 用户气泡之后不 push 任何占位 |
| 3 | 全程 | `oc-http/ui/src/components/StatusBar.vue:34` | `status.active_run` 只在 ambient SSE 开场 snapshot 赋值一次（`native/events.rs:53`），之后只更新 Usage/Proactive/Task——这个「生成中」指示灯永远不亮 |
| 4 | 工具执行完 → 下一轮首 token | — | ToolCard 停在 ok 之后又是空白，多工具轮反复出现 |

主力模型是 thinking 类，所以 1 是大头：模型在疯狂产出 reasoning token 的整段时间里，
事件流上一个字节都没有。

探查中发现一个**比 reasoning 更严重的问题**：

- `Frame` / `Event` 都是 `#[serde(tag = "kind")]`，没有 `other` 兜底。
- `oc-tui/src/client.rs:115` 的 `serde_json::from_str::<Frame>(t)?` 遇到不认识的帧
  **直接抛错，整条连接断掉**。
- `oc-http/src/conn_pool.rs:251-259` 则是宽容的（skip + warn，连接存活）。

也就是说**今天 daemon 只要加任何一个新 `Event` 变体，旧版 TUI 与任何第三方 client
都会断连**。reasoning 只是第一个撞上这堵墙的特性。

且 `ConnectParams { proto_version, token }` 只有版本号，**没有 client 能力声明**，
daemon 无从知道对面认不认识新事件。

## 目标

1. thinking 模型的推理过程在 Web UI 与 TUI 上实时可见，消除第 1 层静默。
2. 顺带消除第 2/3/4 层静默（纯前端）。
3. 把「新增事件」从破坏性变更降级为兼容变更，让将来任何 app 接入都走同一套机制。
4. 思维链可回看历史，但**绝不参与 prompt 重放**。

## 非目标

- 不改 OpenAI 兼容层（`/v1/responses`）。Responses 协议无对应事件，保持沉默是正确的。
- 不做 reasoning 事件合批/节流（理由见「决策记录」）。
- 不把 thinking 塞进 TUI 消息流（理由见「决策记录」）。

## 设计

### L1 — 前向兼容兜底

`oc-tui/src/client.rs:115` 对齐 `conn_pool.rs` 已有的宽容策略：解不出的行 skip + warn，
连接存活。

兜底放在**解码层**，`Event` enum 不加 `Unknown` 变体——那会给每一处 match 增加一条
永远走不到的分支，污染所有消费点。

这一条独立成立：它是后面一切的地基，也应当作为协议的长期约定写进 `protocol.md`。

### L2 — 能力协商

`oc-proto/src/method.rs`：

```rust
pub struct ConnectParams {
    pub proto_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// client 声明它能处理的**可选**事件类别。daemon 只推声明过的。
    /// 缺省空 = 只收基础事件集，老 client 与第三方零改动继续工作。
    #[serde(default)]
    pub accepts: Vec<EventClass>,
}

/// 可选事件类别。基础集（Lifecycle / Assistant / Tool / Usage / Approval /
/// UserInput / Proactive / Task）永远推送，不在此列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventClass {
    Reasoning,
}
```

- daemon 在连接状态里记住 `accepts`，推可选事件前过滤。
- 新 app 接入只需声明 `accepts: ["reasoning"]`，其余照旧。
- 将来新增可选事件（token 级 usage、工具参数流式…）复用同一机制。

`docs/reference/protocol.md` 需写清两件事：**基础集 vs 可选集的划分**，以及
**「新增事件一律进可选集」**的约定。

### L3 — 新事件 `Event::Reasoning`

`oc-proto/src/event.rs`：

```rust
/// thinking 模型的推理增量。与 Assistant 分开：它不是可见回答，
/// client 应折叠 / 单行展示，且绝不参与下一轮 prompt 重放。
Reasoning {
    session: SessionId,
    run_id: RunId,
    delta: String,
},
```

- `run.rs:593` 的分支保留 `reasoning.push_str(&r)`（同轮回喂逻辑一字不动），
  **追加**一次 `emit_inline`。
- 走 `RunSink::Conn`（每 run 专属、有界、背压的通道），与 Assistant delta 同形同频。
  `sink.rs` 当初把内联事件挪出广播的理由对它完全适用——**不能进 broadcast**。
- `oc-http/src/native/chat.rs`：`belongs()` 加 run_id 匹配，`event_name()` 加 `"reasoning"`。
- `oc-http/src/sse.rs` 不动，落 `_ => None`。

加变体会让编译器把所有 `Event` 消费点报出来，逐一决定处理或忽略——这是想要的效果。

### L4 — 落库（v4 迁移）

`oc-store/src/schema.rs`，沿用 V2/V3 的 `ALTER TABLE ADD COLUMN` 模式：

```rust
/// v4：thinking 模型的推理内容列。
///
/// 只服务 UI 回看，**不参与 prompt 重放**——见 `session.rs` 的 entry→Message 映射。
pub const V4: &str = r#"
ALTER TABLE entry ADD COLUMN reasoning TEXT;
"#;
```

- `NewEntry` / `Entry` 加 `reasoning: Option<String>`；run.rs 落 assistant 条目时写入
  本轮累积值（只写产生它的那一条）。
- `HistoryEntry`（`oc-proto/src/method.rs:275` 附近）加同名可选字段透传。对老 client
  是新增字段，serde 默认忽略未知字段，安全。
- **落库与 `accepts` 正交**：不声明 accepts 的 client 收不到实时事件，但历史照样存下，
  换个 client 仍能回看。

**关键不变量**：`oc-server/src/session.rs:642` 的 `reasoning: None` **保持不变**。
那行现有注释写的是「thinking 内容不落库，仅实时轮回喂」，本次改动后前半句不再成立，
必须改成说明「**刻意不回喂**」——否则下一个读代码的人会把它当成遗漏而"修好"，
直接把历史思维链灌进上下文。

### L5 — Web UI

新增 `ThinkingCard.vue`，消息模型加 `role: 'reasoning'`：

- 首个 reasoning delta → push `{ role:'reasoning', content:'', pending:true, startedAt }`。
- 后续 delta 追加。
- **定格条件**（`pending=false` + 自动折叠成一行「思考 18s」，可点开）：收到本轮的
  首个 `assistant` delta、**或**首个 `tool` start、**或** `lifecycle` end/error —— 三者
  取最先到达者。只认 assistant delta 是不够的：纯工具轮不产出可见文本，卡会永远
  停在 pending。
- `loadHistory`：带 `reasoning` 的 assistant 条目前插一条**已折叠**的卡。
- 展示开关存 localStorage，关掉时不渲染卡（事件照收），切换即时生效。

同时消除另外三层静默：

- **占位**：submit 后立刻挂一条 `role:'waiting'` 卡（「等待模型 · 3s」+ 计时）。
  它与 ThinkingCard 是**两个独立组件**：收到首个 reasoning delta 时移除 waiting 卡、
  建 ThinkingCard；若首个到达的是 assistant/tool 事件（非 thinking 模型），
  直接移除 waiting 卡。
- **工具后空档**：ToolCard 收到 `end` 且本轮未结束（未收到 lifecycle end/error）
  → 重新挂 waiting 卡。
- **StatusBar**：`active_run` 改为从 `activeChats` 派生（`isStreaming` 已存在），
  不再依赖那个永不更新的 snapshot 字段。

### L6 — TUI 状态栏滚动

`oc-tui/src/app.rs`：

- `App` 加 `reasoning_tail: String` 与 `show_reasoning: bool`。tail 有界：保留末尾
  512 字符（状态栏最多用掉一行，多留的部分只为窄窗口下不至于截空）。
- 收到 `Event::Reasoning` → 换行压成空格后追加，超过上限从**头部**丢弃。
- `draw()` 的 `chunks[2]` 状态栏：thinking 活跃时渲染 `💭 <tail>`，按状态栏剩余宽度
  **从右往左**取能放下的部分。delta 持续追加天然形成左滚，**不需要动画定时器**。
  中文双宽要按**显示列宽**算，不能按字符数。
- 首个 Assistant delta 或 `Lifecycle::End` / `Error` → 清空 tail，恢复原状态文本。
- 键位切换 `show_reasoning`。关掉只是不渲染，`accepts` 仍声明——**连接级能力与
  展示偏好分离**。

## 实施顺序

```
L1 → L2 → L3 → L4 → (L5 ∥ L6)
```

L1 / L2 是地基且能独立验证；L3 之后 Web 与 TUI 可并行。

## 测试要点

- **L1**：喂一行伪造的未知 `kind` 事件，断言 TUI client 跳过该行且连接存活。
- **L2**：不声明 `accepts` 的连接收不到 `Reasoning`；声明了的收得到。
- **L3**：mock provider 产出 `Delta::Reasoning`，断言事件流上出现 `Reasoning`，
  且同轮回喂的 `Message.reasoning` 仍被正确带回（回归保护）。
- **L4**：v3 库跑迁移到 v4，既有行 `reasoning` 为 NULL；
  **历史重放构造的 `Message.reasoning` 恒为 `None`**——这条断言是防回归的核心。
- **L6**：含中文的 tail 按显示列宽截断，不出现半个字符或越界。

## 决策记录

**为什么开关放纯客户端？**
L2 已经回答了「这个 client 认不认识 reasoning」，L3 的开关只管「此刻用户想不想看」。
daemon 按 `accepts` 持续推，各 client 自存偏好：切换即时生效、零协议往返，
loopback 带宽不是问题。将来远程接入若在意带宽，再加 `ChatSendParams.stream_reasoning`
的 per-turn override——L2 机制已就位，那时是小改。

**为什么不合批 reasoning 事件？**
思维链 delta 密集，但与 Assistant delta 同频、同通道、同背压机制，现状已扛住。
先按原样逐条发，实测有压力再加节流——过早优化会把一个简单分支变成状态机。

**为什么 TUI 的 thinking 只进状态栏？**
消息流是对话记录，思维链是过程噪音。且 TUI 没有折叠能力，塞进消息流会把历史冲掉。

**为什么兜底放解码层而不是 `Event::Unknown`？**
`Unknown` 变体会给每一处 `match` 增加一条永远走不到的分支，污染所有消费点；
而解码层兜底只需两个 client 各改一处。
