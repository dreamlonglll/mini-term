// 极简时间轴引擎:所有动画都是 t(秒)的纯函数,render(t) 可任意跳帧 → 逐帧截图确定性可复现。
const E = {
  linear: (x) => x,
  inQuad: (x) => x * x,
  outQuad: (x) => 1 - (1 - x) * (1 - x),
  inOutQuad: (x) => (x < 0.5 ? 2 * x * x : 1 - Math.pow(-2 * x + 2, 2) / 2),
  outCubic: (x) => 1 - Math.pow(1 - x, 3),
  inCubic: (x) => x * x * x,
  inOutCubic: (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2),
  outExpo: (x) => (x === 1 ? 1 : 1 - Math.pow(2, -10 * x)),
  inOutExpo: (x) => (x === 0 ? 0 : x === 1 ? 1 : x < 0.5 ? Math.pow(2, 20 * x - 10) / 2 : (2 - Math.pow(2, -20 * x + 10)) / 2),
  outBack: (x) => { const c1 = 1.70158, c3 = c1 + 1; return 1 + c3 * Math.pow(x - 1, 3) + c1 * Math.pow(x - 1, 2); },
  inOutSine: (x) => -(Math.cos(Math.PI * x) - 1) / 2,
};
const clamp = (v, a = 0, b = 1) => Math.min(b, Math.max(a, v));
const lerp = (a, b, k) => a + (b - a) * k;
// 关键帧插值:frames = [[t0, v0], [t1, v1, ease?], ...],ease 作用在进入该帧的那一段
function kf(t, frames) {
  if (t <= frames[0][0]) return frames[0][1];
  for (let i = 1; i < frames.length; i++) {
    const [t1, v1, ease] = frames[i];
    const [t0, v0] = frames[i - 1];
    if (t <= t1) {
      const k = (E[ease || 'inOutCubic'])((t - t0) / (t1 - t0));
      return Array.isArray(v0) ? v0.map((x, j) => lerp(x, v1[j], k)) : lerp(v0, v1, k);
    }
  }
  return frames[frames.length - 1][1];
}
// 区间进度:t 在 [a,b] 内从 0→1(带缓动)
const prog = (t, a, b, ease = 'inOutCubic') => E[ease](clamp((t - a) / (b - a)));
// 场景淡入淡出窗口
function win(t, start, end, fin = 0.6, fout = 0.6) {
  if (t < start || t > end) return 0;
  return Math.min(fin ? clamp((t - start) / fin) : 1, fout ? clamp((end - t) / fout) : 1);
}
const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];
function css(el, props) { if (typeof el === 'string') el = $(el); if (el) Object.assign(el.style, props); }
