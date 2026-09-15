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

**唯一的新渲染单元**，不进 `messageMap`（它不是消息）。渲染在 `ChatPane.vue` 模板里
`MessageBubble` 的 `v-for` 之后、`.error-banner` 之前——始终位于消息流尾部。
状态机：

| 触发 | 形态 |
|---|---|
| 提交后 / 工具结束后，空窗超过阈值 | `waiting`：「等待模型 · 3s」+ 计时 |
| 收到 `reasoning` delta | `thinking`：「思考中 · 12s」+ 流式正文 |
| 收到本 step 首个可见产物（`assistant` delta 或 `tool` start） | **隐藏** |
| 工具执行中（start → end） | 保持隐藏（ToolCard 自带 running 指示） |
| `tool` end 且 run 未结束 | 回到 `waiting`（同样走延迟） |
| `lifecycle` end / error，或用户停止 | 隐藏并销毁 |

要点：

- **状态按 sessionId 存在 `state.js`，不能用 `ChatPane` 的局部 `ref`**。
  `App.vue` 里 `<ChatPane :session-id="activeSessionId.value" />` **没有 `:key`**，
  切换会话时 ChatPane 不重新挂载、只是 prop 变了——局部 ref 会让用户在会话 B
  看见会话 A 的思考卡。放进 `state.js`、与 `activeChats` 同样按 session 键入，
  back-keep 语义才成立（切走再切回，该会话自己的卡状态还在）。
  （现有 `pendingApproval` / `pendingInput` / `errorText` 都是局部 ref，有同样的
  串台问题；不在本次范围内修，但**不要照抄这个模式**。）
- **自动滚动要把卡算进去**。`ChatPane.vue:37-46` 的 `watchEffect` 只读
  `messages.value.length` 与末条 `content.length` 建立依赖；卡不在 `messages` 里，
  它的出现与形态切换**不会触发滚动**，结果是卡把内容顶上去而视口不跟。
  把卡的状态一并读进那个 `watchEffect`。
  卡自身有限高（见下），高度只会变化有限几次，不必每个 delta 都滚。
- **`waiting` 延迟出现**。空窗常常只有几百毫秒，立刻渲染会让卡闪一下就换成
  `thinking`，纯属视觉噪音。设一个阈值（**暂定 400ms**），空窗超过它才渲染 `waiting`；
  没超过就直接以 `thinking` 形态出现。两条路径最终收敛到同一状态。
  首个 delta 到达时必须取消未触发的延迟定时器。
  `thinking` **不延迟**——正文已经在手上了，没有可闪的空档。
- **阈值要实测校准，不要拍脑袋**。代码里已有现成埋点：`run.rs:510` 附近的
  「模型流已建立」记建流耗时，每轮结束的 debug 日志带 `ttfb_ms`。注意两者都**不含**
  `begin_run` 里的 prompt 组装与记忆检索，真实空窗比 `ttfb_ms` 更长，实现时
  开 debug 跑一轮看实际分布再定。
- **计时口径**：`waiting` 从本次空窗开始计（提交时刻 / 工具结束时刻），
  不是从卡出现时刻计——否则显示的秒数比真实等待短一个阈值。
  `thinking` 从本 step 首个 `reasoning` delta 计。
- **已完成的思考段不留痕**。不折叠成一行，不挂进 ToolCard，不进历史。切换 step 时
  正文清空重来。因此不需要「这段思考属于哪一步」的归属建模。
- **限高**：正文最多约 6 行，超出在卡内自动滚到底，不产生外层滚动条——
  否则长思维链会把输入框顶出视口。
- **展示开关**（localStorage）：关闭时卡**不消失**，退化为只有标题行与计时器，
  不渲染正文。完全隐藏等于退回静默，那就白做了。
- 计时器 1s tick，组件卸载时清理。

**`Approval` / `UserInput` 不需要卡做任何处理**（已核实，写下来免得实现时纠结）：
两者都由 `tools_bridge.rs:282` / `:298` 在 `ToolExecutor` 内部发出，而 `exec_tool`
在 `run.rs:702` **先**发了 `ToolPhase::Start`——审批框/提问框弹出时卡早已隐藏。
且 `tools_bridge.rs` 注释写明「审批期间不再 send」，卡不会在等用户时误报「思考中」。

**已知边界（不修）**：多 client 并发时本会话的 turn 可能排队
（`Snapshot.queued_turns`），此时卡会显示「等待模型」而实际是在排队。
Web UI 自身有 `isStreaming` 守卫不能并发提交，只有 TUI / 其他 client 同时发才会撞上，
显示也只是不够精确而非错误。留待将来协议层能区分排队与执行时再说。

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
- 键位切换 `show_reasoning`（仅控制渲染）。具体按键在实现计划里定，需避开
  `app.rs` 已注册的按键；并在状态栏或帮助里可发现，否则等于没有。
- **TUI 不做 `waiting` 态**。状态栏已有 `self.status` 文本承担「现在在干嘛」，
  再叠一个等待计时是重复。TUI 只接管 thinking 那一段。

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
- **B**：空窗短于阈值时 `waiting` **不出现**，卡直接以 `thinking` 形态出场；
  且延迟定时器已被取消（不会事后补一次闪烁）。
- **B**：**会话隔离**——会话 A 正在 thinking 时切到会话 B，B 的视图里没有卡；
  切回 A，A 的卡状态仍在。这条是防「卡状态写进 ChatPane 局部 ref」的回归保护。
- **B**：卡出现 / 切换形态时触发了自动滚动（视口跟到底部）。
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

**`waiting` 态为什么必须存在？**
它修的不是 thinking 的静默，是第 2、4 层——首 token 前的空窗。更硬的理由：
**非 thinking 模型根本不产生 reasoning delta**，对它们来说 `waiting` 是这张卡
唯一会出现的形态；砍掉它，卡对非 thinking 模型永远不出现，第 2、4 层原封不动。
thinking 模型某一步不思考直接出文本时，走的也是这条路径。
代价（短空窗闪一下）用延迟出现消解。

**为什么不合批 reasoning 事件？**
思维链 delta 密集，但与 Assistant delta 同频、同通道、同背压机制，现状已扛住。
先按原样逐条发，实测有压力再加节流——过早优化会把一个简单分支变成状态机。
