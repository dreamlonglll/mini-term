//! WSL 发行版枚举(会话来源选择用)。
//!
//! 读注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`:
//! - 每个子键(guid)下 `DistributionName`(REG_SZ)= 发行版名;
//! - 根键 `DefaultDistribution`(REG_SZ)= 默认发行版的 guid;
//! - `State`(REG_DWORD,1 = installed)过滤未安装完成的项。
//!
//! 不 spawn `wsl.exe -l -q`:进程开销 + stdout 是 UTF-16LE 编码坑。
//! 非 Windows 平台返回空列表(前端菜单枚举为空即自然隐藏)。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WslDistro {
    pub name: String,
    pub is_default: bool,
}

/// 注册表单条原始记录:(guid, DistributionName, State)。
/// 与注册表 IO 解耦,便于跨平台单测纯逻辑。
/// 非 Windows 下仅测试使用,cfg_attr 抑制 cargo build 的 dead_code 警告。
#[cfg_attr(not(windows), allow(dead_code))]
type RawDistroEntry = (String, Option<String>, Option<u32>);

/// 从原始注册表记录构建发行版列表:
/// - `State` 存在且 != 1 → 跳过(未安装完成 / 正在卸载);缺省视为已安装(老版本无此值);
/// - `DistributionName` 缺失或为空 → 跳过;
/// - guid 与 `DefaultDistribution` 匹配(大小写不敏感)→ is_default;
/// - 默认项排最前,其余按名称不区分大小写排序。
///
/// 非 Windows 下仅测试使用,cfg_attr 抑制 cargo build 的 dead_code 警告。
#[cfg_attr(not(windows), allow(dead_code))]
fn build_distro_list(entries: Vec<RawDistroEntry>, default_guid: &str) -> Vec<WslDistro> {
    let mut distros: Vec<WslDistro> = entries
        .into_iter()
        .filter_map(|(guid, name, state)| {
            if let Some(s) = state {
                if s != 1 {
                    return None;
                }
            }
            let name = name.filter(|n| !n.is_empty())?;
            Some(WslDistro {
                name,
                is_default: !default_guid.is_empty() && guid.eq_ignore_ascii_case(default_guid),
            })
        })
        .collect();

    distros.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    distros
}

#[cfg(windows)]
fn read_lxss_registry() -> Option<Vec<WslDistro>> {
    use windows_registry::CURRENT_USER;

    let lxss = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Lxss")
        .ok()?;
    let default_guid = lxss.get_string("DefaultDistribution").unwrap_or_default();

    let mut entries: Vec<RawDistroEntry> = Vec::new();
    for guid in lxss.keys().ok()? {
        let sub = match lxss.open(&guid) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let name = sub.get_string("DistributionName").ok();
        let state = sub.get_u32("State").ok();
        entries.push((guid, name, state));
    }

    Some(build_distro_list(entries, &default_guid))
}

/// 枚举已安装的 WSL 发行版。未装 WSL / 读注册表失败一律静默返回空列表。
#[tauri::command]
pub fn list_wsl_distros() -> Vec<WslDistro> {
    #[cfg(windows)]
    {
        read_lxss_registry().unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_distro_list_filters_state_and_marks_default() {
        let entries = vec![
            (
                "{aaa}".to_string(),
                Some("Ubuntu".to_string()),
                Some(1u32),
            ),
            ("{bbb}".to_string(), Some("Debian".to_string()), Some(3)), // 卸载中 → 过滤
            ("{ccc}".to_string(), Some("Alpine".to_string()), None),    // 无 State → 视为已安装
        ];
        let list = build_distro_list(entries, "{AAA}"); // 大小写不敏感匹配默认项

        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Ubuntu");
        assert!(list[0].is_default);
        assert_eq!(list[1].name, "Alpine");
        assert!(!list[1].is_default);
    }

    #[test]
    fn build_distro_list_skips_missing_or_empty_name() {
        let entries = vec![
            ("{a}".to_string(), None, Some(1)),
            ("{b}".to_string(), Some("".to_string()), Some(1)),
            ("{c}".to_string(), Some("Ubuntu-22.04".to_string()), Some(1)),
        ];
        let list = build_distro_list(entries, "");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Ubuntu-22.04");
        assert!(!list[0].is_default); // 空 default_guid 不匹配任何项
    }

    #[test]
    fn build_distro_list_sorts_default_first_then_by_name() {
        let entries = vec![
            ("{z}".to_string(), Some("zeta".to_string()), Some(1)),
            ("{d}".to_string(), Some("Debian".to_string()), Some(1)),
            ("{u}".to_string(), Some("Ubuntu".to_string()), Some(1)),
        ];
        let list = build_distro_list(entries, "{u}");
        let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Ubuntu", "Debian", "zeta"]);
    }
}
