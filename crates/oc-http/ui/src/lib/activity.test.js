import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createActivity, WAIT_THRESHOLD_MS } from './activity.js';

test('提交后 arm，阈值前不 waiting', () => {
  const a = createActivity();
  a.arm(1000);
  assert.equal(a.state, 'armed');
  a.showWaiting(1000 + WAIT_THRESHOLD_MS - 1);
  assert.equal(a.state, 'waiting');
});

test('reasoning 直接转 thinking，跳过 waiting', () => {
  const a = createActivity();
  a.arm(1000);
  a.reasoning('先想想', 1000 + 100);
  assert.equal(a.state, 'thinking');
  assert.equal(a.thinkingSince, 1100);
  // 期间 showWaiting 不应回退
  a.showWaiting(1000 + WAIT_THRESHOLD_MS);
  assert.equal(a.state, 'thinking');
});

test('reasoning 追加累计，thinkingSince 只在首 delta 设', () => {
  const a = createActivity();
  a.arm(0);
  a.reasoning('甲', 10);
  a.reasoning('乙', 20);
  assert.equal(a.thinkingText, '甲乙');
  assert.equal(a.thinkingSince, 10);
});

test('visible 隐藏并清空 thinkingText', () => {
  const a = createActivity();
  a.arm(0);
  a.reasoning('想想', 10);
  a.visible();
  assert.equal(a.state, 'hidden');
  assert.equal(a.thinkingText, '');
  assert.equal(a.thinkingSince, null);
});

test('toolEnd 回到空窗重计', () => {
  const a = createActivity();
  a.arm(1000);
  a.reasoning('想', 1100);
  a.visible();
  a.toolEnd(2000);
  assert.equal(a.state, 'armed');
  assert.equal(a.emptySince, 2000);
});

test('end 全部归零', () => {
  const a = createActivity();
  a.arm(0);
  a.reasoning('想', 10);
  a.end();
  assert.equal(a.state, 'idle');
  assert.equal(a.thinkingText, '');
  assert.equal(a.emptySince, null);
});
