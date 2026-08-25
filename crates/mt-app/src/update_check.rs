//! 版本自检与 GitHub release 查询。
//!
//! **与 UI 无关**:这里只有语义版本比较、频道过滤与 GitHub API 调用。
//! 两个消费方 —— `main.rs` 的启动自检([`newer_release`])与设置面板 about
//! 页的手动检查([`fetch_latest_release`])—— 共用同一套判据。
//!
//! 网络调用([`fetch_releases`] / [`fetch_latest_release`])**整体阻塞**,
//! 调用方一律丢 `cx.background_executor()`。

use crate::i18n::{t, tr};

/// 语义版本比较(原版 `src/utils/updateChecker.ts:11-19` 只做主干数值比较;
/// v1.0.0-beta 起 tag 带预发布段,这里按 SemVer 补齐精度):
/// 去掉前导 `v`,`+` 后的 build metadata 不参与排序;主干按 `.` 分段数值比较,
/// 缺段按 0;主干相同时无预发布段 > 有预发布段(`1.0.0 > 1.0.0-beta`),
/// 两侧都有则交给 [`compare_prerelease`]。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parse(s: &str) -> (Vec<u64>, Option<&str>) {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let s = s.split('+').next().unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, (!pre.is_empty()).then_some(pre)),
            None => (s, None),
        };
        let nums = core
            .split('.')
            .map(|seg| {
                seg.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect();
        (nums, pre)
    }
    let ((a_num, a_pre), (b_num, b_pre)) = (parse(a), parse(b));
    for i in 0..a_num.len().max(b_num.len()) {
        let l = a_num.get(i).copied().unwrap_or(0);
        let r = b_num.get(i).copied().unwrap_or(0);
        if l != r {
            return l.cmp(&r);
        }
    }
    match (a_pre, b_pre) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(l), Some(r)) => compare_prerelease(l, r),
    }
}

/// 预发布段比较(SemVer §11.4):按 `.` 拆标识符逐个比,纯数字按数值且
/// 恒小于字母数字标识符,其余按 ASCII 字典序;前缀相同时段多者大
/// (`beta < beta.2 < beta.10 < rc`)。
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a_ids, mut b_ids) = (a.split('.'), b.split('.'));
    loop {
        match (a_ids.next(), b_ids.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                let ord = match (l.parse::<u64>(), r.parse::<u64>()) {
                    (Ok(ln), Ok(rn)) => ln.cmp(&rn),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => l.cmp(r),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// tag / 版本号带预发布段(`-` 后缀,如 `v1.0.0-beta`)即视为预发布。
/// 频道规则的判定入口 —— 见 [`pick_latest`]。
pub fn is_prerelease(version: &str) -> bool {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('+')
        .next()
        .unwrap_or("")
        .contains('-')
}

/// ISO 时间戳 → `2026/8/19`。
///
/// 原版是 `new Date(publishedAt).toLocaleDateString('zh-CN')`——**locale 写死**
/// (`SettingsModal.tsx:1635`),所以这里也不跟界面语言走。
pub fn format_published_at(iso: &str) -> String {
    let date = iso.split('T').next().unwrap_or(iso);
    let mut parts = date.split('-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d)) if y.len() == 4 && !m.is_empty() && !d.is_empty() => format!(
            "{y}/{}/{}",
            m.trim_start_matches('0'),
            d.trim_start_matches('0')
        ),
        _ => iso.to_string(),
    }
}

/// GitHub 上的仓库(`updateChecker.ts:3`)。
const GITHUB_REPO: &str = "dreamlonglll/mini-term";

/// 一条 release 的关键信息。
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    pub url: String,
    pub published_at: String,
}

/// 拉 release 列表。**整体阻塞**,调用方一律丢 `cx.background_executor()`。
///
/// ⚠️ 不用 `/releases/latest`:那个端点**永远不含预发布**,v1.0.0-beta 起
/// beta 用户会两头落空 —— 看不到下一个 beta,也等不到它变 stable 前的任何
/// 通知。改拉列表,频道取舍在本地由 [`pick_latest`] 决定。
///
/// 复用 `pricing` 那份 `zed-reqwest`(blocking feature 已开,净新增 crate = 0)。
/// GitHub API 强制要求 `User-Agent`,缺了直接 403。
fn fetch_releases() -> Result<Vec<ReleaseInfo>, String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=20");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("User-Agent", "mini-term")
        .header("Accept", "application/vnd.github+json")
        .send()
        // 传输层失败(断网 / DNS / TLS)的原文是英文且没什么可操作性,
        // 与原版一样收成一句「检查失败,请稍后重试」
        .map_err(|e| {
            eprintln!("[settings] 检查更新失败: {e}");
            t("settings", "about.checkFailed").to_string()
        })?;
    let status = resp.status().as_u16();
    if status == 404 {
        return Err(t("updateChecker", "noRelease").to_string());
    }
    if !resp.status().is_success() {
        return Err(tr!("updateChecker", "requestFailed", status = status));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let releases: Vec<ReleaseInfo> = json
        .as_array()
        .map(|list| {
            list.iter()
                // 匿名请求本就看不到 draft,这层过滤只是防御性的
                .filter(|r| !r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false))
                .map(|r| {
                    let field = |name: &str| {
                        r.get(name)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    ReleaseInfo {
                        version: field("tag_name"),
                        url: field("html_url"),
                        published_at: field("published_at"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if releases.is_empty() {
        return Err(t("updateChecker", "noRelease").to_string());
    }
    Ok(releases)
}

/// 从 release 列表里挑「本频道的最新版」:正式版用户只看正式版,
/// 预发布(beta)用户全都看 —— 跑着 beta 的人应该被引向下一个 beta 或
/// 转正的 stable,跑着 stable 的人不该被红点推去装 beta。
///
/// 返回的是频道内版本最大者,**不保证比 `current` 新** —— 新旧由
/// [`pick_newer`] / 渲染端把关(与旧 `/releases/latest` 的语义一致:
/// about 页「已是最新」也要靠这条数据摆出来)。
pub fn pick_latest(releases: Vec<ReleaseInfo>, current: &str) -> Option<ReleaseInfo> {
    let include_pre = is_prerelease(current);
    releases
        .into_iter()
        .filter(|r| include_pre || !is_prerelease(&r.version))
        .max_by(|x, y| compare_versions(&x.version, &y.version))
}

/// 拉列表 + 频道过滤后的「最新 release」,about 页手动检查用。
pub(crate) fn fetch_latest_release() -> Result<ReleaseInfo, String> {
    pick_latest(fetch_releases()?, env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| t("updateChecker", "noRelease").to_string())
}

/// 「比当前版本新才算数」的那道闸(`updateChecker.ts:30` 那行三元)。
///
/// 抽成纯函数只为可测 —— [`newer_release`] 的另一半是网络,测不了。
pub fn pick_newer(release: ReleaseInfo, current: &str) -> Option<ReleaseInfo> {
    compare_versions(&release.version, current)
        .is_gt()
        .then_some(release)
}

/// **启动自检**用的一次性检查,等价于原版 `checkForUpdate(currentVersion)`
/// (`updateChecker.ts:21-31`):拉最新 release,比当前版本新才返回 `Some`,
/// 否则(含任何失败)返回 `None`。
///
/// 原版那边是 `checkForUpdate(ver).then(...).catch(() => {})` ——
/// **失败静默**,启动时联不上网 / GitHub 限流都不该弹任何东西给用户看。
/// 这里同样把错误吃掉(只留一行 stderr,`fetch_latest_release` 已经打了)。
///
/// **整体阻塞**,调用方一律丢 `cx.background_executor()`。
pub fn newer_release(current: &str) -> Option<ReleaseInfo> {
    pick_newer(fetch_latest_release().ok()?, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本比较:去 `v` 前缀、缺段按 0。
    #[test]
    fn 版本比较() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("v0.13.2", "0.13.1"), Ordering::Greater);
        assert_eq!(compare_versions("0.13.1", "v0.13.1"), Ordering::Equal);
        assert_eq!(compare_versions("0.14", "0.13.9"), Ordering::Greater);
        // 缺段按 0:`1.0` == `1.0.0`
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.9.9", "0.10.0"), Ordering::Less);
    }

    /// 预发布序(v1.0.0-beta 起的新增精度):同主干下正式版 > 预发布,
    /// 预发布之间按 SemVer §11.4 逐标识符比较。
    #[test]
    fn 版本比较_预发布() {
        use std::cmp::Ordering;
        // 主干不同时预发布段不影响大小
        assert_eq!(compare_versions("v1.0.0-beta", "0.14.0"), Ordering::Greater);
        // 同主干:正式版胜出(旧实现这里判 Equal,升级红点会哑)
        assert_eq!(compare_versions("1.0.0-beta", "1.0.0"), Ordering::Less);
        assert_eq!(compare_versions("v1.0.0", "v1.0.0-beta"), Ordering::Greater);
        // beta < beta.2 < beta.10 < rc:数字标识符按数值,且小于字母数字标识符
        assert_eq!(compare_versions("1.0.0-beta", "1.0.0-beta.2"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-beta.2", "1.0.0-beta.10"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-beta.10", "1.0.0-rc"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-1", "1.0.0-alpha"), Ordering::Less);
        assert_eq!(compare_versions("v1.0.0-beta", "1.0.0-beta"), Ordering::Equal);
        // build metadata 不参与排序
        assert_eq!(compare_versions("1.0.0+5", "1.0.0"), Ordering::Equal);
        // 预发布判定入口
        assert!(is_prerelease("v1.0.0-beta"));
        assert!(!is_prerelease("v1.0.0"));
        assert!(!is_prerelease("1.0.0+build"));
    }

    /// 启动自检那道闸:只有**严格更新**才算数,同版本 / 更旧一律 `None`
    /// (原版 `updateChecker.ts:30` 的 `> 0 ? release : null`)。
    #[test]
    fn 启动自检只认更新的版本() {
        let release = |v: &str| ReleaseInfo {
            version: v.to_string(),
            url: "https://example.invalid/r".to_string(),
            published_at: "2026-08-19T00:00:00Z".to_string(),
        };
        assert!(pick_newer(release("v0.14.0"), "0.13.1").is_some());
        // 带 `v` 前缀的 tag 与不带的当前版本要能对上(GitHub tag 是 `v0.13.1`)
        assert!(pick_newer(release("v0.13.1"), "0.13.1").is_none());
        assert!(pick_newer(release("0.13.0"), "0.13.1").is_none());
        assert_eq!(
            pick_newer(release("v1.0.0"), "0.13.1").map(|r| r.version),
            Some("v1.0.0".to_string())
        );
        // 预发布链路:同 beta 不算更新,beta.2 与转正 stable 都算
        assert!(pick_newer(release("v1.0.0-beta"), "1.0.0-beta").is_none());
        assert!(pick_newer(release("v1.0.0-beta.2"), "1.0.0-beta").is_some());
        assert!(pick_newer(release("v1.0.0"), "1.0.0-beta").is_some());
    }

    /// 频道规则:stable 用户只看 stable,beta 用户全都看;返回频道内
    /// 版本最大者(不保证比当前新 —— 新旧是 `pick_newer` 的事)。
    #[test]
    fn 频道内挑最新() {
        let release = |v: &str| ReleaseInfo {
            version: v.to_string(),
            url: "https://example.invalid/r".to_string(),
            published_at: "2026-08-20T00:00:00Z".to_string(),
        };
        let all = || {
            vec![
                release("v0.14.0"),
                release("v1.0.0-beta"),
                release("v0.13.1"),
            ]
        };
        // stable 用户:无视 beta,挑到旧 stable(渲染端据此显示「已是最新」)
        assert_eq!(
            pick_latest(all(), "0.14.0").map(|r| r.version),
            Some("v0.14.0".to_string())
        );
        // beta 用户:看得到 beta
        assert_eq!(
            pick_latest(all(), "1.0.0-beta").map(|r| r.version),
            Some("v1.0.0-beta".to_string())
        );
        // beta 用户遇到转正 stable:stable 胜出
        assert_eq!(
            pick_latest(vec![release("v1.0.0-beta"), release("v1.0.0")], "1.0.0-beta")
                .map(|r| r.version),
            Some("v1.0.0".to_string())
        );
        assert!(pick_latest(vec![], "1.0.0-beta").is_none());
    }

    /// 发布日期:ISO → `2026/8/19`(locale 写死,与原版一致)。
    #[test]
    fn 发布日期格式() {
        assert_eq!(format_published_at("2026-08-19T03:21:00Z"), "2026/8/19");
        assert_eq!(format_published_at("2026-12-01"), "2026/12/1");
        // 认不出的原样返回,不崩
        assert_eq!(format_published_at("nope"), "nope");
    }
}
