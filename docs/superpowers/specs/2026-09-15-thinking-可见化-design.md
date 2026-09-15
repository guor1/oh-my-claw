# thinking 实时可见化 设计

日期：2026-09-15
状态：已确认，待写实现计划

## 背景与问题

Web UI 发完消息后长时间没有任何中间状态。探查下来是**四层静默叠加**：

| # | 静默段 | 位置 | 原因 |
|---|---|---|---|
| 1 | 模型输出思维链全程 | `oc-server/src/run.rs:591-595` | `Delta::Reasoning` 只累积，不推事件流 |
| 2 | 点发送 → 第一个事件到达 | `oc-http/ui/src/components/ChatPane.vue:59-70` | 用户气泡之后不 push 任何占位 |
| 3 | 全程 | `oc-http/ui/src/components/StatusBar.vue:34` | `status.active_run` 只在 ambient SSE 开场 snapshot 赋值一次（`native/events.rs:53`），之后只更新 Usage/Proactive/Task——这个「生成中」指示灯**永远不亮** |
| 4 | 工具执行完 → 下一轮首 token | — | ToolCard 停在 ok 之后又是空白，多工具轮反复出现 |

主力模型是 thinking 类，所以第 1 层是大头：模型在产出 reasoning token 的整段时间里，
事件流上一个字节都没有。

### 一次 run 的真实结构

设计建立在这个结构上，先写清楚：

```
run（用户的一次提问）
└── step × N              ← 一次模型调用 = 一条 assistant 消息
    ├── reasoning         为什么这么做
    ├── content           说了什么      （可选）
    └── tool_call         做了什么      （可选，最多 1 个）+ 它的结果
```

依据：

- `run.rs:518` 注释原话：「模型的输出有三个去处（**acc / reasoning / tc_args**）」——
  一次模型流的三个 buffer。
- `run.rs:151` 主循环每次迭代开头 `reasoning.clear()`——reasoning 的作用域就是这一轮。
- `run.rs:258-268` 产生工具调用时，把本轮 reasoning 绑在**那条带 `tool_calls` 的
  assistant 消息**上回喂；不带回去 DeepSeek 直接 400。**协议层面就规定了
  reasoning 与 tool_calls 同属一条消息**。
- `openai.rs:288`：「只取 `tool_calls[index=0]`：本轮架构一次执行一个工具」——
  一步最多一个工具。

所以 reasoning 与 tool_call **不是并列的两类事件**，而是同一条 assistant 消息的
两个字段。最后一步没有工具调用，它的 action 就是产出最终回答。

## 目标

1. thinking 模型的推理过程在 Web UI 与 TUI 上**实时**可见，消除第 1 层静默。
2. 同一个组件顺带消除第 2、4 层静默；第 3 层单独修。
3. 历史态与今天**逐字相同**——不为 thinking 留任何常驻结构。

## 非目标

- **thinking 不落库**。不加迁移、不加列、不改 history 接口。
- 不改 OpenAI 兼容层（`/v1/responses`）：Responses 协议无对应事件，保持沉默是正确的。
- 不做 client 能力协商。项目未上生产，所有 client 都在本仓库内，一起改即可；
  真要断代时 bump 已有的 `ConnectParams.proto_version`。
- 不做 reasoning 事件合批/节流（理由见「决策记录」）。

## 设计

### A. 协议：新增 `Event::Reasoning`

`oc-proto/src/event.rs`：

```rust
/// thinking 模型的推理增量。与 Assistant 分开：它不是可见回答，
/// 只供客户端实时展示，不落库、不参与 prompt 重放。
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
- 加变体会让编译器报出所有 `Event` 消费点，逐一决定处理或忽略——这是想要的效果。

`oc-server/src/session.rs:642` 的 `reasoning: None` 与其现有注释（「thinking 内容
不落库，仅实时轮回喂」）**继续成立，一个字都不用改**。

### B. Web UI：单例活动卡 `LiveActivity.vue`

**唯一的新渲染单元**，常驻消息流尾部，不进 `messageMap`（它不是消息）。
状态机：

| 触发 | 形态 |
|---|---|
| 提交后立即 | `waiting`：「等待模型 · 3s」+ 计时 |
| 收到 `reasoning` delta | `thinking`：「思考中 · 12s」+ 流式正文 |
| 收到本 step 首个可见产物（`assistant` delta 或 `tool` start） | **隐藏** |
| 工具执行中（start → end） | 保持隐藏（ToolCard 自带 running 指示） |
| `tool` end 且 run 未结束 | 回到 `waiting` |
| `lifecycle` end / error，或用户停止 | 隐藏并销毁 |

要点：

- **已完成的思考段不留痕**。不折叠成一行，不挂进 ToolCard，不进历史。切换 step 时
  正文清空重来。因此不需要「这段思考属于哪一步」的归属建模。
- **限高**：正文最多约 6 行，超出在卡内自动滚到底，不产生外层滚动条——
  否则长思维链会把输入框顶出视口。
- **展示开关**（localStorage）：关闭时卡**不消失**，退化为只有标题行与计时器，
  不渲染正文。完全隐藏等于退回静默，那就白做了。
- 计时器 1s tick，组件卸载时清理。

`loadHistory` 与 `MessageBubble` **不改**：历史里本就没有 thinking。

### C. TUI：状态栏滚动

`oc-tui/src/app.rs`，与 B 同构——瞬时、单例、不留痕：

- `App` 加 `reasoning_tail: String` 与 `show_reasoning: bool`。tail 保留末尾 512 字符
  （状态栏最多用掉一行，多留只为窄窗口下不至于截空）。
- 收到 `Event::Reasoning` → 换行压成空格后追加，超上限从**头部**丢弃。
- `draw()` 的 `chunks[2]`：thinking 活跃时渲染 `💭 <tail>`，按状态栏剩余宽度
  **从右往左**取能放下的部分。delta 持续追加天然形成左滚，**不需要动画定时器**。
  中文双宽按**显示列宽**算，不能按字符数。
- 首个 Assistant delta / Tool start / `Lifecycle::End|Error` → 清空 tail，恢复原状态文本。
- 键位切换 `show_reasoning`（仅控制渲染）。

### D. StatusBar 死指示灯

`StatusBar.vue:34` 的 `active_run` 改为从 `activeChats` 派生（`ChatPane` 里的
`isStreaming` 已经是这个语义），不再依赖那个永不更新的 snapshot 字段。

### E. 解码层兜底

`oc-tui/src/client.rs:115` 的 `serde_json::from_str::<Frame>(t)?` 遇到不认识的帧会
**抛错断连**；`oc-http/src/conn_pool.rs:251-259` 则是 skip + warn。把 TUI 对齐到宽容策略。

不是为了版本兼容（本项目不考虑），而是「一条脏行不该杀掉整条连接」。顺手做，
与 A 同一个提交。

## 实施顺序

```
A（含 E） → (B ∥ C ∥ D)
```

A 落地后 Web / TUI / StatusBar 三件事互不依赖，可并行。

## 测试要点

- **A**：mock provider 产出 `Delta::Reasoning`，断言事件流上出现 `Reasoning`；
  且同轮回喂的 `Message.reasoning` 仍被正确带回（**回归保护**：
  `run.rs:264` 那段不能被改坏，否则 DeepSeek 400）。
- **A**：`Reasoning` 事件不出现在 `/v1/responses` 的 SSE 输出里。
- **B**：状态机六条迁移各一例；尤其「tool end 后回到 waiting」与
  「lifecycle end 后销毁」。
- **B**：`loadHistory` 的产物不含任何 thinking 单元。
- **C**：含中文的 tail 按显示列宽截断，不出现半个字符或越界。
- **E**：喂一行伪造的未知 `kind` 帧，断言 TUI client 跳过该行且连接存活。

## 决策记录

**为什么 thinking 不落库、不留痕？**
落库要动 schema、history 接口，并引入「截断续写时多轮 reasoning 怎么合并」的新问题
（`run.rs:151` 每轮清零，`run.rs:192` 已论证被砍断的 reasoning 连回喂价值都没有）。
而它的全部价值在**运行中告诉用户"还活着、在想什么"**，事后回看的价值很低。
不留痕还顺带消掉了「思考归属哪一步」的整套建模——收益/代价严重不对称。
代价明确：思考看过就没了，卡片消失后无法回看。

**为什么是一张单例活动卡，而不是每段思考一张卡？**
每段一张卡就必须回答「它属于哪一步、历史里怎么显示、折叠态什么样」；
单例卡把这些问题全部消除，且天然覆盖第 2、4 层静默——等待与思考本来就是
同一件事的两个阶段。

**为什么开关关闭时卡不消失？**
开关管的是「要不要看思维链内容」，不是「要不要知道模型在动」。
完全隐藏就退回静默了。

**为什么不合批 reasoning 事件？**
思维链 delta 密集，但与 Assistant delta 同频、同通道、同背压机制，现状已扛住。
先按原样逐条发，实测有压力再加节流——过早优化会把一个简单分支变成状态机。
