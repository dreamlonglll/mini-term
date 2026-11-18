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
//! - 带行号打开([`open_in_editor_at`])是 GPUI 版补的:按编辑器命令的可执行文件名认出
//!   家族([`editor_family`]),用各家自己的跳行参数([`editor_args`]);认不出来的只传文件。

use std::ffi::OsString;
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

/// 外部编辑器要跳到的位置:行从 1 数;列可选,同样从 1 数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorPosition {
    pub line: u32,
    pub column: Option<u32>,
}

impl EditorPosition {
    /// 只定到行。
    pub fn line(line: u32) -> Self {
        Self { line, column: None }
    }
}

/// 按可执行文件名认出来的编辑器家族 —— 它决定「打开并跳到第几行」的参数怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorFamily {
    /// VS Code 与它的分支(Cursor / Windsurf / VSCodium / Trae……):`-g 文件:行[:列]`
    VsCode,
    /// JetBrains 全家:`--line 行 [--column 列] 文件`
    JetBrains,
    /// Sublime Text(`subl`)与 Zed:位置直接拼在文件后面,`文件:行[:列]`
    PathSuffix,
    /// Notepad++:`-n行 [-c列] 文件`
    NotepadPlusPlus,
    /// gVim / nvim-qt:`+行 文件`(列不传)
    VimGui,
    /// 认不出来:只传文件,不猜参数 —— 猜错了,编辑器会把 `-g` 之类当成一个文件名去开
    Unknown,
}

/// 编辑器命令 → 家族。按可执行文件名认(不分大小写、去掉目录与扩展名),所以装在哪、
/// 配的是 `Code.exe` 还是 `bin\code.cmd` 都一样。
pub fn editor_family(command: &str) -> EditorFamily {
    match executable_stem(command).as_str() {
        "code" | "code-insiders" | "code - insiders" | "code-oss" | "codium" | "vscodium"
        | "cursor" | "windsurf" | "trae" | "trae cn" | "kiro" | "positron" => EditorFamily::VsCode,
        "idea" | "idea64" | "webstorm" | "webstorm64" | "pycharm" | "pycharm64" | "goland"
        | "goland64" | "rider" | "rider64" | "clion" | "clion64" | "rustrover" | "rustrover64"
        | "phpstorm" | "phpstorm64" | "rubymine" | "rubymine64" | "datagrip" | "datagrip64"
        | "dataspell" | "dataspell64" | "studio" | "studio64" => EditorFamily::JetBrains,
        "subl" | "sublime_text" | "zed" => EditorFamily::PathSuffix,
        "notepad++" => EditorFamily::NotepadPlusPlus,
        "gvim" | "nvim-qt" => EditorFamily::VimGui,
        _ => EditorFamily::Unknown,
    }
}

/// 命令里的可执行文件名:去掉引号、目录与最后一段扩展名,转小写。`/` 与 `\` 都当目录
/// 分隔符(配置里是 Windows 路径,而这段逻辑要在各平台上一样);盘符里的 `:` 落在目录那段,
/// 不影响。`Code - Insiders.exe` → `code - insiders`。
fn executable_stem(command: &str) -> String {
    let command = command.trim().trim_matches('"');
    let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let stem = match file.rfind('.') {
        Some(dot) if dot > 0 => &file[..dot],
        _ => file,
    };
    stem.to_lowercase()
}

/// 启动参数(不含可执行文件本身)。没有位置、或认不出来的编辑器只传文件。
///
/// 每个参数都是**一整个** `OsString`:带空格的路径不会被拆开(交给 `Command::args`,
/// 由它按平台规则加引号);`C:\a.rs` 里盘符的 `:` 原样留着,行号只追加在末尾 ——
/// 这几家编辑器都是从末尾往前认 `:行[:列]`。
pub fn editor_args(
    family: EditorFamily,
    path: &Path,
    position: Option<EditorPosition>,
) -> Vec<OsString> {
    let Some(position) = position.filter(|position| position.line > 0) else {
        return vec![path.as_os_str().to_owned()];
    };
    let column = position.column.filter(|column| *column > 0);
    let with_suffix = || {
        let mut arg = path.as_os_str().to_owned();
        arg.push(format!(":{}", position.line));
        if let Some(column) = column {
            arg.push(format!(":{column}"));
        }
        arg
    };
    match family {
        EditorFamily::VsCode => vec!["-g".into(), with_suffix()],
        EditorFamily::JetBrains => {
            let mut args: Vec<OsString> = vec!["--line".into(), position.line.to_string().into()];
            if let Some(column) = column {
                args.push("--column".into());
                args.push(column.to_string().into());
            }
            args.push(path.as_os_str().to_owned());
            args
        }
        EditorFamily::PathSuffix => vec![with_suffix()],
        EditorFamily::NotepadPlusPlus => {
            let mut args: Vec<OsString> = vec![format!("-n{}", position.line).into()];
            if let Some(column) = column {
                args.push(format!("-c{column}").into());
            }
            args.push(path.as_os_str().to_owned());
            args
        }
        EditorFamily::VimGui => vec![
            format!("+{}", position.line).into(),
            path.as_os_str().to_owned(),
        ],
        EditorFamily::Unknown => vec![path.as_os_str().to_owned()],
    }
}

/// 用指定编辑器打开路径。编辑器为 `None`(没配过 / 名字没匹配上)时给出可操作的提示。
pub fn open_in_editor(editor: Option<&Editor>, path: &Path) -> Result<()> {
    open_in_editor_at(editor, path, None)
}

/// 用指定编辑器打开文件并跳到 `position`(认得出跳行参数的编辑器才带,见 [`editor_args`])。
pub fn open_in_editor_at(
    editor: Option<&Editor>,
    path: &Path,
    position: Option<EditorPosition>,
) -> Result<()> {
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
    cmd.args(editor_args(editor_family(exe), path, position));
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

    /// 家族按可执行文件名认:不分大小写、去目录去扩展名,`/` 与 `\` 都当分隔符,盘符的冒号
    /// 与目录里的点不碍事。
    #[test]
    fn 按可执行文件名认编辑器家族() {
        use EditorFamily::*;
        for (command, family) in [
            (r"C:\Program Files\Microsoft VS Code\Code.exe", VsCode),
            (
                r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\bin\code.cmd",
                VsCode,
            ),
            (r"D:\VS Code Insiders\Code - Insiders.exe", VsCode),
            ("/usr/bin/code-insiders", VsCode),
            (
                r"C:\Users\u\AppData\Local\Programs\cursor\Cursor.exe",
                VsCode,
            ),
            ("/opt/Windsurf/windsurf", VsCode),
            (r"C:\Program Files\VSCodium\VSCodium.exe", VsCode),
            ("/usr/bin/codium", VsCode),
            (r"D:\Trae\Trae.exe", VsCode),
            (
                r"C:\Program Files\JetBrains\IntelliJ IDEA 2025.2\bin\idea64.exe",
                JetBrains,
            ),
            ("/opt/idea/bin/idea.sh", JetBrains),
            (r"D:\JetBrains\WebStorm\bin\webstorm64.exe", JetBrains),
            ("pycharm", JetBrains),
            ("goland64.exe", JetBrains),
            ("rider64.exe", JetBrains),
            ("CLion64.EXE", JetBrains),
            ("rustrover64.exe", JetBrains),
            (r"C:\Program Files\Sublime Text\subl.exe", PathSuffix),
            (
                r"C:\Program Files\Sublime Text\sublime_text.exe",
                PathSuffix,
            ),
            (r"C:\Users\u\AppData\Local\Programs\Zed\zed.exe", PathSuffix),
            (r"D:\tools\npp.8.6\notepad++.exe", NotepadPlusPlus),
            (r"C:\Program Files\Vim\vim91\gvim.exe", VimGui),
            ("/usr/bin/nvim-qt", VimGui),
            (r#""C:\Program Files\Microsoft VS Code\Code.exe""#, VsCode),
            (r"C:\Windows\notepad.exe", Unknown),
            ("/usr/bin/emacs", Unknown),
            ("", Unknown),
        ] {
            assert_eq!(editor_family(command), family, "{command}");
        }
    }

    fn args(family: EditorFamily, path: &str, position: Option<EditorPosition>) -> Vec<String> {
        editor_args(family, Path::new(path), position)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// 各家的跳行写法;带空格的路径始终是**一个**参数,盘符的冒号原样留着、行号只追加在末尾。
    #[test]
    fn 各家编辑器的跳行参数() {
        use EditorFamily::*;
        let path = r"C:\My Project\src\main.rs";
        let at = Some(EditorPosition::line(12));
        let at_col = Some(EditorPosition {
            line: 12,
            column: Some(5),
        });
        assert_eq!(
            args(VsCode, path, at),
            ["-g", r"C:\My Project\src\main.rs:12"]
        );
        assert_eq!(
            args(VsCode, path, at_col),
            ["-g", r"C:\My Project\src\main.rs:12:5"]
        );
        assert_eq!(args(JetBrains, path, at), ["--line", "12", path]);
        assert_eq!(
            args(JetBrains, path, at_col),
            ["--line", "12", "--column", "5", path]
        );
        assert_eq!(
            args(PathSuffix, path, at),
            [r"C:\My Project\src\main.rs:12"]
        );
        assert_eq!(
            args(PathSuffix, path, at_col),
            [r"C:\My Project\src\main.rs:12:5"]
        );
        assert_eq!(args(NotepadPlusPlus, path, at), ["-n12", path]);
        assert_eq!(args(NotepadPlusPlus, path, at_col), ["-n12", "-c5", path]);
        assert_eq!(args(VimGui, path, at_col), ["+12", path]);
        // 认不出来的编辑器不猜参数
        assert_eq!(args(Unknown, path, at_col), [path]);
        // POSIX 路径同一套
        assert_eq!(
            args(VsCode, "/home/u/my proj/a.ts", at),
            ["-g", "/home/u/my proj/a.ts:12"]
        );
    }

    /// 没有位置(或行号是 0 这种无效值)时只传文件:盘符 `C:` 不会被当成「文件:行」。
    #[test]
    fn 没有行号时只传文件() {
        for family in [
            EditorFamily::VsCode,
            EditorFamily::JetBrains,
            EditorFamily::PathSuffix,
            EditorFamily::NotepadPlusPlus,
            EditorFamily::VimGui,
            EditorFamily::Unknown,
        ] {
            assert_eq!(args(family, r"C:\a b\x.rs", None), [r"C:\a b\x.rs"]);
            assert_eq!(
                args(family, r"C:\a b\x.rs", Some(EditorPosition::line(0))),
                [r"C:\a b\x.rs"]
            );
        }
        // 列号 0 视同没给
        assert_eq!(
            args(
                EditorFamily::VsCode,
                "/a.rs",
                Some(EditorPosition {
                    line: 3,
                    column: Some(0)
                })
            ),
            ["-g", "/a.rs:3"]
        );
    }

    #[test]
    fn open_default_app_rejects_missing_path() {
        let err = open_path_with_default_app(Path::new("definitely-not-here-xyz"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("路径不存在"));
    }
}
