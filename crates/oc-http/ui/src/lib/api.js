/**
 * API client for the oh-my-claw native REST+SSE API (/api/v1/*).
 *
 * Auth is cookie-based: the browser exchanges the operator-supplied token for
 * an HttpOnly `oc_session` cookie via `/auth/login`, then every request rides
 * that cookie (sent automatically). In loopback no-auth mode there is no token
 * and requests simply go out unauthenticated.
 */

/** Thrown when the server requires authentication (401). */
export class AuthError extends Error {
  constructor(message) {
    super(message)
    this.name = 'AuthError'
  }
}

// ── Auth state plumbing ──────────────────────────────────────────────────────
//
// Every path that can discover "we are no longer authenticated" funnels through
// `notifyAuthRequired()`. There are three such paths and they fail differently:
// a rejected `apiFetch` (AuthError), an `EventSource` that can only report
// `onerror`, and the startup probe. Without one exit, each of them has to
// re-derive what to do — which is exactly how the "refresh logs you out" and
// "silent reconnect loop" bugs happened.

/** Startup/liveness probe outcomes. */
export const AUTH_OK = 'authed'
export const AUTH_REQUIRED = 'unauthed'
export const AUTH_UNREACHABLE = 'unreachable'

/** The single exit for "session is gone; show the login gate". */
export function notifyAuthRequired() {
  window.dispatchEvent(new CustomEvent('oc:auth-required'))
}

/**
 * Route a rejection to the login gate when it is an auth failure.
 * @returns {boolean} true if it was handled as an auth failure
 */
export function handleAuthFailure(err) {
  if (err?.name === 'AuthError') {
    notifyAuthRequired()
    return true
  }
  return false
}

/**
 * Probe whether the current session is authenticated.
 *
 * Deliberately tri-state and never rejects: only a definitive 200 or 401 is
 * conclusive. A 5xx (e.g. the daemon connection is down) or a transport error
 * must NOT be read as "authenticated" — that would show the app shell and fire
 * a cascade of doomed requests — nor may it throw, which would leave the caller
 * with no state to render.
 *
 * @returns {Promise<'authed'|'unauthed'|'unreachable'>}
 */
export async function probeAuth() {
  let resp
  try {
    resp = await fetch('/api/v1/status')
  } catch (_) {
    return AUTH_UNREACHABLE
  }
  if (resp.status === 200) return AUTH_OK
  if (resp.status === 401) return AUTH_REQUIRED
  return AUTH_UNREACHABLE
}

async function apiFetch(path, opts = {}) {
  const resp = await fetch(path, {
    ...opts,
    headers: { 'Content-Type': 'application/json', ...opts.headers },
  })
  if (!resp.ok) {
    if (resp.status === 401) throw new AuthError('authentication required')
    let msg = `${resp.status} ${resp.statusText}`
    try {
      const body = await resp.json()
      if (body?.error?.message) msg = body.error.message
    } catch (_) {}
    throw new Error(msg)
  }
  return resp
}

/**
 * Exchange the operator token for an HttpOnly session cookie.
 * @param {string} token
 */
export async function login(token) {
  const resp = await fetch('/auth/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token }),
  })
  if (!resp.ok) {
    if (resp.status === 401) throw new AuthError('令牌无效')
    throw new Error(`登录失败：${resp.status}`)
  }
  return resp.json()
}


// ── Sessions ─────────────────────────────────────────────────────────────────

export async function fetchSessions() {
  const r = await apiFetch('/api/v1/sessions')
  return r.json()
}

export async function fetchHistory(sessionId, limit = 200) {
  const r = await apiFetch(`/api/v1/sessions/${encodeURIComponent(sessionId)}/history?limit=${limit}`)
  return r.json()
}

export async function resetSession(sessionId) {
  await apiFetch(`/api/v1/sessions/${encodeURIComponent(sessionId)}/reset`, { method: 'POST' })
}

export async function compactSession(sessionId) {
  await apiFetch(`/api/v1/sessions/${encodeURIComponent(sessionId)}/compact`, { method: 'POST' })
}

// ── Slash commands ──────────────────────────────────────────────────────────

/**
 * Send a `/...` slash command to the daemon (the sole parser), returning the
 * rendered result. The client only decides "does this start with `/`".
 *
 * @param {string} session - session id
 * @param {string} text    - the raw `/...` line
 * @returns {Promise<{text: string, switch_session?: string, clear_view?: string}>}
 */
export async function sendCommand(session, text) {
  const r = await apiFetch('/api/v1/command', {
    method: 'POST',
    body: JSON.stringify({ session, text }),
  })
  return r.json()
}

export async function fetchStatus() {
  const r = await apiFetch('/api/v1/status')
  return r.json()
}

// ── Chat ─────────────────────────────────────────────────────────────────────

/**
 * Send a chat turn and stream events back via the caller-supplied callbacks.
 *
 * Returns a controller whose `abort()` stops the fetch (and signals the daemon
 * via a separate POST to /api/v1/chat/abort once the run_id is known).
 *
 * @param {object} opts
 * @param {string}   opts.session   - session id
 * @param {string}   opts.text      - user message
 * @param {function} opts.onAccepted  - ({run_id, session}) → void
 * @param {function} opts.onDelta     - (text_delta: string) → void
 * @param {function} opts.onReasoning - (delta: string) → void
 * @param {function} opts.onTool      - (event) → void
 * @param {function} opts.onApproval  - (event) → void
 * @param {function} opts.onUserInput - (event) → void
 * @param {function} opts.onEnd       - () → void
 * @param {function} opts.onError     - (message: string) → void
 */
export function sendChat(opts) {
  const ctrl = new AbortController()
  let runId = null

  ;(async () => {
    try {
      const resp = await apiFetch('/api/v1/chat/send', {
        method: 'POST',
        body: JSON.stringify({ session: opts.session, text: opts.text }),
        signal: ctrl.signal,
        headers: { Accept: 'text/event-stream' },
      })

      const reader = resp.body.getReader()
      const decoder = new TextDecoder()
      let buf = ''

      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buf += decoder.decode(value, { stream: true })

        // SSE frames are separated by blank lines.
        const frames = buf.split('\n\n')
        buf = frames.pop() ?? ''

        for (const frame of frames) {
          const eventLine = frame.match(/^event: (.+)$/m)?.[1]?.trim()
          const dataLine  = frame.match(/^data: (.+)$/m)?.[1]?.trim()
          if (!dataLine) continue

          let data
          try { data = JSON.parse(dataLine) } catch (_) { continue }

          switch (eventLine) {
            case 'accepted':
              runId = data.run_id
              opts.onAccepted?.(data)
              break
            case 'assistant':
              opts.onDelta?.(data.delta ?? '')
              break
            case 'reasoning':
              opts.onReasoning?.(data.delta ?? '')
              break
            case 'tool':
              opts.onTool?.(data)
              break
            case 'approval':
              opts.onApproval?.(data)
              break
            case 'user_input':
              opts.onUserInput?.(data)
              break
            case 'lifecycle':
              if (data.phase?.phase === 'end') {
                opts.onEnd?.()
                return
              }
              if (data.phase?.phase === 'error') {
                opts.onError?.(data.phase.message ?? 'run failed')
                return
              }
              break
          }
        }
      }
      opts.onEnd?.()
    } catch (err) {
      if (err.name !== 'AbortError') opts.onError?.(err.message)
    }
  })()

  return {
    abort() {
      ctrl.abort()
      if (runId) {
        // Fire-and-forget: tell the daemon to stop the run too.
        apiFetch('/api/v1/chat/abort', {
          method: 'POST',
          body: JSON.stringify({ run_id: runId, hard: true }),
        }).catch(() => {})
      }
    },
  }
}

/**
 * 接续一个在途 run 的剩余流（回放 + 续流），回调签名与 sendChat 一致
 * （无 onAccepted——run_id 由调用方已知）。返回 controller，abort() 只断流、不发 abort。
 */
export function resumeChat({ session, runId, onDelta, onReasoning, onTool, onEnd, onError }) {
  const ctrl = new AbortController()
  ;(async () => {
    try {
      const resp = await apiFetch(
        `/api/v1/chat/resume?run_id=${encodeURIComponent(runId)}&session=${encodeURIComponent(session)}`,
        { signal: ctrl.signal, headers: { Accept: 'text/event-stream' } },
      )
      const reader = resp.body.getReader()
      const decoder = new TextDecoder()
      let buf = ''
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buf += decoder.decode(value, { stream: true })
        const frames = buf.split('\n\n')
        buf = frames.pop() ?? ''
        for (const frame of frames) {
          const eventLine = frame.match(/^event: (.+)$/m)?.[1]?.trim()
          const dataLine = frame.match(/^data: (.+)$/m)?.[1]?.trim()
          if (!dataLine) continue
          let data
          try { data = JSON.parse(dataLine) } catch (_) { continue }
          switch (eventLine) {
            case 'assistant': onDelta?.(data.delta ?? ''); break
            case 'reasoning': onReasoning?.(data.delta ?? ''); break
            case 'tool': onTool?.(data); break
            case 'lifecycle':
              if (data.phase?.phase === 'end') { onEnd?.(); return }
              if (data.phase?.phase === 'error') { onError?.(data.phase.message ?? 'run failed'); return }
              break
          }
        }
      }
      // 干净但异常的收尾（forward 任务因 send 失败退出、或 daemon 未发终态就关流）
      // 兜底调用 onEnd，避免 UI 卡在 streaming 态。
      onEnd?.()
    } catch (err) {
      if (err.name !== 'AbortError') onError?.(err.message)
    }
  })()
  return { abort: () => ctrl.abort() }
}

// ── Replies ───────────────────────────────────────────────────────────────────

export async function approvalReply(approvalId, allow) {
  await apiFetch('/api/v1/approval/reply', {
    method: 'POST',
    body: JSON.stringify({ approval_id: approvalId, allow }),
  })
}

export async function userReply(inputId, text) {
  await apiFetch('/api/v1/user/reply', {
    method: 'POST',
    body: JSON.stringify({ input_id: inputId, text }),
  })
}

// ── Ambient event stream ──────────────────────────────────────────────────────

/**
 * Open the ambient SSE stream (Usage, Proactive, Task, Status).
 *
 * Returns a cleanup function that closes the connection.
 *
 * @param {object} callbacks
 * @param {function} callbacks.onStatus    - (snapshot) → void
 * @param {function} callbacks.onUsage     - (event) → void
 * @param {function} callbacks.onProactive - (event) → void
 * @param {function} callbacks.onTask      - (event) → void
 * @param {function} callbacks.onError     - (message) → void, called on reconnect
 */
export function openAmbientStream(callbacks) {
  // EventSource cannot set custom headers, but it does send the HttpOnly
  // cookie automatically (same-origin). No token in the URL anymore.
  const es = new EventSource('/api/v1/events')

  es.addEventListener('status',    e => callbacks.onStatus?.(JSON.parse(e.data)))
  es.addEventListener('usage',     e => callbacks.onUsage?.(JSON.parse(e.data)))
  es.addEventListener('proactive', e => callbacks.onProactive?.(JSON.parse(e.data)))
  es.addEventListener('task',      e => callbacks.onTask?.(JSON.parse(e.data)))

  es.onerror = () => {
    callbacks.onError?.('ambient stream disconnected; reconnecting…')
    // EventSource exposes neither the status code nor the body, so a 401 from an
    // expired cookie is indistinguishable from a dropped connection — and its
    // auto-reconnect would otherwise spin forever re-sending unauthenticated
    // requests behind a UI that still looks logged in. Probe to tell them apart.
    probeAuth().then((state) => {
      if (state === AUTH_REQUIRED) {
        es.close()
        notifyAuthRequired()
      }
    })
  }

  return () => es.close()
}
