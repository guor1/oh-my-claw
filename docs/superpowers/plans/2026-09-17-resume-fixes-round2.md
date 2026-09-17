# 真机复验两处修复：resume 半落库重复卡 + 空工具名毒化会话

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉方案 H 真机复验暴露的两个缺陷：(A) 刷新落在「dispatch 已落库、工具结果未落库」窗口时前端多建一张同 `call_id` 的工具卡；(B) 模型吐出空工具名时被原样落库 → 回喂 400 → 会话永久毒化。

**Architecture:** (A) 服务端裁剪是对的（H=dispatch seq 时客户端确实缺这组事件），缺的是前端 `applyToolEvent('start')` 按 `call_id` 幂等——已有卡就认领，不新建。(B) 在 `run.rs` 工具调用组装处把空名替换为哨兵 `unknown_tool`（与旁边空 `call_id` → `gen_call_id()` 同一模式），并在 `entry_to_message` 重放时对已落库的空名做同样替换，救回已毒化的会话。

**Tech Stack:** Rust workspace（`oc-server`），Vue 3 前端（`node:test` 单测，vite 构建 dist）。

## Global Constraints

- 不得改动 `RunSink::Conn`（TUI/CLI）路径的行为与开销。
- `crates/oc-http/ui/dist/` 是 git 追踪的构建产物，改 `ui/src/` 必须重建并提交 dist。
- 注释用中文，解释「为什么」。
- 每个 Task 结束前 `cargo test --workspace` 全绿；前端 `node --test 'src/lib/*.test.js'` 全绿。

---

## 根因（实现前必读）

### (A) 半落库重复卡

```
run.rs:276  persist(dispatch)   → seq 21, mark(21)
run.rs:301  exec_tool           → 发 Tool::Start / Update* / End      ← 全部夹在 mark(21) 与 mark(22) 之间
run.rs:307  persist(tool result)→ seq 22, mark(22)
```

刷新落在 21→22 之间（本次实测窗口 470ms）：`loadHistory` 拿到 H=21，按 dispatch 建一张 `status:'running'` 的半卡；`resume(since_seq=21)` 从 mark(21) 回放 → **Start 又来一次** → `state.js` start 分支无条件 `appendMessage` → 第二张卡。随后 End 经 `list.find` 命中第一张 → 第一张定格、第二张永远转圈。截图与此完全吻合。

**为什么只补 start 幂等就够、不需要 sealed 标记：** 工具 T 的所有事件夹在 mark(dispatch_T) 与 mark(result_T) 之间。若 result_T ≤ H，回放起点 ≥ mark(result_T)，T 的事件**一条都不回放**——所以「已落满库的卡被回放 update 拼两遍输出」在 H 之下不可能发生。回放能送到的 Start，其对应的卡在历史里要么不存在（正常新建），要么是 output 为空的半卡（认领即可）。

### (B) 空工具名毒化

日志 `模型轮结束（工具调用） tool= tool_args_chars=63`：`deepseek-v4-flash-0731` 吐了 `function.name=""` 的调用。`run.rs:655` 不校验 → 执行得 `未知工具: ` → **落库 seq 10 `name:""`** → 下一请求 assistant 消息带空函数名 → provider HTTP 400 → run `Failed`。此后 `entry_to_message`（`session.rs:715-724`）每轮原样重放 seq 10 → **每轮 400**，会话卡死到压缩/reset 为止。

哨兵选 `unknown_tool`：符合 OpenAI 函数名规则 `^[a-zA-Z0-9_-]{1,64}$`；`tools_bridge.rs:241` 会回「未知工具: unknown_tool」，模型看到明确报错会自行重发。

---

## 对现有功能的影响面

| # | 功能 | 怎么变 | 兼容性 / 风险 |
|---|---|---|---|
| 1 | 前端 `applyToolEvent('start')` | 同 `call_id` 已有卡时认领而不新建 | live 路径零影响：call_id 由 uuid v7 / provider 保证唯一，正常流里同 id Start 不会来两次。只有 resume 的半落库窗口受益 |
| 2 | 工具调用落库（`run.rs`） | 仅当模型给的 name 为空：落 `unknown_tool` 而非 `""`，并 `warn!` 一条 | 合法调用零变化。以前这种情况必 400，现在变成一次可恢复的工具报错 |
| 3 | 历史重放（`entry_to_message`） | 仅对已落库的空名 `ToolCallSpec` 生效：替换为 `unknown_tool` | 只影响已被毒化的行。用户当前的 `main` 会话（seq 10）部署后自动恢复，无需手改库 |
| 4 | 不确定点 | provider 是否接受历史里出现 `tools` 列表之外的函数名 | 判断接受（400 是 schema 校验非白名单校验），无实证。若被拒，退路是 #3 改为「整组丢弃 + 丢匹配的 tool 结果」 |

**不受影响**：方案 H 的服务端裁剪、落库顺序、断连继续跑完、审批/ask_user、TUI/CLI。

---

## File Structure

| 文件 | 职责变化 |
|---|---|
| `crates/oc-server/src/run.rs` | 新增 `pub(crate) const UNKNOWN_TOOL_NAME`；工具调用组装处空名 → 哨兵 |
| `crates/oc-server/src/session.rs` | `entry_to_message` 对空名 spec 做同样替换 |
| `crates/oc-server/tests/tool_history_replay.rs` | 新增 2 个集成测试（在途兜底 + 重放消毒） |
| `crates/oc-http/ui/src/lib/state.js` | `applyToolEvent('start')` 幂等 |
| `crates/oc-http/ui/src/lib/state.test.js` | 新增 2 个用例 |
| `crates/oc-http/ui/dist/` | 重建 |

---

### Task 1: 空工具名 → `unknown_tool`（在途兜底 + 重放消毒）

**Files:**
- Modify: `crates/oc-server/src/run.rs`（顶部常量区 + `Delta::Done(FinishReason::ToolUse)` 分支，约 :636-660）
- Modify: `crates/oc-server/src/session.rs:715-724`（`entry_to_message` 的 Assistant 分支）
- Test: `crates/oc-server/tests/tool_history_replay.rs`

**Interfaces:**
- Produces: `pub(crate) const UNKNOWN_TOOL_NAME: &str = "unknown_tool";`（`crate::run::UNKNOWN_TOOL_NAME`）

- [ ] **Step 1: 写失败测试**

追加到 `crates/oc-server/tests/tool_history_replay.rs`。文件已有 `messages_sent_after_seeding(store)` helper（跑一轮、返回到达模型的消息序列）与 `wait_terminal`。补 import：

```rust
use oc_llm::mock::{ScriptStep, SequencedMock};
use oc_llm::types::ToolCallDelta;
use oc_llm::{Delta, FinishReason};
use oc_server::tools_bridge::ToolExecutor;
use oc_tools::ToolRegistry;
```

然后追加两个用例：

```rust
/// 已落库的空工具名不得原样重放——那正是 provider 400、会话永久卡死的来源。
/// 重放时替换成哨兵 `unknown_tool`，结构保持「dispatch + 结果」成对，模型能看懂。
#[tokio::test]
async fn replay_sanitizes_empty_tool_name() {
    let store = oc_store::Store::open_memory().unwrap();
    let w = store.writer();
    w.ensure_session("main".into(), "main".into()).await.unwrap();

    w.append_entry(oc_store::NewEntry::text("main", oc_store::Role::User, "跑一下", 3))
        .await
        .unwrap();
    // 真机脏数据的形状：name 为空串、args 却是完整的 exec 参数。
    w.append_entry(oc_store::NewEntry {
        tool_calls: Some(r#"[{"id":"call_bad","name":"","args":"{\"command\":\"ls\"}"}]"#.into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Assistant, "", 1)
    })
    .await
    .unwrap();
    w.append_entry(oc_store::NewEntry {
        tool_call_id: Some("call_bad".into()),
        ..oc_store::NewEntry::text("main", oc_store::Role::Tool, "未知工具: ", 2)
    })
    .await
    .unwrap();

    let sent = messages_sent_after_seeding(store).await;

    let dispatch = sent
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Assistant && !m.tool_calls.is_empty())
        .unwrap_or_else(|| panic!("应重放出带 tool_calls 的 assistant: {sent:?}"));
    assert_eq!(dispatch.tool_calls[0].id, "call_bad");
    assert_eq!(
        dispatch.tool_calls[0].name, "unknown_tool",
        "空工具名必须替换成哨兵，否则回喂 400: {sent:?}"
    );
    assert!(
        !sent.iter().any(|m| m.tool_calls.iter().any(|tc| tc.name.is_empty())),
        "到达模型的消息里不得有空函数名: {sent:?}"
    );
}

/// 模型在途吐出空工具名：不落 `""`，落哨兵；工具桥回「未知工具: unknown_tool」；
/// 下一轮回喂的 assistant 消息函数名非空（不再 400）。
#[tokio::test]
async fn empty_tool_name_from_model_is_sanitized_before_persist() {
    let round1 = vec![
        ScriptStep {
            delay: Duration::ZERO,
            delta: Delta::ToolCall(ToolCallDelta {
                call_id: "call_x".into(),
                name: Some(String::new()), // 真机：function.name = ""
                args_chunk: r#"{"command":"ls"}"#.into(),
            }),
        },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::ToolUse) },
    ];
    let round2 = vec![
        ScriptStep { delay: Duration::ZERO, delta: Delta::Text("好的。".into()) },
        ScriptStep { delay: Duration::ZERO, delta: Delta::Done(FinishReason::Stop) },
    ];
    let provider = Arc::new(SequencedMock::new(vec![round1, round2]));
    let captures = provider.captures();

    let store = oc_store::Store::open_memory().unwrap();
    store.writer().ensure_session("main".into(), "main".into()).await.unwrap();

    // 空注册表：任何名字都是「未知工具」，正好走到 tools_bridge 的那条分支。
    let tools = ToolExecutor::new(Arc::new(ToolRegistry::new()));
    let (tx, mut rx) = broadcast::channel(512);
    let sid = oc_proto::SessionId::main();
    let handle = session::spawn(
        sid.clone(),
        cfg().with_tools(tools),
        provider,
        tx,
        store.clone(),
        oc_server::diag::DiagRegistry::new().for_session(&sid),
    );
    handle.submit("跑一下".into(), handle.broadcast_sink()).await.expect("run");
    wait_terminal(&mut rx, Duration::from_secs(5)).await;

    // ① 回喂第 2 轮的请求里，assistant 的函数名是哨兵而非空串。
    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 2, "应有两轮模型请求（工具轮 + 收尾）");
    let dispatch = reqs[1]
        .messages
        .iter()
        .find(|m| !m.tool_calls.is_empty())
        .expect("第 2 轮请求应含工具调用历史");
    assert_eq!(dispatch.tool_calls[0].name, "unknown_tool");
    let result = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == oc_llm::MsgRole::Tool)
        .expect("应有工具结果");
    assert!(
        result.content.contains("未知工具: unknown_tool"),
        "工具桥应报出哨兵名，实际: {}",
        result.content
    );
    drop(reqs);

    // ② 落库的 dispatch 也是哨兵（否则下次重放又毒化）。
    let hist = store.writer().load_transcript("main".into(), 100).await.unwrap();
    let persisted = hist
        .iter()
        .find(|e| e.role == oc_store::Role::Assistant && e.tool_calls.is_some())
        .expect("应落库一条带 tool_calls 的 assistant");
    assert!(
        persisted.tool_calls.as_deref().unwrap().contains(r#""name":"unknown_tool""#),
        "落库的工具名应为哨兵: {:?}",
        persisted.tool_calls
    );
}
```

> `cfg()` 是该文件已有的 helper（`test_cfg().with_soul("人格")`）。`Store` 需要 `Clone`（`store.clone()`）——`oc_store::Store` 已是 `Clone`（`session::spawn` 的既有调用方都在传值，`tool_history_replay.rs:41` 的 helper 也按值收）。若编译报 `Store` 不可 clone，改为在 spawn 前先 `let writer = store.writer();` 留一份 writer 用于 ② 的 `load_transcript`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oc-server --test tool_history_replay`
Expected: 两个新用例 FAIL——`replay_sanitizes_empty_tool_name` 断言 `"" != "unknown_tool"`；`empty_tool_name_from_model_is_sanitized_before_persist` 断言 name 为空 / 落库含 `"name":""`。

- [ ] **Step 3: 改 `run.rs`**

在 `run.rs` 顶部常量区（`LOOP_REPEAT_THRESHOLD` / `MAX_TOOL_ROUNDS` 附近）加：

```rust
/// 模型吐出空工具名时的哨兵。
///
/// 空函数名回喂 provider 必 400（OpenAI 函数名规则 `^[a-zA-Z0-9_-]{1,64}$`），
/// 而这条 assistant 一旦落库，之后每一轮重放都 400——会话就此卡死。
/// 与旁边空 `call_id` → `gen_call_id()` 同一思路：结构保持合法，让工具桥回一条
/// 「未知工具: unknown_tool」，模型看到明确报错会自行重发。
pub(crate) const UNKNOWN_TOOL_NAME: &str = "unknown_tool";
```

把 `Delta::Done(FinishReason::ToolUse)` 分支里的 `tracing::debug!(...)` 之前插入哨兵替换，并让后面的 `step()` 与返回值都用替换后的名字：

```rust
            Delta::Done(FinishReason::ToolUse) => {
                // 空工具名兜底：见 UNKNOWN_TOOL_NAME。
                let tc_name = if tc_name.is_empty() {
                    warn!(
                        run_id = %ctx.run_id,
                        args_chars = tc_args.chars().count(),
                        "模型返回空工具名，替换为哨兵 {UNKNOWN_TOOL_NAME}"
                    );
                    UNKNOWN_TOOL_NAME.to_string()
                } else {
                    tc_name
                };
                // 有工具调用。
                tracing::debug!(
                    tool = %tc_name,
                    // …其余保持原样…
```

> `tc_name` 在该分支内被 shadow 成新绑定即可，下方 `step(.., name: tc_name.clone(), ..)` 与 `return TurnResult::ToolCall { name: tc_name, .. }` 无需改动。注意该分支的 `tc_name` 若原本是 `&mut`/外层可变量，shadow 后外层值不变，但本分支 `return` 掉不再回到外层，无副作用。

- [ ] **Step 4: 改 `session.rs`**

`entry_to_message` 的 `Role::Assistant` 分支：

```rust
        oc_store::Role::Assistant => {
            // 反序列化失败（脏数据/手改库）按无工具调用处理，不让整轮历史丢失。
            let specs: Vec<oc_llm::ToolCallSpec> = e
                .tool_calls
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default()
                .into_iter()
                // 已落库的空工具名（哨兵引入前的脏数据）重放前替换掉：
                // 否则每一轮都 400，会话卡死到压缩为止。见 run::UNKNOWN_TOOL_NAME。
                .map(|mut s: oc_llm::ToolCallSpec| {
                    if s.name.is_empty() {
                        s.name = crate::run::UNKNOWN_TOOL_NAME.to_string();
                    }
                    s
                })
                .collect();
            oc_llm::Message {
                tool_calls: specs,
                ..plain(oc_llm::MsgRole::Assistant)
            }
        }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oc-server --test tool_history_replay`
Expected: 全绿（既有 + 新增 2）

- [ ] **Step 6: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 7: 提交**

```bash
git add crates/oc-server/src/run.rs crates/oc-server/src/session.rs crates/oc-server/tests/tool_history_replay.rs
git commit -m "fix(run): 空工具名替换为哨兵 unknown_tool，在途兜底 + 重放消毒（修会话被 400 卡死）"
```

---

### Task 2: `applyToolEvent('start')` 按 `call_id` 幂等

**Files:**
- Modify: `crates/oc-http/ui/src/lib/state.js`（`applyToolEvent` 的 start 分支，约 :296-310）
- Modify: `crates/oc-http/ui/src/lib/state.test.js`（追加 2 个用例）
- Modify: `crates/oc-http/ui/dist/`（重建）

**Interfaces:**
- Consumes: `applyToolEvent(sessionId, ev)`、`messagesFor(sessionId)`（均已导出）
- Produces: 无新接口，行为变更见影响面 #1

- [ ] **Step 1: 写失败测试**

追加到 `crates/oc-http/ui/src/lib/state.test.js`（import 行补上 `applyToolEvent, messagesFor`）：

```js
import { historyMaxSeq, applyToolEvent, messagesFor } from './state.js'

test('applyToolEvent start：同 call_id 已有历史半卡时认领，不新建', () => {
  // 刷新落在「dispatch 已落库、结果未落库」窗口：loadHistory 已建一张 running 半卡。
  const list = messagesFor('t-dup')
  list.length = 0
  list.push({ id: 'tool-c1', role: 'tool', name: 'web_search', args: '{}', content: '', output: '', status: 'running' })

  // resume 回放把同一个 call 的 Start 又送来一次。
  applyToolEvent('t-dup', { call_id: 'c1', phase: { phase: 'start', name: 'web_search', args: '{}' } })

  const cards = list.filter((m) => m.id === 'tool-c1')
  assert.equal(cards.length, 1, '同 call_id 只能有一张卡')
  assert.equal(cards[0].status, 'running')

  // 随后的 End 要能定格到这唯一一张上。
  applyToolEvent('t-dup', { call_id: 'c1', phase: { phase: 'end', status: 'ok' } })
  assert.equal(list.filter((m) => m.id === 'tool-c1')[0].status, 'ok')
})

test('applyToolEvent start：新 call_id 照常建卡（live 路径不受影响）', () => {
  const list = messagesFor('t-new')
  list.length = 0
  applyToolEvent('t-new', { call_id: 'c9', phase: { phase: 'start', name: 'exec', args: '{"command":"ls"}' } })
  const cards = list.filter((m) => m.id === 'tool-c9')
  assert.equal(cards.length, 1)
  assert.equal(cards[0].name, 'exec')
  assert.equal(cards[0].status, 'running')
})
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd crates/oc-http/ui && node --test 'src/lib/*.test.js'`
Expected: 第一个新用例 FAIL——`cards.length` 为 2

- [ ] **Step 3: 改 `state.js`**

把 `applyToolEvent` 的 start 分支改为：

```js
  if (ev.phase?.phase === 'start') {
    // step 边界：定格本 step 的文本气泡（见 ChatPane.submit 的 onTool 注释）。
    finalizeLastAssistant(sessionId)
    activityFor(sessionId).visible()
    const list = messagesFor(sessionId)
    const existing = list.find(x => x.id === `tool-${ev.call_id}`)
    if (existing) {
      // 刷新落在「dispatch 已落库、结果未落库」的窗口：loadHistory 已按 dispatch
      // 建了一张 running 半卡，resume 回放的 Start 是同一个 call——认领它，别再建
      // 一张（否则第二张永远转圈）。live 路径 call_id 唯一，不会走到这里。
      existing.toolStatus = 'running'
      existing.status = 'running'
      return
    }
    appendMessage(sessionId, {
      id: `tool-${ev.call_id}`,
      // …以下原样…
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd crates/oc-http/ui && node --test 'src/lib/*.test.js'`
Expected: 全绿（含既有 9 个 + 新增 2 个）

- [ ] **Step 5: 重建 dist**

```bash
cd crates/oc-http/ui && npm run build
```

- [ ] **Step 6: 全量回归**

Run: `cargo test --workspace`
Expected: 全绿

- [ ] **Step 7: 提交**

```bash
git add crates/oc-http/ui/src/lib/state.js crates/oc-http/ui/src/lib/state.test.js crates/oc-http/ui/dist
git commit -m "fix(ui): 工具卡 start 按 call_id 幂等，修 resume 半落库窗口重复建卡"
```

---

## 验收（全部完成后）

- [ ] `cargo test --workspace` 全绿；`node --test 'src/lib/*.test.js'` 全绿；`git status` 干净
- [ ] **重建并重启 daemon**（当前跑的是修复前的二进制）
- [ ] 真机：`main` 会话（当前 seq 10 仍是空名脏数据）直接发一条消息——应正常回复，不再 400；日志不应出现 `模型返回空工具名`（那是新调用才会触发的）
- [ ] 真机：发一条多轮工具消息，输出中连续刷新 ≥5 次，工具卡数量 == DB 该轮 `tool_calls` 条数，无永久转圈的卡
