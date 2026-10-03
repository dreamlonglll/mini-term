// Mini-Term 宣传片时间轴。所有画面元素都是 t(秒)的纯函数 —— render(t) 可任意跳帧。
// 截图来自 e2e(4K,逻辑坐标 1920×1080);区域坐标一律写逻辑像素。
const BH = (n) => `assets/bh/${n}.png`;
const MM = (n) => `assets/mm/${n}.png`;
const W = 1920, H = 1080;

const root = $('#scenes');
const SCENES = [];

function el(tag, cls, parent, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html !== undefined) e.innerHTML = html;
  (parent || root).appendChild(e);
  return e;
}
function scene(start, end, build) {
  const node = el('div', 'scene');
  const s = { start, end, node, update: () => {} };
  s.update = build(node, s) || s.update;
  SCENES.push(s);
  return s;
}

// ---------------------------------------------------------------- 镜头
// cam 关键帧:[t, cx, cy, s];s 在对数空间插值(推拉匀速),中心点自动夹在画面内
function camAt(t, frames) {
  if (t <= frames[0][0]) return frames[0].slice(1);
  for (let i = 1; i < frames.length; i++) {
    const [t1, x1, y1, s1, ease] = frames[i];
    const [t0, x0, y0, s0] = frames[i - 1];
    if (t <= t1) {
      const k = E[ease || 'inOutCubic']((t - t0) / (t1 - t0));
      return [lerp(x0, x1, k), lerp(y0, y1, k), Math.exp(lerp(Math.log(s0), Math.log(s1), k))];
    }
  }
  return frames[frames.length - 1].slice(1);
}
function clampCam([cx, cy, s]) {
  const hw = W / 2 / s, hh = H / 2 / s;
  return [clamp(cx, hw, W - hw), clamp(cy, hh, H - hh), s];
}
function applyCam(cam, c) {
  const [cx, cy, s] = clampCam(c);
  cam.style.transform = `translate(${W / 2 - s * cx}px, ${H / 2 - s * cy}px) scale(${s})`;
  return [cx, cy, s];
}
// 逻辑坐标 → 屏幕坐标(给高亮框 / 光标用)
function toScreen(c, x, y) {
  const [cx, cy, s] = clampCam(c);
  return [W / 2 + s * (x - cx), H / 2 + s * (y - cy)];
}
function shot(parent, src, cls = '') {
  const frame = el('div', 'frame ' + cls, parent);
  const cam = el('div', 'cam', frame);
  const img = el('img', 'shot', cam);
  img.src = src;
  return { frame, cam, img };
}
function highlight(parent, cls = '') {
  const h = el('div', 'hl ' + cls, parent);
  h.place = (c, [x0, y0, x1, y1], alpha, pad = 8) => {
    const [a, b] = toScreen(c, x0, y0), [d, e] = toScreen(c, x1, y1);
    css(h, { left: a - pad + 'px', top: b - pad + 'px', width: d - a + 2 * pad + 'px', height: e - b + 2 * pad + 'px', opacity: alpha });
  };
  return h;
}

// ---------------------------------------------------------------- 字幕
function caption(parent, { kicker, title, sub, pos = '' }) {
  const c = el('div', 'cap ' + pos, parent);
  if (kicker) el('div', 'kicker', c, kicker);
  const t = el('div', 'title', c);
  const chars = [...title].map((ch) => el('span', 'ch', t, ch === ' ' ? '&nbsp;' : ch));
  const s = sub ? el('div', 'sub', c, sub) : null;
  c.show = (lt, inAt, outAt) => {
    const pIn = prog(lt, inAt, inAt + 0.55, 'outCubic');
    const pOut = prog(lt, outAt - 0.4, outAt, 'inCubic');
    const a = pIn * (1 - pOut);
    css(c, { opacity: a, transform: `translateX(${(1 - pIn) * -36 + pOut * -20}px)` });
    chars.forEach((ch, i) => {
      const k = prog(lt, inAt + 0.12 + i * 0.028, inAt + 0.5 + i * 0.028, 'outCubic');
      css(ch, { opacity: k, transform: `translateY(${(1 - k) * 22}px)`, filter: `blur(${(1 - k) * 6}px)` });
    });
    if (s) {
      const k = prog(lt, inAt + 0.35, inAt + 0.9, 'outCubic');
      css(s, { opacity: k, transform: `translateY(${(1 - k) * 10}px)` });
    }
  };
  return c;
}

function cursor(parent) {
  const c = el('div', 'cursor', parent,
    '<svg viewBox="0 0 24 24" width="34" height="34"><path d="M4 2 L4 19 L8.6 14.8 L11.6 21.6 L14.6 20.3 L11.7 13.6 L18 13.6 Z" fill="#fff" stroke="#111" stroke-width="1.4" stroke-linejoin="round"/></svg>');
  const r = el('div', 'ripple', parent);
  c.at = (x, y, alpha) => css(c, { left: x - 4 + 'px', top: y - 2 + 'px', opacity: alpha });
  c.click = (x, y, k) => css(r, { left: x + 'px', top: y + 'px', opacity: k > 0 && k < 1 ? 1 - k : 0, transform: `scale(${0.3 + k * 1.2})` });
  return c;
}

// =================================================================== 时间轴
// 片头 0–5.2
scene(0, 5.2, (n) => {
  const box = el('div', 'center', n);
  const logo = el('img', 'logo', box); logo.src = 'assets/icon.png';
  const wm = el('div', 'wordmark', box, 'Mini<span class="dash">-</span>Term');
  const tag = el('div', 'tagline', box, '为 AI 时代打造的桌面终端管理器');
  const en = el('div', 'tagline-en', box, 'A desktop terminal manager built for the AI era');
  const chips = el('div', 'chips', box);
  const chipEls = [['GPUI', '原生渲染'], ['AI', '状态感知'], ['多项目', '· 多标签 · 分屏'], ['SSH', '远程项目']]
    .map(([b, t]) => el('div', 'chip', chips, `<b>${b}</b>${t}`));
  return (t) => {
    const out = prog(t, 4.5, 5.2, 'inCubic');
    const pl = prog(t, 0.2, 1.0, 'outBack');
    css(logo, { opacity: prog(t, 0.2, 0.7), transform: `scale(${lerp(0.55, 1, pl)}) translateY(${(1 - prog(t, 0.2, 1, 'outCubic')) * 30}px)`, filter: `blur(${(1 - prog(t, 0.2, 0.8)) * 12}px)` });
    const pw = prog(t, 0.7, 1.6, 'outExpo');
    css(wm, { opacity: pw, letterSpacing: `${lerp(0.28, -0.02, pw)}em` });
    const pt = prog(t, 1.3, 2.0, 'outCubic');
    css(tag, { opacity: pt, transform: `translateY(${(1 - pt) * 24}px)` });
    const pe = prog(t, 1.6, 2.3, 'outCubic');
    css(en, { opacity: pe, transform: `translateY(${(1 - pe) * 16}px)` });
    chipEls.forEach((c, i) => {
      const k = prog(t, 2.2 + i * 0.14, 2.8 + i * 0.14, 'outBack');
      css(c, { opacity: clamp(k), transform: `translateY(${(1 - k) * 24}px) scale(${lerp(0.9, 1, k)})` });
    });
    css(box, { opacity: 1 - out, transform: `scale(${1 + out * 0.12})`, filter: `blur(${out * 8}px)` });
  };
});

// 主视觉:窗口从下方翻起 4.6–11
scene(4.6, 11.0, (n) => {
  const s = shot(n, BH('hero'), 'windowed');
  const cap = caption(n, { kicker: 'Mini-Term', title: '一个窗口，管住所有项目与 AI 会话', sub: 'One window for every project and every AI session — tabs, recursive splits, native GPUI' });
  return (t) => {
    const up = prog(t, 0, 1.6, 'outCubic');
    const full = prog(t, 3.9, 5.6, 'inOutCubic');
    const sc = lerp(lerp(0.62, 0.8, up), 1, full);
    css(s.frame, {
      opacity: prog(t, 0, 0.5),
      transform: `perspective(2200px) translateY(${(1 - up) * 340}px) rotateX(${(1 - up) * 26}deg) scale(${sc})`,
      borderRadius: `${lerp(16, 0, full)}px`,
    });
    applyCam(s.cam, [960, 540, 1]);
    cap.show(t, 1.2, 5.3);
  };
});

// 01 项目分组 + 后台完成提醒 11–16.5
scene(11.0, 16.6, (n) => {
  const s = shot(n, BH('hero-toast'));
  const h1 = highlight(n), h2 = highlight(n, 'green'), h3 = highlight(n, 'green');
  const cap = caption(n, { kicker: '01 · Projects', title: '项目分组，AI 状态一眼看全', sub: 'Every project row shows its AI sessions: working · done · needs you', pos: 'right' });
  const cap2 = caption(n, { kicker: '01 · Projects', title: '后台项目跑完，立刻提醒你', sub: 'An AI finished in another project? A toast pops up — click to jump there', pos: 'top' });
  const cams = [[0, 960, 540, 1], [1.4, 372, 210, 2.55], [3.3, 372, 210, 2.55], [4.4, 1520, 960, 2.35], [5.6, 1520, 960, 2.42, 'linear']];
  return (t) => {
    const c = camAt(t, cams);
    applyCam(s.cam, c);
    css(s.frame, { opacity: prog(t, 0, 0.35) });
    h1.place(c, [46, 93, 376, 125], prog(t, 1.4, 1.8) * (1 - prog(t, 3.1, 3.4)), 4);
    h2.place(c, [46, 127, 376, 158], prog(t, 1.7, 2.1) * (1 - prog(t, 3.1, 3.4)), 4);
    h3.place(c, [1628, 1006, 1905, 1064], prog(t, 4.6, 5.0), 6);
    cap.show(t, 1.3, 3.5);
    cap2.show(t, 4.3, 5.7);
  };
});

// 02 AI 状态三态卡 16.6–23
scene(16.6, 23.1, (n) => {
  const s = shot(n, BH('hero'));
  const cards = el('div', 'states', n);
  const defs = [
    { crop: [380, 37, 2.0], color: '#7aa2ff', icon: '◌', t1: '在跑', t2: 'Working · spinner on tab & project row' },
    { crop: [1253, 37, 2.0], color: '#8ddfa8', icon: '✓', t1: '做完', t2: 'Done · green check + unseen dot' },
    { crop: [1253, 561, 2.0], color: '#ff9a62', icon: '!', t1: '等你处理', t2: 'Needs you · permission or question' },
  ];
  const cardEls = defs.map((d) => {
    const c = el('div', 'state', cards);
    const crop = el('div', 'crop', c);
    const img = el('img', '', crop); img.src = BH('hero');
    css(img, { transform: `scale(${d.crop[2]}) translate(${-d.crop[0]}px, ${-d.crop[1]}px)` });
    el('div', 'label', c, `<div class="dot" style="background:${d.color}">${d.icon}</div><div><div class="t1">${d.t1}</div><div class="t2">${d.t2}</div></div>`);
    return c;
  });
  const cap = caption(n, { kicker: '02 · AI Status', title: '在跑、做完、等你批准，一眼分清', sub: 'Native hooks for Claude Code · Codex · Grok Build · oh-my-pi' });
  return (t) => {
    const blur = prog(t, 0, 0.8);
    applyCam(s.cam, camAt(t, [[0, 960, 540, 1], [6.5, 960, 540, 1.06, 'linear']]));
    css(s.frame, { filter: `blur(${blur * 14}px) brightness(${1 - blur * 0.55})` });
    cardEls.forEach((c, i) => {
      const k = prog(t, 0.5 + i * 0.22, 1.2 + i * 0.22, 'outBack');
      const o = 1 - prog(t, 6.0, 6.5, 'inCubic');
      css(c, { opacity: clamp(k) * o, transform: `translateY(${(1 - k) * 60}px) scale(${lerp(0.92, 1, clamp(k))})` });
    });
    cap.show(t, 1.0, 6.4);
  };
});

// 03 Git 23.1–28.2
scene(23.1, 28.2, (n) => {
  const s = shot(n, BH('git'));
  const h = highlight(n, 'violet');
  const cap = caption(n, { kicker: '03 · Git', title: 'VS Code 风格的 Git 面板', sub: 'Staged / unstaged / untracked · commit graph · worktree management' });
  const cams = [[0, 960, 540, 1.0], [1.2, 1690, 330, 1.9], [3.0, 1690, 330, 1.9], [4.2, 1690, 820, 1.9], [5.1, 1690, 830, 1.93, 'linear']];
  return (t) => {
    const c = camAt(t, cams);
    applyCam(s.cam, c);
    css(s.frame, { opacity: prog(t, 0, 0.4) });
    h.place(c, [1466, 140, 1914, 214], prog(t, 1.3, 1.7) * (1 - prog(t, 2.9, 3.2)), 4);
    cap.show(t, 0.9, 5.0);
  };
});

// 04 AI 历史 + 悬停预览 28.2–33
scene(28.2, 33.1, (n) => {
  const a = shot(n, BH('sessions'));
  const b = shot(n, BH('hover'));
  const cap = caption(n, { kicker: '04 · AI History', title: '按项目归档的 AI 历史会话', sub: 'Claude / Codex sessions per project — hover a project to peek at its live terminals' });
  return (t) => {
    applyCam(a.cam, camAt(t, [[0, 960, 540, 1.0], [1.2, 1690, 420, 1.75], [2.6, 1690, 470, 1.78, 'linear']]));
    css(a.frame, { opacity: prog(t, 0, 0.4) });
    const sw = prog(t, 2.5, 3.0);
    applyCam(b.cam, camAt(t, [[2.5, 640, 330, 1.9], [4.9, 640, 320, 2.15, 'linear']]));
    css(b.frame, { opacity: sw });
    cap.show(t, 0.8, 4.8);
  };
});

// 05 使用统计 33.1–38.6
scene(33.1, 38.6, (n) => {
  const s = shot(n, BH('usage'));
  const cap = caption(n, { kicker: '05 · Usage', title: '成本、调用、会话，多维聚合', sub: 'Cost, calls & sessions across Claude Code / Codex / Grok — trends and rankings' });
  const cams = [[0, 960, 560, 1.12], [1.4, 760, 300, 1.75], [3.0, 1180, 520, 1.6], [5.5, 1080, 560, 1.45, 'linear']];
  return (t) => {
    applyCam(s.cam, camAt(t, cams));
    css(s.frame, { opacity: prog(t, 0, 0.4) });
    cap.show(t, 0.9, 5.4);
  };
});

// 06 Markdown + Mermaid 38.6–44.2
scene(38.6, 44.2, (n) => {
  const a = shot(n, BH('markdown'));
  const b = shot(n, BH('mermaid'));
  const cap = caption(n, { kicker: '06 · Preview', title: 'Markdown 预览，Mermaid 纯 Rust 渲染', sub: 'Diagrams, tables & task lists rendered in pure Rust — click a diagram to zoom' });
  return (t) => {
    applyCam(a.cam, camAt(t, [[0, 960, 540, 1.0], [1.4, 1148, 420, 1.75], [2.8, 1148, 410, 1.85, 'linear']]));
    css(a.frame, { opacity: prog(t, 0, 0.4) });
    const sw = prog(t, 2.8, 3.3);
    applyCam(b.cam, camAt(t, [[2.8, 1450, 350, 1.25], [5.6, 1420, 330, 1.4, 'linear']]));
    css(b.frame, { opacity: sw });
    cap.show(t, 0.8, 5.4);
  };
});

// 快切:全局搜索 / 命令库 / 项目切换 / SSH 44.2–52.2
const MONTAGE = [
  { src: BH('search'), cam: [968, 556, 1.45], kicker: 'Search', title: '文件名 / 内容双模式搜索', sub: 'Filename & content search · regex · streamed and cancellable' },
  { src: BH('commands'), cam: [960, 330, 2.1], kicker: 'Commands', title: '常用命令，一键敲进终端', sub: 'Save commands once, type them into any terminal' },
  { src: BH('switcher'), cam: [970, 310, 2.0], kicker: 'Switcher', title: 'Ctrl+Shift+P 秒切项目', sub: 'Jump between projects — status lights included' },
  { src: BH('ssh'), cam: [954, 461, 1.45], kicker: 'SSH', title: 'SSH 远程项目与连接管理', sub: 'SFTP file tree · one-click reconnect · groups · encrypted passwords' },
];
MONTAGE.forEach((m, i) => {
  const t0 = 44.2 + i * 2.0;
  scene(t0, t0 + 2.25, (n) => {
    const s = shot(n, m.src);
    const cap = caption(n, { kicker: m.kicker, title: m.title, sub: m.sub });
    return (t) => {
      const [cx, cy, sc] = m.cam;
      applyCam(s.cam, camAt(t, [[0, cx, cy, sc * 0.93], [2.25, cx, cy, sc * 1.04, 'linear']]));
      css(s.frame, { opacity: prog(t, 0, 0.25) });
      cap.show(t, 0.1, 2.25 + (i === MONTAGE.length - 1 ? 0 : 0.4));
    };
  });
});

// 07 外置主题包:点卡片实时换肤 52.2–60.8
scene(52.2, 60.8, (n) => {
  const a = shot(n, BH('settings-theme'));
  const b = shot(n, BH('settings-switched'));
  const c3 = shot(n, BH('switched-main'));
  const left = shot(n, BH('hero'));
  const right = shot(n, MM('hero'));
  const edge = el('div', 'wipe-edge', n);
  const cur = cursor(n);
  const bl = el('div', 'badge', n, 'Blue Hour 蓝调时分');
  const br = el('div', 'badge', n, 'Morning Mist 晨雾');
  const cap = caption(n, { kicker: '07 · Theme Packs', title: '一键换肤，改文件即热重载', sub: 'Dream Skin–compatible packs, hot-reloaded · bundled: Blue Hour & Morning Mist', pos: 'right' });
  const camA = [[0, 960, 560, 1.15], [1.0, 880, 590, 1.55]];
  return (t) => {
    const ca = camAt(t, camA);
    applyCam(a.cam, ca); applyCam(b.cam, ca);
    css(a.frame, { opacity: prog(t, 0, 0.4) });
    // 光标滑到 Morning Mist 卡上点击
    const mv = prog(t, 0.9, 1.9, 'inOutCubic');
    const [tx, ty] = toScreen(ca, 1035, 560);
    const [sx, sy] = toScreen(ca, 1330, 820);
    cur.at(lerp(sx, tx, mv), lerp(sy, ty, mv), prog(t, 0.8, 1.0) * (1 - prog(t, 3.2, 3.5)));
    cur.click(tx, ty, prog(t, 2.0, 2.6, 'outCubic'));
    // 斜向擦除:蓝调 → 晨雾
    const w = prog(t, 2.05, 2.9, 'inOutCubic');
    const x = lerp(-400, W + 400, w);
    css(b.frame, { opacity: 1, clipPath: `polygon(0 0, ${x}px 0, ${x - 300}px ${H}px, 0 ${H}px)` });
    css(edge, { left: x - 150 + 'px', opacity: w > 0 && w < 1 ? 1 : 0, transform: 'rotate(15.5deg)' });
    // 拉远到整窗(晨雾主界面)
    const z = prog(t, 3.6, 4.6, 'inOutCubic');
    applyCam(c3.cam, camAt(t, [[3.6, 880, 590, 1.55], [4.6, 960, 540, 1.0]]));
    css(c3.frame, { opacity: z });
    // 左右对照:同一工作区,两套皮肤
    const sp = prog(t, 5.2, 6.0, 'inOutCubic');
    const div = lerp(W, W / 2, sp);
    applyCam(left.cam, [960, 540, 1]); applyCam(right.cam, [960, 540, 1]);
    css(left.frame, { opacity: sp > 0 ? 1 : 0, clipPath: `inset(0 ${W - div}px 0 0)` });
    css(right.frame, { opacity: sp > 0 ? 1 : 0, clipPath: `inset(0 0 0 ${div}px)` });
    const ba = prog(t, 5.8, 6.3);
    css(bl, { left: '60px', top: '60px', opacity: ba });
    css(br, { right: '60px', top: '60px', opacity: ba });
    cap.show(t, 0.6, 8.6);
  };
});

// 晨雾快切 60.8–65
const MIST = [['markdown', [1100, 520, 1.25]], ['usage', [960, 540, 1.12]], ['git', [1500, 560, 1.3]]];
MIST.forEach(([name, cam], i) => {
  const t0 = 60.8 + i * 1.4;
  scene(t0, t0 + 1.65, (n) => {
    const s = shot(n, MM(name));
    return (t) => {
      applyCam(s.cam, camAt(t, [[0, cam[0], cam[1], cam[2]], [1.65, cam[0], cam[1], cam[2] * 1.06, 'linear']]));
      css(s.frame, { opacity: prog(t, 0, 0.25) });
    };
  });
});
// 晨雾字幕跨三张快切常驻:单独挂一层
scene(60.8, 65.0, (n) => {
  const cap = caption(n, { kicker: 'Morning Mist 晨雾', title: '浅色皮肤，同样好用', sub: 'Background art, panel opacity and terminal palette — all defined by the skin' });
  return (t) => cap.show(t, 0.15, 4.2);
});

// 技术卖点 65–69.4
scene(65.0, 69.4, (n) => {
  const s = shot(n, BH('hero'));
  const box = el('div', 'facts', n);
  const big = el('div', 'big', box, 'Rust 原生 · <em>GPU 渲染</em> · 单进程零 IPC<div class="big-en">Native Rust · GPU-rendered · single process, zero IPC</div>');
  const row = el('div', 'row', box);
  const facts = [
    ['零 IPC', 'PTY 字节直喂 VT 状态机，刷屏也拖不垮界面<br><span class="en">In-process VT parsing — floods never freeze the UI</span>'],
    ['零网络', '启动不发任何网络请求，没有 Web 资源<br><span class="en">Zero network requests at startup</span>'],
    ['2165<small>个测试</small>', '33 个测试目标守着每一次发版<br><span class="en">2,165 Rust tests across 33 targets</span>'],
  ].map(([nn, d]) => el('div', 'fact', row, `<div class="n">${nn}</div><div class="d">${d}</div>`));
  return (t) => {
    applyCam(s.cam, camAt(t, [[0, 960, 540, 1.08], [4.4, 960, 540, 1.16, 'linear']]));
    css(s.frame, { opacity: prog(t, 0, 0.5), filter: 'blur(16px) brightness(0.38)' });
    const o = 1 - prog(t, 3.9, 4.4);
    const pb = prog(t, 0.3, 1.0, 'outCubic');
    css(big, { opacity: pb * o, transform: `translateY(${(1 - pb) * 30}px)` });
    facts.forEach((f, i) => {
      const k = prog(t, 0.9 + i * 0.2, 1.6 + i * 0.2, 'outBack');
      css(f, { opacity: clamp(k) * o, transform: `translateY(${(1 - k) * 40}px)` });
    });
  };
});

// 片尾 69.4–74.4
scene(69.4, 74.4, (n) => {
  const box = el('div', 'center', n);
  const logo = el('img', 'logo', box); logo.src = 'assets/icon.png';
  const wm = el('div', 'wordmark', box, 'Mini<span class="dash">-</span>Term');
  const tag = el('div', 'tagline', box, '为 AI 时代打造的桌面终端管理器<div class="tagline-en">A desktop terminal manager built for the AI era</div>');
  const url = el('div', 'url', box, 'github.com/<b>dreamlonglll/mini-term</b>');
  const plat = el('div', 'plat', box, 'Windows · macOS · Linux　|　MIT 开源');
  return (t) => {
    const k = (a, b) => prog(t, a, b, 'outCubic');
    css(logo, { opacity: k(0.1, 0.6), transform: `scale(${lerp(0.7, 1, prog(t, 0.1, 0.9, 'outBack'))})` });
    css(wm, { opacity: k(0.4, 1.0), transform: `translateY(${(1 - k(0.4, 1.0)) * 20}px)` });
    css(tag, { opacity: k(0.7, 1.3), transform: `translateY(${(1 - k(0.7, 1.3)) * 20}px)` });
    css(url, { opacity: k(1.1, 1.7), transform: `translateY(${(1 - k(1.1, 1.7)) * 20}px)` });
    css(plat, { opacity: k(1.4, 2.0) });
    css(box, { opacity: 1 - prog(t, 4.4, 5.0) });
  };
});

window.DURATION = 74.4;
const TAIL = 0.6;

// ---------------------------------------------------------------- 渲染
const art = $('#art'), gA = $('#glowA'), gB = $('#glowB'), vig = $('#vig');
art.style.backgroundImage = 'url(assets/bh-bg.jpg)';
window.render = (T) => {
  css(art, { transform: `scale(${1.04 + 0.05 * Math.sin(T / 9)}) translate(${Math.sin(T / 7) * 18}px, ${Math.cos(T / 8) * 12}px)` });
  css(gA, { left: `${380 + Math.sin(T / 4.2) * 260}px`, top: `${-260 + Math.cos(T / 5.1) * 160}px` });
  css(gB, { left: `${980 + Math.cos(T / 4.8) * 300}px`, top: `${300 + Math.sin(T / 3.9) * 180}px` });
  // 浅色画面(晨雾皮肤)上暗角会变成脏灰光晕 → 换肤后到晨雾快切结束一律关掉
  const light = prog(T, 54.1, 54.6) * (1 - prog(T, 65.0, 65.5));
  css(vig, { opacity: 0.85 * (1 - light) });
  for (const s of SCENES) {
    // 每个场景在结束后多留 TAIL 秒垫底,下一个场景在它上面淡入 → 交叉溶解
    const on = T >= s.start && T <= s.end + TAIL;
    s.node.style.display = on ? 'block' : 'none';
    if (on) s.update(T - s.start);
  }
};
window.render(0);
