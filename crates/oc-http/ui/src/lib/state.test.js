import { test } from 'node:test'
import assert from 'node:assert/strict'
import { historyMaxSeq, applyToolEvent, messagesFor } from './state.js'

test('historyMaxSeq 取最大 seq', () => {
  assert.equal(historyMaxSeq([{ seq: 1 }, { seq: 3 }, { seq: 2 }]), 3)
})

test('historyMaxSeq 空历史为 0（=全量回放）', () => {
  assert.equal(historyMaxSeq([]), 0)
})

test('historyMaxSeq 忽略缺 seq 的条目', () => {
  // 防御性：少一条 seq 不该让水位变成 NaN——那会让 since_seq 序列化成 null，
  // 服务端 serde(default) 兜成 0，悄悄退回全量回放（重复渲染复发）。
  assert.equal(historyMaxSeq([{ seq: 5 }, {}]), 5)
})

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
