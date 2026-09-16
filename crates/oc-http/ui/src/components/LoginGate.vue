<script setup>
import { ref } from 'vue'
import { login } from '../lib/api.js'

const emit = defineEmits(['authed'])

const token = ref('')
const error = ref('')
const busy = ref(false)
const masked = ref(true)

// 网关地址：展示给操作员，确认自己连的是哪台机器。
const origin = window.location.host || window.location.hostname

async function submit() {
  const value = token.value.trim()
  if (!value) {
    error.value = '请输入访问令牌。'
    return
  }
  busy.value = true
  error.value = ''
  try {
    await login(value)
    // token 只在这一刻经 POST 提交，换取 HttpOnly cookie 后即弃。
    token.value = ''
    emit('authed')
  } catch (e) {
    error.value = e.message || '令牌无效，无法连接。'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <main class="gate">
    <section class="card">
      <div class="beacon" :data-state="busy ? 'handshaking' : error ? 'error' : 'idle'" aria-hidden="true"></div>

      <p class="eyebrow">oh-my-claw · 网关</p>
      <h1 class="title">解锁控制台</h1>
      <p class="sub">这台网关需要访问令牌。令牌只在启动时由你设置，不写入配置文件。</p>

      <div class="endpoint">
        <span class="endpoint-label">目标</span>
        <span class="endpoint-addr">{{ origin }}</span>
      </div>

      <form class="form" novalidate @submit.prevent="submit">
        <label class="field-label" for="token">访问令牌</label>
        <div class="field">
          <input
            id="token"
            v-model="token"
            :type="masked ? 'password' : 'text'"
            autocomplete="off"
            spellcheck="false"
            placeholder="粘贴 --token / OC_HTTP_TOKEN"
            aria-label="访问令牌"
          >
          <button type="button" class="reveal" @click="masked = !masked">{{ masked ? '显示' : '隐藏' }}</button>
        </div>
        <p class="error" v-if="error">{{ error }}</p>
        <button type="submit" class="submit" :disabled="busy">{{ busy ? '握手…' : '建立连接' }}</button>
      </form>

      <details class="where">
        <summary>令牌从哪来？</summary>
        <div class="code">oc http --token <em>你的令牌</em> --bind 0.0.0.0</div>
        <div class="code">OC_HTTP_TOKEN=<em>你的令牌</em> oc http --bind 0.0.0.0</div>
      </details>
    </section>
  </main>
</template>

<style scoped>
.gate {
  height: 100dvh;
  display: grid;
  place-items: center;
  padding: var(--sp-5);
  background:
    radial-gradient(1200px 500px at 50% -10%, var(--accent-bg), transparent 60%),
    var(--surface);
}

.card {
  width: min(400px, 100%);
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  padding: var(--sp-6) var(--sp-6) var(--sp-5);
  box-shadow: 0 24px 60px -32px hsl(220 12% 10% / 0.25);
}

/* 心跳信标：灰=待命 / 蓝脉冲=握手 / 红=拒绝 */
.beacon {
  --beacon-color: var(--n-3);
  position: relative;
  width: 14px; height: 14px;
  border-radius: 50%;
  margin-bottom: var(--sp-5);
}
.beacon::before {
  content: "";
  position: absolute; inset: 0;
  border-radius: 50%;
  background: var(--beacon-color);
}
.beacon::after {
  content: "";
  position: absolute; inset: -7px;
  border-radius: 50%;
  border: 1px solid transparent;
}
.beacon[data-state="idle"]::after { border-color: var(--border); }
.beacon[data-state="handshaking"] { --beacon-color: var(--accent); }
.beacon[data-state="handshaking"]::after {
  border-color: var(--accent-ring);
  animation: pulse 1.1s ease-out infinite;
}
.beacon[data-state="error"] { --beacon-color: var(--danger); }

@keyframes pulse {
  0%   { transform: scale(1);   opacity: 1; }
  100% { transform: scale(2.2); opacity: 0; }
}

.eyebrow {
  font-family: var(--font-mono);
  font-size: 11px;
  letter-spacing: 0.08em;
  color: var(--text-muted);
  text-transform: uppercase;
}
.title {
  font-size: 20px;
  font-weight: 650;
  letter-spacing: -0.01em;
  margin-top: var(--sp-1);
}
.sub {
  font-size: 13px;
  color: var(--text-muted);
  margin-top: var(--sp-2);
}

.endpoint {
  display: flex;
  align-items: baseline;
  gap: var(--sp-2);
  font-family: var(--font-mono);
  font-size: 12px;
  margin-top: var(--sp-4);
  padding: var(--sp-2) var(--sp-3);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  color: var(--text-muted);
}
.endpoint-addr { color: var(--text); }
.endpoint-addr::before { content: "→ "; color: var(--accent); }

.form { margin-top: var(--sp-4); }
.field-label {
  display: block;
  font-size: 12px;
  font-weight: 600;
  color: var(--text);
  margin-bottom: var(--sp-1);
}
.field {
  display: flex;
  align-items: center;
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  background: var(--surface);
}
.field:focus-within { border-color: var(--accent); }
.field input {
  flex: 1;
  min-width: 0;
  font-family: var(--font-mono);
  font-size: 13px;
  letter-spacing: 0.02em;
  color: var(--text);
  background: none;
  border: none;
  padding: var(--sp-2) var(--sp-3);
}
.field input:focus-visible { outline: none; }
.field input::placeholder { color: var(--n-3); letter-spacing: 0; }
.reveal {
  flex-shrink: 0;
  padding: 0 var(--sp-3);
  font-size: 12px;
  color: var(--text-muted);
}
.reveal:hover { color: var(--text); }

.error { font-size: 12px; color: var(--danger); margin-top: var(--sp-2); }

.submit {
  width: 100%;
  margin-top: var(--sp-4);
  padding: var(--sp-2) var(--sp-4);
  font-size: 14px;
  font-weight: 600;
  color: #fff;
  background: var(--accent);
  border-radius: var(--r-sm);
  transition: background 120ms ease;
}
.submit:hover:not(:disabled) { background: var(--accent-hover); }
.submit:disabled { opacity: 0.6; cursor: default; }

.where {
  margin-top: var(--sp-4);
  border-top: 1px solid var(--border);
  padding-top: var(--sp-3);
}
.where summary {
  cursor: pointer;
  font-size: 12px;
  color: var(--text-muted);
  user-select: none;
}
.where summary:hover { color: var(--text); }
.code {
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 1.8;
  color: var(--text-muted);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  padding: var(--sp-2) var(--sp-3);
  margin-top: var(--sp-2);
  white-space: nowrap;
  overflow-x: auto;
}
.code em { color: var(--accent); font-style: normal; }

@media (prefers-reduced-motion: reduce) {
  .beacon[data-state="handshaking"]::after { animation: none; opacity: 0; }
  .submit { transition: none; }
}
</style>
