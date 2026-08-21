//! 用外部编辑器 / 系统默认程序打开路径。
//!
//! # 与 Tauri 版的差别
//!
//! - 原实现签名是 `open_in_editor(app: AppHandle, path, editor_name)`,进函数后
//!   自己 `read_config(&app)` 去翻编辑器列表。这里把「选哪个编辑器」和「怎么启动」
//!   拆成 [`select_editor`] 与 [`open_in_editor`] 两步,前者是纯函数(可测),
//!   后者只认一个具体的 [`Editor`] —— 本 crate 因此不必依赖 `mt-config`。
//! - `open_path_with_default_app` 原本走 `tauri-plugin-opener`。没了 Tauri 就直接
//!   spawn 平台自带的打开器(Windows `explorer.exe` / macOS `open` / Linux `xdg-open`)。

use std::path::Path;
use std::process::Command;

use anyhow::{Result, anyhow, bail};

/// 一个外部编辑器条目。字段与 `AppConfig.editors` 对齐,由调用方从配置里映射过来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub name: String,
    pub command: String,
}

/// 挑出要用的编辑器:
/// - 指定了名字 → 按名字精确匹配(找不到就是 None,不做兜底);
/// - 没指定 → 用 `default_editor` 指向的那个,再退到列表第一个。
pub fn select_editor<'a>(
    editors: &'a [Editor],
    default_editor: Option<&str>,
    requested: Option<&str>,
) -> Option<&'a Editor> {
    match requested {
        Some(name) => editors.iter().find(|e| e.name == name),
        None => default_editor
            .and_then(|name| editors.iter().find(|e| e.name == name))
            .or_else(|| editors.first()),
    }
}

/// 用指定编辑器打开路径。编辑器为 `None`(没配过 / 名字没匹配上)时给出可操作的提示。
pub fn open_in_editor(editor: Option<&Editor>, path: &Path) -> Result<()> {
    let editor = editor.ok_or_else(|| {
        anyhow!("尚未配置外部编辑器,请在『设置 → 系统设置 → 外部编辑器』中添加。")
    })?;

    let exe = editor.command.trim();
    if exe.is_empty() {
        bail!("编辑器「{}」的可执行文件路径为空", editor.name);
    }

    let exe_path = Path::new(exe);
    if !exe_path.exists() {
        bail!("编辑器「{}」的路径不存在:{}", editor.name, exe);
    }

    let mut cmd = Command::new(exe_path);
    cmd.arg(path);
    hide_console_window(&mut cmd);

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("启动编辑器「{}」失败:{}", editor.name, e))
}

/// 用系统默认应用打开文件/目录。
pub fn open_path_with_default_app(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("路径不存在:{}", path.display());
    }

    // 各平台自带的打开器。Windows 上 explorer 对文件走默认关联程序、对目录开资源管理器,
    // 且不像 `cmd /C start` 那样要担心 `&` 之类字符被 shell 二次解析。
    // (explorer 成功时也可能返回非 0 退出码,所以只 spawn 不等 status。)
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("explorer.exe");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };

    hide_console_window(&mut cmd);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("打开失败:{}", e))
}

/// 用**浏览器**打开一个本地文件(HTML 预览的「用浏览器打开」)。
///
/// 与 [`open_path_with_default_app`] 的区别是刻意的:`.html` 的**文件关联**常被
/// 设成编辑器(用户实测是 notepad--),那样点一下只会再开一个编辑器 —— 而这个
/// 动作要的就是浏览器。于是改问**协议关联**:Windows 上读注册表里 `https` 的
/// UserChoice(「设置 → 默认应用 → 网页浏览器」选的那个),拿它的 open 命令行来开。
///
/// 找不到浏览器时**不悄悄退回文件关联**(那就又回到编辑器了),直接报错让上层
/// 说明情况。
pub fn open_path_in_browser(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("路径不存在:{}", path.display());
    }
    let url = file_url(path);

    #[cfg(target_os = "windows")]
    {
        let (exe, args) = default_browser_command()
            .ok_or_else(|| anyhow!("没找到默认浏览器(注册表里没有 https 的协议关联)"))?;
        let mut cmd = Command::new(exe);
        cmd.args(args.iter().map(|arg| substitute_arg(arg, &url)));
        // 命令行模板里没有 `%1` 之类占位符时,URL 就当最后一个参数追加
        if !args.iter().any(|arg| has_placeholder(arg)) {
            cmd.arg(&url);
        }
        hide_console_window(&mut cmd);
        return cmd
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow!("启动浏览器失败:{}", e));
    }

    // mac/Linux:没有 Windows 那样一问就准的协议关联表,按常见约定逐个试。
    // `$BROWSER` 是 Unix 世界的老约定,优先;之后是各发行版的择一包装器。
    #[cfg(not(target_os = "windows"))]
    {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if let Ok(browser) = std::env::var("BROWSER")
            && !browser.trim().is_empty()
        {
            candidates.push(vec![browser, url.clone()]);
        }
        #[cfg(target_os = "macos")]
        for app in ["Google Chrome", "Safari", "Firefox", "Microsoft Edge"] {
            candidates.push(vec![
                "open".into(),
                "-a".into(),
                app.into(),
                url.clone(),
            ]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for opener in ["x-www-browser", "sensible-browser", "xdg-open"] {
            candidates.push(vec![opener.into(), url.clone()]);
        }

        for argv in &candidates {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            hide_console_window(&mut cmd);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
        bail!("没找到可用的浏览器(试过 $BROWSER 与常见启动器)")
    }
}

/// 本地路径 → `file:///…` URL。只转义会被浏览器误解的那几个字符;
/// 其余(含中文)浏览器自己认得,不做整串百分号编码。
fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for ch in raw.trim_start_matches('/').chars() {
        match ch {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(ch),
        }
    }
    out
}

/// 注册表命令行里的占位符(`%1` 是路径/URL,`%L`/`%l` 是长名形式)。
#[cfg(target_os = "windows")]
fn has_placeholder(arg: &str) -> bool {
    arg.contains("%1") || arg.contains("%L") || arg.contains("%l")
}

/// 把参数里的占位符换成实际 URL;没有占位符就原样。
#[cfg(target_os = "windows")]
fn substitute_arg(arg: &str, url: &str) -> String {
    arg.replace("%1", url).replace("%L", url).replace("%l", url)
}

/// 默认浏览器的 `(exe, 参数模板)`。
///
/// 三层依次退让:用户在「默认应用」里选的 https 处理器 → 同一张表里的 http →
/// 系统级的 `HKCR\http\shell\open\command`(没设过 UserChoice 的机器走这条)。
#[cfg(target_os = "windows")]
fn default_browser_command() -> Option<(String, Vec<String>)> {
    for scheme in ["https", "http"] {
        let progid = windows_registry::CURRENT_USER
            .open(format!(
                r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\{scheme}\UserChoice"
            ))
            .ok()
            .and_then(|key| key.get_string("ProgId").ok());
        if let Some(progid) = progid
            && let Some(cmd) = command_of_progid(&progid)
        {
            return Some(cmd);
        }
    }
    command_of_progid("http")
}

/// `HKCR\<ProgId>\shell\open\command` 的默认值 → 拆好的 `(exe, 参数)`。
#[cfg(target_os = "windows")]
fn command_of_progid(progid: &str) -> Option<(String, Vec<String>)> {
    let raw = windows_registry::CLASSES_ROOT
        .open(format!(r"{progid}\shell\open\command"))
        .ok()?
        // 默认值(名字为空串)就是命令行本身
        .get_string("")
        .ok()?;
    split_command_line(&raw)
}

/// 拆注册表里的命令行:`"C:\…\chrome.exe" -- "%1"` → `(exe, ["--", "%1"])`。
///
/// 只认双引号分段(注册表里就这一种写法),引号内的空格不拆。
#[cfg(target_os = "windows")]
fn split_command_line(raw: &str) -> Option<(String, Vec<String>)> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in raw.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let mut it = tokens.into_iter();
    let exe = it.next()?;
    (!exe.is_empty()).then(|| (exe, it.collect()))
}

/// Windows GUI 应用下 spawn 控制台子进程会闪黑框,统一抑制掉。
fn hide_console_window(_cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editors() -> Vec<Editor> {
        vec![
            Editor {
                name: "VS Code".into(),
                command: "code.exe".into(),
            },
            Editor {
                name: "Zed".into(),
                command: "zed.exe".into(),
            },
        ]
    }

    #[test]
    fn 本地路径转_file_url() {
        // Windows 盘符跟在三斜杠后,反斜杠归一;空格要转义,否则浏览器只收半截
        let url = file_url(Path::new(r"D:\my docs\a b.html"));
        assert_eq!(url, "file:///D:/my%20docs/a%20b.html");
        // POSIX 绝对路径不产生四道斜杠
        assert_eq!(file_url(Path::new("/tmp/a.html")), "file:///tmp/a.html");
        // 中文不编码(浏览器认得),`#` 必须编码 —— 否则后面被当锚点
        let url = file_url(Path::new("/x/说明#1.html"));
        assert_eq!(url, "file:///x/说明%231.html");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn 注册表命令行拆成_exe_与参数() {
        // Chrome / Edge 在注册表里的典型写法
        let (exe, args) =
            split_command_line(r#""C:\Program Files\Google\Chrome\Application\chrome.exe" -- "%1""#)
                .unwrap();
        assert_eq!(exe, r"C:\Program Files\Google\Chrome\Application\chrome.exe");
        assert_eq!(args, vec!["--", "%1"]);
        assert!(args.iter().any(|a| has_placeholder(a)));
        assert_eq!(substitute_arg("%1", "file:///D:/a.html"), "file:///D:/a.html");

        // 没有引号、没有占位符的写法也得能拆(URL 由调用方追加)
        let (exe, args) = split_command_line("firefox.exe").unwrap();
        assert_eq!(exe, "firefox.exe");
        assert!(args.is_empty());
        assert!(!args.iter().any(|a| has_placeholder(a)));

        assert!(split_command_line("   ").is_none(), "空命令行不该拆出 exe");
    }

    #[test]
    fn select_by_explicit_name() {
        let list = editors();
        assert_eq!(
            select_editor(&list, Some("VS Code"), Some("Zed"))
                .unwrap()
                .name,
            "Zed"
        );
    }

    #[test]
    fn explicit_name_not_found_is_none() {
        let list = editors();
        assert!(select_editor(&list, Some("VS Code"), Some("Emacs")).is_none());
    }

    #[test]
    fn falls_back_to_default_then_first() {
        let list = editors();
        assert_eq!(select_editor(&list, Some("Zed"), None).unwrap().name, "Zed");
        // default 指向不存在的条目 → 退回列表第一个
        assert_eq!(
            select_editor(&list, Some("Emacs"), None).unwrap().name,
            "VS Code"
        );
        assert_eq!(select_editor(&list, None, None).unwrap().name, "VS Code");
        assert!(select_editor(&[], None, None).is_none());
    }

    #[test]
    fn open_without_editor_gives_actionable_hint() {
        let err = open_in_editor(None, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("尚未配置外部编辑器"));
    }

    #[test]
    fn open_rejects_empty_or_missing_executable() {
        let empty = Editor {
            name: "空".into(),
            command: "   ".into(),
        };
        let err = open_in_editor(Some(&empty), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("可执行文件路径为空"));

        let missing = Editor {
            name: "幽灵".into(),
            command: "definitely-not-here-xyz.exe".into(),
        };
        let err = open_in_editor(Some(&missing), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("路径不存在"));
    }

    #[test]
    fn open_default_app_rejects_missing_path() {
        let err = open_path_with_default_app(Path::new("definitely-not-here-xyz"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("路径不存在"));
    }
}
