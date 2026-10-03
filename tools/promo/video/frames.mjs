// 抽帧预览(调镜头 / 字幕时用,不必整片重渲)。
// 用法:node frames.mjs <compose.html> <输出目录> <t1> [t2 ...]
import fs from 'node:fs';
import path from 'node:path';

const pw = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const { chromium } = pw.default ?? pw;
const [html, outdir, ...ts] = process.argv.slice(2);
fs.mkdirSync(outdir, { recursive: true });
const browser = await chromium.launch({ args: ['--force-device-scale-factor=1'] });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
page.on('pageerror', (e) => console.error('[pageerror]', e.message));
await page.goto('file://' + path.resolve(html));
await page.evaluate(async () => {
  await document.fonts.ready;
  await Promise.all([...document.images].map((i) => i.decode().catch(() => {})));
});
for (const t of ts) {
  await page.evaluate((t) => window.render(t), Number(t));
  await page.screenshot({ path: path.join(outdir, `t${Number(t).toFixed(2).padStart(6, '0')}.png`) });
}
await browser.close();
console.log(`wrote ${ts.length} frames → ${outdir}`);
