<script setup>
import { onMounted, onUnmounted, ref } from 'vue'
import SessionSidebar from './components/SessionSidebar.vue'
import ChatPane from './components/ChatPane.vue'
import StatusBar from './components/StatusBar.vue'
import Notification from './components/Notification.vue'
import LoginGate from './components/LoginGate.vue'
import { probeAuth, AUTH_OK, AUTH_REQUIRED } from './lib/api.js'
import {
  loadSessions,
  loadHistory,
  startAmbientStream,
  stopAmbientStream,
  activeSessionId,
  maybeResume,
} from './lib/state.js'

// 'probing' = 探测中；'login' = 需要登录；'app' = 已认证；'unreachable' = 网关不可达。
const phase = ref('probing')

async function bootstrap() {
  // 三态探测：只有明确的 200/401 才算结论。5xx / 传输失败既不能当成"已认证"
  // （那样会进主界面然后连环报错），也不能让它抛出（那样会白屏）。
  const state = await probeAuth()
  if (state === AUTH_REQUIRED) {
    phase.value = 'login'
  } else if (state === AUTH_OK) {
    phase.value = 'app'
    start()
  } else {
    phase.value = 'unreachable'
  }
}

async function start() {
  // 先加载历史，再开 ambient 流。否则 status 快照带着 active_run 到达时，
  // onStatus 触发的 maybeResume 会往 messageMap['main'] 里写 resume 增量，
  // 随后 loadHistory 完成把 messageMap['main'] 整体覆盖，丢掉那些增量，
  // 而 activeChats 已登记，显式的 maybeResume 又变成 no-op。
  await loadHistory(activeSessionId.value)
  startAmbientStream()
  loadSessions()
  maybeResume(activeSessionId.value)
}

// 任何发现"会话已失效"的路径（apiFetch 的 AuthError、EventSource 的 onerror
// 探测、main.js 的 errorHandler）都汇到这里，是登录态失效的唯一出口。
function onAuthRequired() {
  // 先停掉 ambient 流：否则它会在登录页背后一直重连、反复发未鉴权请求。
  stopAmbientStream()
  phase.value = 'login'
}

function onLoggedIn() {
  phase.value = 'app'
  start()
}

onMounted(() => {
  window.addEventListener('oc:auth-required', onAuthRequired)
  bootstrap()
})

onUnmounted(() => {
  window.removeEventListener('oc:auth-required', onAuthRequired)
})

async function handleSelectSession(id) {
  activeSessionId.value = id
  await loadHistory(id)
  maybeResume(id)
}
</script>

<template>
  <LoginGate v-if="phase === 'login'" @authed="onLoggedIn" />

  <div v-else-if="phase === 'app'" class="app">
    <SessionSidebar @select="handleSelectSession" />
    <main class="main">
      <ChatPane :session-id="activeSessionId.value" />
      <StatusBar />
    </main>
  </div>

  <!-- 网关不可达：明说原因并给重试，而不是留一片白屏。 -->
  <main v-else-if="phase === 'unreachable'" class="fallback">
    <div class="fallback-card">
      <h1 class="fallback-title">连不上网关</h1>
      <p class="fallback-sub">daemon 可能没在运行，或 HTTP 网关已停止。启动后重试。</p>
      <button class="fallback-retry" @click="bootstrap">重试</button>
    </div>
  </main>

  <Notification />
</template>

<style>
:root {
  /* Neutral ramp (OKLCH-inspired lightness steps, encoded as HSL for browser compat) */
  --n-0:  hsl(220 15% 97%);   /* off-white surface */
  --n-1:  hsl(220 12% 93%);
  --n-2:  hsl(220 10% 86%);
  --n-3:  hsl(220  9% 74%);
  --n-4:  hsl(220  8% 58%);   /* muted text */
  --n-5:  hsl(220  7% 42%);
  --n-6:  hsl(220  8% 28%);
  --n-7:  hsl(220  9% 20%);   /* raised surface dark */
  --n-8:  hsl(220 10% 14%);   /* surface dark */
  --n-9:  hsl(220 12% 10%);   /* off-black */

  /* Accent (blue) */
  --accent-h: 218;
  --accent:       hsl(var(--accent-h) 80% 52%);
  --accent-hover: hsl(var(--accent-h) 80% 44%);
  --accent-muted: hsl(var(--accent-h) 60% 70%);
  --accent-bg:    hsl(var(--accent-h) 80% 96%);
  --accent-ring:  hsl(var(--accent-h) 80% 52% / 0.35);

  /* Semantic */
  --success: hsl(150 60% 38%);
  --danger:  hsl(  2 70% 50%);
  --warn:    hsl( 38 90% 46%);

  /* Roles — light mode */
  --surface:      var(--n-0);
  --surface-raised: white;
  --border:       var(--n-2);
  --text:         hsl(220 12% 13%);  /* ~13% lightness, meets AA */
  --text-muted:   var(--n-4);
  --sidebar-w:    220px;

  /* Radii */
  --r-sm: 4px;
  --r-md: 8px;

  /* Space rhythm (8px base) */
  --sp-1: 4px;
  --sp-2: 8px;
  --sp-3: 12px;
  --sp-4: 16px;
  --sp-5: 24px;
  --sp-6: 32px;

  /* Typography */
  --font-mono: ui-monospace, "SF Mono", "Cascadia Code", "JetBrains Mono", Consolas, monospace;
}

@media (prefers-color-scheme: dark) {
  :root {
    --surface:       var(--n-8);
    --surface-raised: var(--n-7);
    --border:        var(--n-6);
    --text:          var(--n-0);
    --text-muted:    var(--n-3);
    --accent:        hsl(var(--accent-h) 65% 62%);  /* lower chroma, higher L */
    --accent-hover:  hsl(var(--accent-h) 65% 70%);
    --accent-muted:  hsl(var(--accent-h) 45% 50%);
    --accent-bg:     hsl(var(--accent-h) 30% 18%);
  }
}

*,
*::before,
*::after {
  box-sizing: border-box;
  margin: 0;
  padding: 0;
}

body {
  font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
  font-size: 14px;
  line-height: 1.6;
  background: var(--surface);
  color: var(--text);
  height: 100dvh;
  overflow: hidden;
}

button {
  font: inherit;
  cursor: pointer;
  border: none;
  background: none;
  color: inherit;
}

button:focus-visible,
input:focus-visible,
textarea:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}

.app {
  display: grid;
  grid-template-columns: var(--sidebar-w) 1fr;
  height: 100dvh;
}

.main {
  display: grid;
  grid-template-rows: 1fr auto;
  overflow: hidden;
  border-left: 1px solid var(--border);
}

/* 网关不可达时的兜底屏，与 LoginGate 同一套卡片语言。 */
.fallback {
  height: 100dvh;
  display: grid;
  place-items: center;
  padding: var(--sp-5);
}

.fallback-card {
  width: min(380px, 100%);
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  padding: var(--sp-6);
  text-align: center;
}

.fallback-title {
  font-size: 18px;
  font-weight: 650;
}

.fallback-sub {
  font-size: 13px;
  color: var(--text-muted);
  margin-top: var(--sp-2);
}

.fallback-retry {
  margin-top: var(--sp-4);
  padding: var(--sp-2) var(--sp-5);
  font-size: 14px;
  font-weight: 600;
  color: #fff;
  background: var(--accent);
  border-radius: var(--r-sm);
}

.fallback-retry:hover {
  background: var(--accent-hover);
}
</style>
