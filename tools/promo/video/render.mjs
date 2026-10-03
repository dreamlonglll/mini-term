// 逐帧渲染合成页 → ffmpeg(高质量母版)。
// 用法:node render.mjs <compose.html> <out.mp4> [fps] [from秒] [to秒]
// 所有动画都是 t 的纯函数(scenes.js 的 window.render),逐帧截图结果确定可复现。
// Playwright 走动态 import:装在项目外时用 PLAYWRIGHT_MODULE 指到它的 index.mjs。
import { spawn } from 'node:child_process';
import path from 'node:path';

const pw = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const { chromium } = pw.default ?? pw;
const [html, out, fpsArg, fromArg, toArg] = process.argv.slice(2);
const fps = Number(fpsArg ?? 30);

const browser = await chromium.launch({ args: ['--force-device-scale-factor=1'] });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
page.on('pageerror', (e) => console.error('[pageerror]', e.message));
await page.goto('file://' + path.resolve(html));
await page.evaluate(async () => {
  await document.fonts.ready;
  await Promise.all([...document.images].map((i) => i.decode().catch(() => {})));
});
const duration = await page.evaluate(() => window.DURATION);
const from = Number(fromArg ?? 0), to = Number(toArg ?? duration);
const n = Math.round((to - from) * fps);

const ff = spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'image2pipe', '-framerate', String(fps), '-c:v', 'png', '-i', '-',
  '-c:v', 'libx264', '-preset', 'medium', '-crf', '14', '-pix_fmt', 'yuv420p', out], { stdio: ['pipe', 'inherit', 'inherit'] });
const t0 = Date.now();
for (let i = 0; i < n; i++) {
  const t = from + i / fps;
  await page.evaluate((t) => window.render(t), t);
  const buf = await page.screenshot({ type: 'png' });
  if (!ff.stdin.write(buf)) await new Promise((r) => ff.stdin.once('drain', r));
  if (i % 150 === 0) console.log(`frame ${i}/${n}  t=${t.toFixed(2)}s  ${((Date.now() - t0) / (i + 1)).toFixed(0)}ms/frame`);
}
ff.stdin.end();
await new Promise((r) => ff.on('close', r));
await browser.close();
console.log(`done: ${n} frames in ${((Date.now() - t0) / 1000).toFixed(0)}s → ${out}`);
