<script setup>
import { computed } from 'vue'
import { status, activeChats } from '../lib/state.js'

// Rough progress: input tokens / context window, capped at 100%.
const pct = computed(() =>
  status.context_window > 0
    ? Math.min(100, Math.round((status.last_input_tokens ?? 0) / status.context_window * 100))
    : 0
)

const pctColor = computed(() =>
  pct.value >= 85 ? 'var(--danger)'
  : pct.value >= 60 ? 'var(--warn)'
  : 'var(--success)'
)

// 本 Web 客户端自己发起的 run 是否还在进行。不用 status.active_run：
// 那个字段只在 ambient SSE 开场 snapshot 赋值一次，之后永不更新（死指示灯）。
const isRunning = computed(() => activeChats.size > 0)
</script>

<template>
  <footer class="statusbar" role="status" aria-label="状态栏">
    <span class="pill">
      {{ status.provider || '—' }} · {{ status.model || '—' }}
    </span>
    <span
      v-if="status.last_input_tokens != null && status.context_window > 0"
      class="tokens"
      :title="`${status.last_input_tokens} / ${status.context_window} tokens`"
    >
      <span class="bar-wrap" aria-hidden="true">
        <span class="bar" :style="{ width: `${pct}%`, background: pctColor }"></span>
      </span>
      <span class="token-label">{{ pct }}% ctx</span>
    </span>
    <span v-if="isRunning" class="chip running">
      <span class="dot" aria-hidden="true"></span>
      生成中
    </span>
    <span v-else-if="status.queued_turns > 0" class="chip queued">{{ status.queued_turns }} 个等待</span>
  </footer>
</template>

<style scoped>
.statusbar {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  padding: var(--sp-2) var(--sp-4);
  border-top: 1px solid var(--border);
  background: var(--surface-raised);
  font-size: 11px;
  color: var(--text-muted);
  flex-wrap: wrap;
  min-height: 32px;
}

.pill {
  font-variant-numeric: tabular-nums;
}

.tokens {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
}

.bar-wrap {
  width: 64px;
  height: 4px;
  background: var(--border);
  border-radius: 2px;
  overflow: hidden;
}

.bar {
  height: 100%;
  border-radius: 2px;
  transition: width 400ms ease, background 400ms ease;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: var(--sp-1);
  padding: 2px var(--sp-2);
  border-radius: 10px;
  font-size: 11px;
}

.running {
  background: color-mix(in srgb, var(--accent) 12%, transparent);
  color: var(--accent);
}

.queued {
  background: color-mix(in srgb, var(--warn) 15%, transparent);
  color: var(--warn);
}

.dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
  animation: pulse 1.4s ease infinite;
}

@keyframes pulse {
  0%, 100% { opacity: 1; }
  50%       { opacity: 0.3; }
}

@media (prefers-reduced-motion: reduce) {
  .dot { animation: none; }
  .bar { transition: none; }
}
</style>
