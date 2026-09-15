/**
 * 单例活动卡的状态机。零依赖、同步纯函数，不持任何定时器——
 * 延迟由调用方用 setTimeout 触发 showWaiting()，这样 node --test 可直接断言
 * 状态迁移，无需 fake timer。
 *
 * state: 'idle' | 'armed' | 'waiting' | 'thinking' | 'hidden'
 *   armed    空窗开始，未达阈值（不渲染）
 *   waiting  空窗超过阈值（渲染「等待模型」）
 *   thinking 收到 reasoning delta（渲染「思考中」）
 *   hidden   本 step 首个可见产物已到（工具/文本接管）
 */

export const WAIT_THRESHOLD_MS = 400;

export function createActivity() {
  return {
    state: 'idle',
    emptySince: null,     // 本次空窗起点
    waitingSince: null,   // waiting 显示起点
    thinkingSince: null,
    thinkingText: '',

    arm(now) {
      this.state = 'armed';
      this.emptySince = now;
      this.waitingSince = null;
      this.thinkingSince = null;
      this.thinkingText = '';
    },

    showWaiting(now) {
      if (this.state !== 'armed') return;   // 已被 reasoning/visible 打断
      this.state = 'waiting';
      this.waitingSince = now;
    },

    reasoning(delta, now) {
      const first = this.state !== 'thinking';
      this.state = 'thinking';
      if (first) this.thinkingSince = now;
      this.thinkingText += delta;
    },

    visible() {
      this.state = 'hidden';
      this.thinkingSince = null;
      this.thinkingText = '';
    },

    toolEnd(now) {
      this.arm(now);
    },

    end() {
      this.state = 'idle';
      this.emptySince = null;
      this.waitingSince = null;
      this.thinkingSince = null;
      this.thinkingText = '';
    },
  };
}
