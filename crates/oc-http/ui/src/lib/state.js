/**
 * Global application state using Vue 3 reactivity.
 *
 * Keeps session list, active session, message map, and ambient status in one
 * place so any component can subscribe without prop-drilling.
 */

import { reactive, shallowReactive } from 'vue'
import { fetchSessions, fetchHistory, openAmbientStream, handleAuthFailure, resumeChat } from './api.js'
import { createActivity } from './activity.js'

// ── Session list ──────────────────────────────────────────────────────────────

export const sessions = reactive({ list: [], loading: false, error: null })

// silent: true skips the loading skeleton — use for background refreshes where
// there's already data in the list and a flash would be jarring (e.g. onAccepted).
export async function loadSessions({ silent = false } = {}) {
  if (!silent) sessions.loading = true
  sessions.error = null
  try {
    sessions.list = await fetchSessions()
  } catch (e) {
    // An expired cookie must reach the login gate, not sit in an error banner.
    if (!handleAuthFailure(e)) sessions.error = e.message
  } finally {
    sessions.loading = false
  }
}

// ── Active session ────────────────────────────────────────────────────────────

export const activeSessionId = reactive({ value: 'main' })

// ── Messages per session ──────────────────────────────────────────────────────
// { [sessionId]: Message[] }
// Message: { id, role, content, pending?, error? }
//
// A plain reactive object, not a Map: Vue's collection reactivity only tracks
// get/set/has/delete on a Map — the stored array values would NOT be made
// reactive, so in-place `push`/content mutation wouldn't re-render. Assigning
// into a plain reactive object deep-wraps the array, so per-message streaming
// updates trigger renders.

const messageMap = reactive({})

export function messagesFor(sessionId) {
  if (!messageMap[sessionId]) messageMap[sessionId] = []
  return messageMap[sessionId]
}

// 每会话的历史水位：`loadHistory` 拿到的最大 entry seq。
// resume 时上报给服务端，让它跳过「已落库、客户端已渲染」的那几组事件——
// 不报的话工具卡会建两张、工具输出拼两遍、assistant 正文渲染两遍。
const historySeq = {}

/** 历史条目里的最大 seq；空历史 / 缺字段返回 0（= 全量回放）。 */
export function historyMaxSeq(entries) {
  let max = 0
  for (const e of entries ?? []) {
    if (typeof e?.seq === 'number' && e.seq > max) max = e.seq
  }
  return max
}

export async function loadHistory(sessionId) {
  let entries
  try {
    entries = await fetchHistory(sessionId)
  } catch (e) {
    // A mid-session cookie expiry surfaces here; route it to the login gate so
    // it can't become an unhandled rejection at a fire-and-forget call site.
    // Anything else is the caller's to handle (ChatPane renders it on the card).
    if (handleAuthFailure(e)) return
    throw e
  }
  // Build a structured message list: assistant dispatch entries that carry
  // `tool_calls` become per-call ToolCard units; each is matched to its
  // subsequent `tool` result entry (by `tool_call_id`), mirroring openclaw's
  // extractToolCards + findFirstUnmatchedCard call-id pairing.
  const msgs = []
  for (const e of entries) {
    if (e.role === 'assistant' && Array.isArray(e.tool_calls) && e.tool_calls.length > 0) {
      // Emit the assistant's prose (if any) as its own bubble, then one tool
      // card per call.
      if ((e.content ?? '').trim() !== '') {
        msgs.push({ id: `hist-${e.seq}`, role: 'assistant', content: e.content, ts: e.created_at })
      }
      for (const tc of e.tool_calls) {
        msgs.push({
          id: `tool-${tc.id}`,
          role: 'tool',
          name: tc.name,
          args: tc.args,
          content: '',
          output: '',
          status: 'running', // provisional until matched by a result entry below
          ts: e.created_at,
        })
      }
      continue
    }
    if (e.role === 'tool' && e.tool_call_id) {
      // Match the result to its dispatch card.
      const card = msgs.findLast((m) => m.id === `tool-${e.tool_call_id}`)
      if (card) {
        card.output = e.content
        card.content = e.content
        card.status = inferToolStatus(e.content)
      } else {
        msgs.push({ id: `hist-${e.seq}`, role: 'tool', name: undefined, content: e.content, ts: e.created_at })
      }
      continue
    }
    msgs.push({ id: `hist-${e.seq}`, role: e.role, content: e.content, ts: e.created_at })
  }
  historySeq[sessionId] = historyMaxSeq(entries)
  messageMap[sessionId] = msgs
}

// A tool result without an explicit status field is inferred from content:
// our tool layer prefixes failures with "工具错误:" / "exec 失败:" or a non-zero
// exit code marker.
function inferToolStatus(content) {
  const text = (content ?? '').trim()
  if (/^工具错误[:：]/.test(text)) return 'error'
  if (/^exec 失败[:：]/.test(text)) return 'error'
  if (/\[退出码:\s*[1-9]/.test(text)) return 'error'
  return 'ok'
}

export function appendMessage(sessionId, msg) {
  const list = messagesFor(sessionId)
  list.push(msg)
}

/**
 * Append a slash command and its result as one unit (rendered by CommandCard).
 *
 * Command and output are one message, not two: they belong to each other, and
 * splitting them meant a `/help` result could end up separated from its echo by
 * an unrelated message arriving mid-request.
 */
export function appendCommandMessage(sessionId, command) {
  const list = messagesFor(sessionId)
  list.push({
    id: `cmd-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
    role: 'command',
    command,
    content: '',
  })
  // Return the *proxy* Vue created on push, not the raw literal: mutating the
  // raw object writes the value but notifies no watchers, so the card would
  // render its header and never show the output filled in later.
  return list.at(-1)
}

/** Attach a command's result text (or error) to the card created above. */
export function resolveCommandMessage(msg, { text, error } = {}) {
  msg.content = text ?? ''
  if (error) msg.error = error
}

/**
 * Ensure a command card sits (exactly once) at the tail of `sessionId`'s list.
 *
 * A command can clear the view it was typed into (`/clear`) or land in a
 * different session than it started in (`/new`, `/session <id>`), and both of
 * those replace the target's message array outright — so the card is re-placed
 * after those effects rather than moved between lists.
 */
export function placeCommandMessage(msg, sessionId) {
  for (const [id, list] of Object.entries(messageMap)) {
    const at = list.indexOf(msg)
    if (at !== -1 && (id !== sessionId || at !== list.length - 1)) list.splice(at, 1)
  }
  const list = messagesFor(sessionId)
  if (!list.includes(msg)) list.push(msg)
}

/** Drop a session's in-memory message list (used by `/clear` and `/new`). */
export function clearSessionMessages(sessionId) {
  if (messageMap[sessionId]) messageMap[sessionId] = []
}

export function updateLastAssistant(sessionId, delta) {
  if (!delta) return
  const list = messagesFor(sessionId)
  const last = list.findLast(m => m.role === 'assistant' && m.pending)
  if (last) {
    last.content += delta
  } else {
    list.push({ id: `stream-${Date.now()}`, role: 'assistant', content: delta, pending: true })
  }
}

export function finalizeLastAssistant(sessionId) {
  const list = messagesFor(sessionId)
  const last = list.findLast(m => m.role === 'assistant' && m.pending)
  if (last) delete last.pending
}

// ── Status / ambient stream ───────────────────────────────────────────────────

export const status = reactive({
  model: '',
  provider: '',
  context_window: 0,
  last_input_tokens: null,
  active_run: null,
  queued_turns: 0,
  session: 'main',
})

export const notification = reactive({ text: null })

let closeAmbient = null

/**
 * Tear down the ambient stream, if one is open.
 *
 * Explicit teardown matters on auth expiry: without it the old EventSource keeps
 * auto-reconnecting behind the login gate, re-sending unauthenticated requests.
 */
export function stopAmbientStream() {
  if (closeAmbient) {
    closeAmbient()
    closeAmbient = null
  }
}

export function startAmbientStream() {
  stopAmbientStream()
  closeAmbient = openAmbientStream({
    onStatus(snap) {
      Object.assign(status, snap)
      // 状态晚到也能接上：若 main 有在途 run 且当前无活跃流，触发一次 resume。
      if (snap.active_run && activeSessionId.value === 'main' && !activeChats.has('main')) {
        maybeResume('main')
      }
    },
    onUsage(ev) {
      if (ev.session === (activeSessionId.value ?? 'main')) {
        status.last_input_tokens = ev.input_tokens
        status.context_window = ev.context_window
      }
    },
    onProactive(ev) {
      notification.text = ev.text
      setTimeout(() => { notification.text = null }, 12000)
    },
    onTask(_ev) {
      // Refresh session list so task-spawned sessions appear.
      loadSessions()
    },
    onError(_msg) {
      // EventSource reconnects automatically; no manual action needed.
    },
  })
}

// ── Per-session active chat controllers ──────────────────────────────────────
// Map<sessionId, { abort }> — keeps the streaming connection alive when user
// switches sessions (back-keep semantics).
//
// Reactive so `activeChats.has(sessionId)` in ChatPane tracks membership, but
// the abort controllers themselves are opaque — hence shallowReactive.

export const activeChats = shallowReactive(new Map())

export function setActiveChatCtrl(sessionId, ctrl) {
  activeChats.set(sessionId, ctrl)
}

export function clearActiveChatCtrl(sessionId) {
  activeChats.delete(sessionId)
}

// ── Per-session live activity (单例活动卡) ─────────────────────────────
// 按 sessionId 键入，与 activeChats 同构：ChatPane 无 :key，切换会话不重新
// 挂载，卡状态若放局部 ref 会在会话间串台。放进这里 back-keep 语义才成立。
const activities = reactive({})

export function activityFor(sessionId) {
  if (!activities[sessionId]) activities[sessionId] = createActivity()
  return activities[sessionId]
}

// ── 工具事件渲染（ChatPane 与 maybeResume 共用） ───────────────────────────
// 与 ChatPane.submit 的 onTool 同构：start 建卡、update 攒输出、end 定格。
// 唯一抽出到 state.js 的前端重构点，避免两份拷贝。
export function applyToolEvent(sessionId, ev) {
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
      role: 'tool',
      name: ev.phase.name,
      args: ev.phase.args ?? '',
      content: '',
      output: '',
      toolStatus: 'running',
      status: 'running',
    })
  } else if (ev.phase?.phase === 'update') {
    // Stream tool progress into the card's output buffer.
    const list = messagesFor(sessionId)
    const m = list.find(x => x.id === `tool-${ev.call_id}`)
    if (m) {
      m.output = (m.output ?? '') + (ev.phase.chunk ?? '')
      m.content = m.output
    }
  } else if (ev.phase?.phase === 'end') {
    const list = messagesFor(sessionId)
    const m = list.find(x => x.id === `tool-${ev.call_id}`)
    if (m) {
      m.toolStatus = ev.phase.status
      m.status = ev.phase.status
    }
    activityFor(sessionId).toolEnd(Date.now())
  }
}

/**
 * 刷新后若 main 会话有在途 run，则接续其剩余流，渲染进当前消息列表。
 * 复用 ChatPane 的渲染回调（onDelta/onTool/onReasoning/onEnd），
 * 回放与续流对渲染透明。
 */
export function maybeResume(sessionId) {
  if (sessionId !== 'main') return
  const rid = status.active_run
  if (!rid) return
  if (activeChats.has(sessionId)) return   // 已有流在跑，不重复挂

  // 历史还没加载完就 resume，等于声明「我什么都没有」→ 服务端全量回放，
  // 随后 loadHistory 再渲染一遍同样内容 → 正是本次要修的重复。宁可不接。
  const sinceSeq = historySeq[sessionId]
  if (sinceSeq === undefined) return

  const target = sessionId
  const act = activityFor(target)
  act.arm(Date.now())

  const ctrl = resumeChat({
    session: target,
    runId: rid,
    sinceSeq,
    onReasoning(delta) { act.reasoning(delta, Date.now()) },
    onDelta(delta) {
      act.visible()
      updateLastAssistant(target, delta)
    },
    onTool(ev) {
      // 与 ChatPane.submit 的 onTool 同构：start 建卡、update 攒输出、end 定格。
      applyToolEvent(target, ev)
    },
    onEnd() {
      act.end()
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
    },
    onError(msg) {
      act.end()
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
    },
  })
  setActiveChatCtrl(target, ctrl)
}
