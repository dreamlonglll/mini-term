import { t } from "../i18n";

const GITHUB_REPO = 'dreamlonglll/mini-term';

export interface ReleaseInfo {
  version: string;
  url: string;
  publishedAt: string;
}

/** 预发布判定：去 `v` 前缀与 `+` build metadata 后含 `-` 即预发布（如 v1.0.0-beta）。 */
export function isPrerelease(version: string): boolean {
  return version.trim().replace(/^v/i, '').split('+')[0].includes('-');
}

/**
 * 预发布段比较（SemVer §11.4）：按 `.` 拆标识符逐个比，纯数字按数值且恒小于
 * 字母数字标识符，其余按 ASCII 字典序；前缀相同时段多者大（beta < beta.2 < rc）。
 *
 * 与 GPUI 侧 `settings.rs` 的 `compare_prerelease` 同构，单测在那边钉死
 * （本文件因 i18n 导入不进 tsconfig.test.json 的纯函数编译清单）。
 */
function comparePrerelease(a: string, b: string): number {
  const pa = a.split('.');
  const pb = b.split('.');
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const l = pa[i];
    const r = pb[i];
    if (l === undefined) return -1;
    if (r === undefined) return 1;
    const ln = /^\d+$/.test(l) ? Number(l) : null;
    const rn = /^\d+$/.test(r) ? Number(r) : null;
    let diff: number;
    if (ln !== null && rn !== null) diff = ln - rn;
    else if (ln !== null) diff = -1;
    else if (rn !== null) diff = 1;
    else diff = l < r ? -1 : l > r ? 1 : 0;
    if (diff !== 0) return diff;
  }
  return 0;
}

/**
 * 语义版本比较：去 `v` 前缀，`+` 后的 build metadata 不参与排序；主干按 `.`
 * 分段数值比较、缺段按 0；主干相同时无预发布段 > 有预发布段（1.0.0 > 1.0.0-beta），
 * 两侧都有则按预发布段比较。v1.0.0-beta 起 tag 带预发布段，旧的纯数值比较会把
 * `1.0.0-beta` 与 `1.0.0` 判成同版本，转正提示会哑掉。
 */
export function compareVersions(a: string, b: string): number {
  const parse = (s: string): [number[], string | null] => {
    const noMeta = s.trim().replace(/^v/i, '').split('+')[0];
    const dash = noMeta.indexOf('-');
    const core = dash === -1 ? noMeta : noMeta.slice(0, dash);
    const pre = dash === -1 || dash === noMeta.length - 1 ? null : noMeta.slice(dash + 1);
    return [core.split('.').map((seg) => parseInt(seg, 10) || 0), pre];
  };
  const [pa, preA] = parse(a);
  const [pb, preB] = parse(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const diff = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (diff !== 0) return diff;
  }
  if (preA === null && preB === null) return 0;
  if (preA === null) return 1;
  if (preB === null) return -1;
  return comparePrerelease(preA, preB);
}

/**
 * 频道规则：正式版用户只看正式版（不被红点推去装 beta），预发布（beta）用户
 * 全都看（该被引向下一个 beta 或转正的 stable）。返回频道内版本最大者，
 * 不保证比当前新——新旧由调用方把关。
 */
export function pickLatest(releases: ReleaseInfo[], currentVersion: string): ReleaseInfo | null {
  const includePre = isPrerelease(currentVersion);
  let best: ReleaseInfo | null = null;
  for (const r of releases) {
    if (!includePre && isPrerelease(r.version)) continue;
    if (!best || compareVersions(r.version, best.version) > 0) best = r;
  }
  return best;
}

export async function checkForUpdate(currentVersion: string): Promise<ReleaseInfo | null> {
  // 不用 /releases/latest：那个端点永远不含预发布，发 beta 后 beta 用户会两头
  // 落空——看不到下一个 beta，也等不到转正 stable 前的任何通知。改拉列表，
  // 频道取舍在本地由 pickLatest 决定。
  const resp = await fetch(`https://api.github.com/repos/${GITHUB_REPO}/releases?per_page=20`);
  if (!resp.ok) throw new Error(resp.status === 404 ? t("updateChecker.noRelease") : t("updateChecker.requestFailed", { status: resp.status }));
  const data = await resp.json();
  const releases: ReleaseInfo[] = (Array.isArray(data) ? (data as any[]) : [])
    // 匿名请求本就看不到 draft，这层过滤只是防御性的
    .filter((r) => !r.draft)
    .map((r) => ({ version: r.tag_name, url: r.html_url, publishedAt: r.published_at }));
  const latest = pickLatest(releases, currentVersion);
  return latest && compareVersions(latest.version, currentVersion) > 0 ? latest : null;
}
