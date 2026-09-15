<script setup>
import { computed } from 'vue'

/**
 * 单例活动卡：不进 messageMap 的瞬时组件，渲染在消息流尾部。
 * 形态由 act.state 驱动（见 lib/activity.js 的状态机）。
 */
const props = defineProps({
  act: { type: Object, required: true },
})

const show = computed(() => props.act.state === 'waiting' || props.act.state === 'thinking')
const isThinking = computed(() => props.act.state === 'thinking')

// 展示开关：关闭时退化为只有标题 + 计时器（不渲染正文），
// 但卡不消失——「不知道模型在动」就等于退回静默。
const prefs = JSON.parse(localStorage.getItem('oc.liveActivity') || '{}')
const showText = (prefs.showThinking ?? true) === true

const seconds = computed(() => {
  const since = isThinking.value ? props.act.thinkingSince : props.act.waitingSince
  if (since == null) return 0
  return Math.max(0, Math.floor((Date.now() - since) / 1000))
})

const label = computed(() => (isThinking.value ? '💭 思考中' : '等待模型'))
</script>

<template>
  <div v-if="show" class="live" :class="{ waiting: !isThinking }">
    <div class="live-head"><span class="dot"></span>{{ label }} · {{ seconds }}s</div>
    <div v-if="isThinking && showText" class="live-body">{{ act.thinkingText }}<span class="caret"></span></div>
  </div>
</template>

<style scoped>
.live {
  border: 1px dashed color-mix(in srgb, var(--accent) 38%, var(--border));
  border-radius: var(--r-sm);
  background: color-mix(in srgb, var(--accent) 4%, transparent);
  padding: var(--sp-2) var(--sp-3);
  align-self: stretch;
}
.live.waiting { border-style: solid; border-color: var(--border); background: transparent; }
.live-head {
  display: flex; align-items: center; gap: 7px;
  font-size: 12px; font-weight: 600; color: var(--accent);
  font-variant-numeric: tabular-nums;
}
.live.waiting .live-head { color: var(--text-muted); font-weight: 500; }
.live-head .dot {
  width: 6px; height: 6px; border-radius: 50%;
  background: currentColor; animation: pulse 1.1s ease-in-out infinite;
}
.live-body {
  margin-top: 4px; font-size: 13px; line-height: 1.7; color: var(--text-muted);
  max-height: calc(1.7em * 6); overflow-y: auto; scrollbar-width: thin;
}
.live-body::-webkit-scrollbar { width: 4px; }
.live-body::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }
.caret {
  display: inline-block; width: 2px; height: 1em;
  vertical-align: text-bottom; background: var(--accent);
  animation: blink .9s step-end infinite;
}
@keyframes pulse { 0%,100% { opacity:.35; transform: scale(.85) } 50% { opacity:1; transform: scale(1) } }
@keyframes blink { 0%,100% { opacity:1 } 50% { opacity:0 } }
</style>
