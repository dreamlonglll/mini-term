//! shell 列表的增删改 —— 纯函数,对照 `src/components/SettingsModal.tsx` 的
//! `TerminalSettings`(handleAdd / handleDelete / handleUpdate / handleSetDefault)。
//!
//! 三条语义单拎出来是因为它们**很容易写错且后果不可见**:默认 shell 是个按
//! 名字引用的字符串,改名或删除时不跟着修,配置里就留下一个指向不存在 shell 的
//! `defaultShell` —— 新建终端会静默回落到列表首项,用户以为自己的默认设置还在。

use mt_config::ShellConfig;

/// shell 列表与默认名的一份快照。
///
/// (没有 `PartialEq`:`mt_config::ShellConfig` 没实现它,而 mt-config 这一批是
/// 只读的 —— 已记入交付说明的接线需求。)
#[derive(Clone, Debug)]
pub struct ShellList {
    pub shells: Vec<ShellConfig>,
    pub default_shell: String,
}

/// 参数里的空白名/命令一律视为无效(旧版按钮在两者非空时才可点)。
pub fn valid_shell(name: &str, command: &str) -> bool {
    !name.trim().is_empty() && !command.trim().is_empty()
}

/// 参数串 → argv。旧版按空白切分(`args.trim().split(/\s+/)`),这里逐字照搬 ——
/// 带空格的单个参数需要用户自己去改 config.json,与装机版同一限制。
pub fn parse_args(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.split_whitespace().map(str::to_string).collect())
}

impl ShellList {
    /// 追加一个 shell。列表原本没有默认项时,新加的这个成为默认。
    pub fn add(&mut self, shell: ShellConfig) {
        if self.default_shell.trim().is_empty() {
            self.default_shell = shell.name.clone();
        }
        self.shells.push(shell);
    }

    /// 覆盖某一行。**改名时默认项跟着改** —— 否则默认设置指向一个不存在的名字。
    pub fn update(&mut self, index: usize, shell: ShellConfig) {
        let Some(slot) = self.shells.get_mut(index) else {
            return;
        };
        let was_default = slot.name == self.default_shell;
        *slot = shell;
        if was_default {
            self.default_shell = self.shells[index].name.clone();
        }
    }

    /// 删除某一行。删掉的正是默认项时,默认落到剩下的第一个(全删光则为空串)。
    pub fn remove(&mut self, index: usize) {
        if index >= self.shells.len() {
            return;
        }
        self.shells.remove(index);
        if !self.shells.iter().any(|s| s.name == self.default_shell) {
            self.default_shell = self
                .shells
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_default();
        }
    }

    /// 设默认。名字不在列表里就不动(防止把默认指到空气上)。
    pub fn set_default(&mut self, name: &str) {
        if self.shells.iter().any(|s| s.name == name) {
            self.default_shell = name.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(name: &str) -> ShellConfig {
        ShellConfig {
            name: name.into(),
            command: format!("{name}.exe"),
            args: None,
        }
    }

    fn list() -> ShellList {
        ShellList {
            shells: vec![shell("PowerShell"), shell("cmd")],
            default_shell: "PowerShell".into(),
        }
    }

    #[test]
    fn 空列表加第一个即成为默认() {
        let mut l = ShellList {
            shells: vec![],
            default_shell: String::new(),
        };
        l.add(shell("cmd"));
        assert_eq!(l.default_shell, "cmd");

        // 已有默认时新加的不抢
        l.add(shell("bash"));
        assert_eq!(l.default_shell, "cmd");
    }

    #[test]
    fn 改名时默认项跟着改() {
        let mut l = list();
        l.update(0, shell("pwsh"));
        assert_eq!(l.default_shell, "pwsh", "默认项不跟着改就会指向不存在的名字");

        // 改非默认行不动默认
        l.update(1, shell("cmd2"));
        assert_eq!(l.default_shell, "pwsh");
    }

    #[test]
    fn 删掉默认项后默认落到首项() {
        let mut l = list();
        l.remove(0);
        assert_eq!(l.default_shell, "cmd");

        l.remove(0);
        assert!(l.shells.is_empty());
        assert_eq!(l.default_shell, "");
    }

    #[test]
    fn 删掉非默认项不动默认() {
        let mut l = list();
        l.remove(1);
        assert_eq!(l.default_shell, "PowerShell");
        assert_eq!(l.shells.len(), 1);
    }

    /// 列表的可比较快照(ShellConfig 没有 PartialEq)。
    fn snapshot(l: &ShellList) -> (Vec<(String, String)>, String) {
        (
            l.shells
                .iter()
                .map(|s| (s.name.clone(), s.command.clone()))
                .collect(),
            l.default_shell.clone(),
        )
    }

    #[test]
    fn 越界的增删改一律不动列表() {
        let mut l = list();
        let before = snapshot(&l);
        l.remove(9);
        l.update(9, shell("x"));
        assert_eq!(snapshot(&l), before);
    }

    #[test]
    fn 设默认只认列表里的名字() {
        let mut l = list();
        l.set_default("cmd");
        assert_eq!(l.default_shell, "cmd");
        l.set_default("不存在");
        assert_eq!(l.default_shell, "cmd");
    }

    #[test]
    fn 参数串按空白切分() {
        assert_eq!(parse_args("  "), None);
        assert_eq!(
            parse_args(" -NoLogo   -NoProfile "),
            Some(vec!["-NoLogo".to_string(), "-NoProfile".to_string()])
        );
    }

    #[test]
    fn 名字或命令为空一律无效() {
        assert!(valid_shell("cmd", "cmd.exe"));
        assert!(!valid_shell("  ", "cmd.exe"));
        assert!(!valid_shell("cmd", " "));
    }
}
