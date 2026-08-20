//! 仅做一件事:Windows 构建时把品牌图标与版本信息嵌进 mini-term.exe 的资源段。
//! GPUI 不管 exe 资源,不嵌的话资源管理器 / 快捷方式 / 任务栏全是系统白板图标,
//! NSIS 安装版的开始菜单与桌面快捷方式直接引用 exe 图标,这里是唯一来源。
//!
//! 两道闸:`cfg(windows)` 判**宿主**(build script 跑在宿主上,Cargo.toml 里
//! winresource 也挂在 `cfg(windows)` 的 build-dependencies 下,mac/Linux 宿主
//! 连这个 crate 都不编译),`CARGO_CFG_TARGET_OS` 判**目标**,都过才嵌。

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // FILEVERSION/PRODUCTVERSION 资源只收纯数字四段(u16×4),1.0.0-beta 这类
        // 预发布号解析不动 —— 数字段手动拆出来喂,字符串档保留完整语义版本。
        let semver = std::env::var("CARGO_PKG_VERSION").unwrap();
        let numeric = semver.split(['-', '+']).next().unwrap();
        let mut parts = numeric.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        let (major, minor, patch) = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        let version_u64 = (major << 48) | (minor << 32) | (patch << 16);
        let mut res = winresource::WindowsResource::new();
        res.set_icon("resources/icon.ico");
        res.set("ProductName", "Mini-Term");
        res.set("FileDescription", "Mini-Term");
        res.set("ProductVersion", &semver);
        res.set("FileVersion", &semver);
        res.set_version_info(winresource::VersionInfo::FILEVERSION, version_u64);
        res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, version_u64);
        res.compile().expect("嵌入 Windows exe 资源(图标/版本信息)失败");
    }
}
