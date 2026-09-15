/**
 * API client for the oh-my-claw native REST+SSE API (/api/v1/*).
 *
 * Token is injected by the server into window.__OC_TOKEN__ at page load.
 * An empty string means no-auth (loopback-only mode), in which case the
 * Authorization header is omitted rather than sent as "Bearer ".
 */

const token = () => window.__OC_TOKEN__ || ''

function authHeaders() {
  const t = token()
  return t ? { Authorization: `Bearer ${t}` } : {}
}

async function apiFetch(path, opts = {}) {
  const resp = await fetch(path, {
    ...opts,
    headers: { 'Content-Type': 'application/json', ...authHeaders(), ...opts.headers },
  })
  if (!resp.ok) {
    let msg = `${resp.status} ${resp.statusText}`
    try {
      const body = await resp.json()
      if (body?.error?.message) msg = body.error.message
    } catch (_) {}
    throw new Error(msg)
  }
  return resp
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
  // EventSource doesn't support custom headers, so the token is injected into
  // the URL as a query param for this specific request. The native API accepts
  // it there too (handled in auth middleware via query-param fallback).
  const t = token()
  const url = t
    ? `/api/v1/events?token=${encodeURIComponent(t)}`
    : '/api/v1/events'

  const es = new EventSource(url)

  es.addEventListener('status',    e => callbacks.onStatus?.(JSON.parse(e.data)))
  es.addEventListener('usage',     e => callbacks.onUsage?.(JSON.parse(e.data)))
  es.addEventListener('proactive', e => callbacks.onProactive?.(JSON.parse(e.data)))
  es.addEventListener('task',      e => callbacks.onTask?.(JSON.parse(e.data)))
  es.onerror = () => callbacks.onError?.('ambient stream disconnected; reconnecting…')

  return () => es.close()
}
