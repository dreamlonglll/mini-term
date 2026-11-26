//! 扩展 id:已知集合是封闭的,清单里不认识的 id 一律忽略(前向兼容)。

use std::fmt;

/// 远程 Git helper 的文件名(四个服务器平台共用)。
pub const GIT_HELPER_FILE: &str = "mt-git-helper";
/// `ssh-tools` 扩展里的 SSH CLI(本桌面平台的文件名)。
pub const SSH_CLI_FILE: &str = if cfg!(windows) {
    "mt-ssh-cli.exe"
} else {
    "mt-ssh-cli"
};
/// `ssh-tools` 扩展里的 SSH MCP server(过渡期的旧通道,随 CLI 一起装)。
pub const SSH_MCP_FILE: &str = if cfg!(windows) {
    "mt-ssh-mcp.exe"
} else {
    "mt-ssh-mcp"
};

/// 本版本认识的扩展。
///
/// ⚠️ [`Self::as_str`] 的字符串同时是清单里的 `id`、资产名的一段(`mt-ext_<id>_…zip`)与
/// 本机存放目录名 —— 一经发布**不改**,改了旧清单引用的资产与已装目录就全对不上了。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComponentId {
    /// 远程 Git helper,Linux x86_64 服务器(musl 静态)。四个 helper 对**所有**桌面平台都
    /// 提供:按 SSH 服务器平台选,不按桌面平台选。
    GitHelperLinuxX86_64,
    /// 远程 Git helper,Linux aarch64 服务器(musl 静态)。
    GitHelperLinuxAarch64,
    /// 远程 Git helper,macOS x86_64 服务器。
    GitHelperMacosX86_64,
    /// 远程 Git helper,macOS arm64 服务器。
    GitHelperMacosAarch64,
    /// `mt-ssh-cli[.exe]` + `mt-ssh-mcp[.exe]`(阶段 B 接入,桌面平台相关)。
    SshTools,
    /// 语法高亮扩展包动态库(阶段 C 接入,桌面平台相关)。
    SyntaxPack,
}

/// 扩展的类别:决定旧布局在 exe 旁的位置、界面上怎么描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentKind {
    GitHelper,
    SshTools,
    SyntaxPack,
}

impl ComponentId {
    /// 全部已知 id(界面与 GC 按这个顺序遍历)。
    pub const ALL: [Self; 6] = [
        Self::GitHelperLinuxX86_64,
        Self::GitHelperLinuxAarch64,
        Self::GitHelperMacosX86_64,
        Self::GitHelperMacosAarch64,
        Self::SshTools,
        Self::SyntaxPack,
    ];

    /// 清单 id / 资产名片段 / 存放目录名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHelperLinuxX86_64 => "git-helper-linux-x86_64",
            Self::GitHelperLinuxAarch64 => "git-helper-linux-aarch64",
            Self::GitHelperMacosX86_64 => "git-helper-macos-x86_64",
            Self::GitHelperMacosAarch64 => "git-helper-macos-aarch64",
            Self::SshTools => "ssh-tools",
            Self::SyntaxPack => "syntax-pack",
        }
    }

    /// 按清单 id 反查;不认识 = `None`(调用方忽略该条目)。
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.as_str() == text)
    }

    /// 远程 Git helper 的服务器平台键(`linux-x86_64` …,即 mt-remote 部署时
    /// `uname` 探出来的 target,也是旧布局 `remote-git-helpers/<target>/` 的目录名)。
    pub const fn helper_target(self) -> Option<&'static str> {
        match self {
            Self::GitHelperLinuxX86_64 => Some("linux-x86_64"),
            Self::GitHelperLinuxAarch64 => Some("linux-aarch64"),
            Self::GitHelperMacosX86_64 => Some("macos-x86_64"),
            Self::GitHelperMacosAarch64 => Some("macos-aarch64"),
            Self::SshTools | Self::SyntaxPack => None,
        }
    }

    /// 服务器平台键 → helper 扩展。不支持的平台 = `None`。
    pub fn git_helper(target: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|id| id.helper_target() == Some(target))
    }

    pub const fn kind(self) -> ComponentKind {
        match self {
            Self::GitHelperLinuxX86_64
            | Self::GitHelperLinuxAarch64
            | Self::GitHelperMacosX86_64
            | Self::GitHelperMacosAarch64 => ComponentKind::GitHelper,
            Self::SshTools => ComponentKind::SshTools,
            Self::SyntaxPack => ComponentKind::SyntaxPack,
        }
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
