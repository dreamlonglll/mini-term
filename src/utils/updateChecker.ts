const GITHUB_REPO = 'dreamlonglll/mini-term';

export interface ReleaseInfo {
  version: string;
  url: string;
  publishedAt: string;
}

export function compareVersions(a: string, b: string): number {
  const pa = a.replace(/^v/, '').split('.').map(Number);
  const pb = b.replace(/^v/, '').split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const diff = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

export async function checkForUpdate(currentVersion: string): Promise<ReleaseInfo | null> {
  const resp = await fetch(`https://api.github.com/repos/${GITHUB_REPO}/releases/latest`);
  if (!resp.ok) throw new Error(resp.status === 404 ? '暂无发布版本' : `请求失败 (${resp.status})`);
  const data = await resp.json();
  const release: ReleaseInfo = {
    version: data.tag_name,
    url: data.html_url,
    publishedAt: data.published_at,
  };
  return compareVersions(release.version, currentVersion) > 0 ? release : null;
}
