import { test } from 'node:test'
import assert from 'node:assert/strict'
import { historyMaxSeq } from './state.js'

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
