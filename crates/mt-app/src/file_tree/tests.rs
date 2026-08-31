use super::menu::FileMenuAction::*;
use super::menu::{HeaderActionCapabilities, file_menu_actions, header_action_capabilities};
use super::*;

#[test]
fn 远程下载上下文要求项目根目录和连接身份完全一致() {
    let context = FileOperationContext {
        project_id: "project-a".into(),
        root: PathBuf::from("/workspace"),
        backend: FileBackendIdentity::Remote {
            connection_id: "ssh-a".into(),
            connection_fingerprint: 7,
        },
        generation: 3,
    };
    assert!(remote_download_context_matches(
        &context,
        "project-a",
        "/workspace",
        "ssh-a",
        7,
    ));
    assert!(!remote_download_context_matches(
        &context,
        "project-b",
        "/workspace",
        "ssh-a",
        7,
    ));
    assert!(!remote_download_context_matches(
        &context,
        "project-a",
        "/other",
        "ssh-a",
        7,
    ));
    assert!(!remote_download_context_matches(
        &context,
        "project-a",
        "/workspace",
        "ssh-b",
        7,
    ));
    assert!(!remote_download_context_matches(
        &context,
        "project-a",
        "/workspace",
        "ssh-a",
        8,
    ));

    for backend in [
        FileBackendIdentity::Local,
        FileBackendIdentity::BrokenRemote,
    ] {
        let context = FileOperationContext {
            backend,
            ..context.clone()
        };
        assert!(!remote_download_context_matches(
            &context,
            "project-a",
            "/workspace",
            "ssh-a",
            7,
        ));
    }
}

/// 文件的菜单:「使用默认工具打开」在最前(原版 unshift),没有「新建」两项。
///
/// ⚠️ Y 批把「查看变更」接了上去(V 批的 `open_file_diff` 已就绪),
/// 于是这条断言的期望向量**多了尾部两项**(分隔线 + ViewDiff);
/// 没有 git 状态的文件仍然与从前一模一样,见下面那条。
#[test]
fn 文件菜单项序与原版一致() {
    assert_eq!(
        file_menu_actions(false, true, false),
        vec![
            Some(OpenWithDefault),
            Some(CopyEntry),
            None,
            Some(CopyRelativePath),
            Some(CopyAbsolutePath),
            Some(RevealInFolder),
            Some(OpenInTerminal),
            None,
            Some(Rename),
            Some(Delete),
            None,
            Some(ViewDiff),
        ]
    );
    // 干净文件:一项不多(原版 `entryGitStatus && !entry.isDir`)
    assert_eq!(
        file_menu_actions(false, false, false),
        vec![
            Some(OpenWithDefault),
            Some(CopyEntry),
            None,
            Some(CopyRelativePath),
            Some(CopyAbsolutePath),
            Some(RevealInFolder),
            Some(OpenInTerminal),
            None,
            Some(Rename),
            Some(Delete),
        ]
    );
}

/// 目录的菜单:没有「默认工具打开」,末尾多一段「新建文件 / 新建文件夹」。
#[test]
fn 目录菜单项序与原版一致() {
    assert_eq!(
        file_menu_actions(true, false, false),
        vec![
            Some(CopyEntry),
            Some(Paste),
            None,
            Some(CopyRelativePath),
            Some(CopyAbsolutePath),
            Some(RevealInFolder),
            Some(OpenInTerminal),
            None,
            Some(Rename),
            Some(Delete),
            None,
            Some(NewFile),
            Some(NewFolder),
        ]
    );
}

/// 「查看变更」只给**有 git 状态的文件**:目录哪怕汇总出了字母也不给
/// (原版判定是 `entryGitStatus && !entry.isDir`,单文件 diff 对目录没意义);
/// 而「默认工具打开」只对文件出现。
#[test]
fn 目录与文件的差别只在两处() {
    let file: Vec<_> = file_menu_actions(false, false, false)
        .into_iter()
        .flatten()
        .collect();
    let dir: Vec<_> = file_menu_actions(true, false, false)
        .into_iter()
        .flatten()
        .collect();
    assert!(file.contains(&OpenWithDefault));
    assert!(!dir.contains(&OpenWithDefault));
    assert!(dir.contains(&NewFile) && dir.contains(&NewFolder));
    assert!(!file.contains(&NewFile) && !file.contains(&NewFolder));
    // 有状态的目录同样不给 ViewDiff
    let dirty_dir: Vec<_> = file_menu_actions(true, true, false)
        .into_iter()
        .flatten()
        .collect();
    assert!(!dirty_dir.contains(&ViewDiff));
    assert_eq!(dirty_dir, dir);
}

#[test]
fn 远程菜单不暴露本机动作并提供传输入口() {
    assert_eq!(
        file_menu_actions(false, true, true),
        vec![
            Some(CopyEntry),
            Some(Download),
            None,
            Some(CopyRelativePath),
            Some(CopyAbsolutePath),
            Some(OpenInTerminal),
            None,
            Some(Rename),
            Some(Delete),
        ]
    );
    assert_eq!(
        file_menu_actions(true, false, true),
        vec![
            Some(CopyEntry),
            Some(Paste),
            Some(Download),
            Some(UploadFiles),
            Some(UploadFolder),
            None,
            Some(CopyRelativePath),
            Some(CopyAbsolutePath),
            Some(OpenInTerminal),
            None,
            Some(Rename),
            Some(Delete),
            None,
            Some(NewFile),
            Some(NewFolder),
        ]
    );
}

// ─── git 状态着色 ─────────────────────────────────────────

/// 六个字母的配色逐条对照 `FileTree.tsx:362-369`,认不出的退 muted。
#[test]
fn git状态配色照抄原版() {
    assert_eq!(git_color("M"), ui::color_warning());
    assert_eq!(git_color("A"), ui::color_success());
    // 未跟踪与新增同色(原版 `'?': text-success`)
    assert_eq!(git_color("?"), ui::color_success());
    assert_eq!(git_color("D"), ui::color_error());
    assert_eq!(git_color("C"), ui::color_error());
    assert_eq!(git_color("R"), ui::color_info());
    // 后端将来加了新字母也不会画成错的颜色
    assert_eq!(git_color("X"), ui::text_muted());
    assert_eq!(git_color(""), ui::text_muted());
}

/// 目录汇总取子树里优先级最高的那个字母,且**只认前缀是自己的**条目。
#[test]
fn 目录汇总取最高优先级() {
    let map: HashMap<String, String> = [
        ("src/a.rs", "M"),
        ("src/b.rs", "C"),
        ("src/deep/c.rs", "A"),
        // 同名前缀的兄弟目录不许被算进来
        ("srcx/d.rs", "D"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    assert_eq!(rollup_dir_label(&map, "src"), Some("C"));
    assert_eq!(rollup_dir_label(&map, "src/deep"), Some("A"));
    assert_eq!(rollup_dir_label(&map, "srcx"), Some("D"));
    // 没有子项的目录不出徽章
    assert_eq!(rollup_dir_label(&map, "docs"), None);
    // 文件自身那条不算「子树」(前缀要带 `/`)
    assert_eq!(rollup_dir_label(&map, "src/a.rs"), None);
}

// ─── 单链目录压缩 ─────────────────────────────────────────

fn entry(name: &str, path: &str, is_dir: bool, ignored: bool) -> FileEntry {
    FileEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        is_dir,
        ignored,
    }
}

/// 假目录表:`路径 → 子项`。
fn faker(table: Vec<(&'static str, Vec<FileEntry>)>) -> impl FnMut(&Path) -> Vec<FileEntry> {
    let table: HashMap<PathBuf, Vec<FileEntry>> = table
        .into_iter()
        .map(|(k, v)| (PathBuf::from(k), v))
        .collect();
    move |dir: &Path| table.get(dir).cloned().unwrap_or_default()
}

/// 一路单子目录 → 折成一行,名字用 `/` 拼,路径指向**链尾**。
#[test]
fn 单链目录折成一行() {
    let entries = vec![entry("src", "/p/src", true, false)];
    let list = faker(vec![
        ("/p/src", vec![entry("main", "/p/src/main", true, false)]),
        (
            "/p/src/main",
            vec![entry("java", "/p/src/main/java", true, false)],
        ),
        // 链尾有两个子项 → 停
        (
            "/p/src/main/java",
            vec![
                entry("A.java", "/p/src/main/java/A.java", false, false),
                entry("B.java", "/p/src/main/java/B.java", false, false),
            ],
        ),
    ]);
    let out = compact_dir_chains(entries, list);
    assert_eq!(out.len(), 1);
    let (entry, chain) = &out[0];
    assert_eq!(entry.name, "src/main/java");
    assert_eq!(entry.path, PathBuf::from("/p/src/main/java"));
    assert_eq!(
        chain,
        &vec![
            PathBuf::from("/p/src"),
            PathBuf::from("/p/src/main"),
            PathBuf::from("/p/src/main/java"),
        ]
    );
}

/// 不压缩的几种:文件 / 被忽略的目录 / 唯一子项是文件 / 唯一子项被忽略。
/// 这几种**都返回长度 1 的 chain**(调用方据此不登记链、不额外挂监听)。
#[test]
fn 不满足前提时原样返回() {
    let entries = vec![
        entry("readme.md", "/p/readme.md", false, false),
        entry("target", "/p/target", true, true),
        entry("only-file", "/p/only-file", true, false),
        entry("only-ignored", "/p/only-ignored", true, false),
    ];
    let list = faker(vec![
        (
            "/p/only-file",
            vec![entry("a.txt", "/p/only-file/a.txt", false, false)],
        ),
        (
            "/p/only-ignored",
            vec![entry(
                "node_modules",
                "/p/only-ignored/node_modules",
                true,
                true,
            )],
        ),
        // 被忽略的目录压根不该被列(命中就说明闸门漏了)
        ("/p/target", vec![entry("x", "/p/target/x", true, false)]),
    ]);
    let out = compact_dir_chains(entries, list);
    for (entry, chain) in &out {
        assert_eq!(chain.len(), 1, "{} 不该被压缩", entry.name);
        assert!(!entry.name.contains('/'), "{} 不该改名", entry.name);
    }
}

/// 链深上限 8:再深也不继续列(每层一次串行 IPC)。
#[test]
fn 链深封顶八层() {
    // /p/d0 → d1 → … 无限深
    let mut table: Vec<(&'static str, Vec<FileEntry>)> = Vec::new();
    const PATHS: [&str; 12] = [
        "/p/d0", "/p/d1", "/p/d2", "/p/d3", "/p/d4", "/p/d5", "/p/d6", "/p/d7", "/p/d8", "/p/d9",
        "/p/d10", "/p/d11",
    ];
    for (i, path) in PATHS.iter().enumerate().take(PATHS.len() - 1) {
        let next = PATHS[i + 1];
        let name = next.rsplit('/').next().unwrap();
        table.push((path, vec![entry(name, next, true, false)]));
    }
    let out = compact_dir_chains(vec![entry("d0", "/p/d0", true, false)], faker(table));
    let (entry, chain) = &out[0];
    assert_eq!(chain.len(), MAX_CHAIN);
    assert_eq!(entry.name, "d0/d1/d2/d3/d4/d5/d6/d7");
    assert_eq!(entry.path, PathBuf::from("/p/d7"));
}

// ─── 展开态与缓存的对账 ───────────────────────────────────

/// `entries` 缓存表:`目录 → 子项`。
fn listed(table: Vec<(&'static str, Vec<FileEntry>)>) -> HashMap<PathBuf, Vec<FileEntry>> {
    table
        .into_iter()
        .map(|(k, v)| (PathBuf::from(k), v))
        .collect()
}

fn expanded_set(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
    let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
    move |p: &Path| set.contains(p)
}

/// 换项目回来的那一刻:只有根列过,展开着的一级目录全要补列。
#[test]
fn 展开却没列过的目录要补列() {
    let entries = listed(vec![(
        "/p",
        vec![
            entry("src", "/p/src", true, false),
            entry("docs", "/p/docs", true, false),
            entry("readme.md", "/p/readme.md", false, false),
        ],
    )]);
    let mut out = Vec::new();
    missing_expanded_dirs(
        &entries,
        Path::new("/p"),
        &expanded_set(&["/p/src"]),
        &mut out,
    );
    // 折叠的 docs 与文件 readme.md 都不掺和
    assert_eq!(out, vec![PathBuf::from("/p/src")]);
}

/// 已列过的目录不重复排队,但要**顺着它往下**继续对账。
#[test]
fn 已列过的目录只往下走() {
    let entries = listed(vec![
        ("/p", vec![entry("src", "/p/src", true, false)]),
        ("/p/src", vec![entry("core", "/p/src/core", true, false)]),
    ]);
    let mut out = Vec::new();
    missing_expanded_dirs(
        &entries,
        Path::new("/p"),
        &expanded_set(&["/p/src", "/p/src/core"]),
        &mut out,
    );
    assert_eq!(out, vec![PathBuf::from("/p/src/core")]);
}

/// 一轮只补**下一层**:祖先自己都还没列回来时,深层那条陈旧展开记录翻不到 ——
/// 远程一次列目录是一趟 SFTP 往返,不能按 `expandedDirs` 整份去列。
#[test]
fn 祖先没列出来时不越级补列() {
    let entries = listed(vec![("/p", vec![entry("src", "/p/src", true, false)])]);
    let mut out = Vec::new();
    missing_expanded_dirs(
        &entries,
        Path::new("/p"),
        // /p/src/core 也是展开的,但 /p/src 这一层还没内容,够不着
        &expanded_set(&["/p/src", "/p/src/core"]),
        &mut out,
    );
    assert_eq!(out, vec![PathBuf::from("/p/src")]);
}

/// 列失败时那条空记录(见 `load_dir_with` 的 Err 分支)让补列就此打住 ——
/// 否则 render → 补列 → 失败 → notify → render 会绕成死循环。
#[test]
fn 列过的空目录不再重排() {
    let entries = listed(vec![
        ("/p", vec![entry("src", "/p/src", true, false)]),
        ("/p/src", Vec::new()),
    ]);
    let mut out = Vec::new();
    missing_expanded_dirs(
        &entries,
        Path::new("/p"),
        &expanded_set(&["/p/src"]),
        &mut out,
    );
    assert!(out.is_empty());
}

/// 根目录自己都还没列出来(冷启动第一帧)时一条都不补:根那趟由
/// `sync_project` / `refresh_root` 显式排,补列不插手。
#[test]
fn 根没列出来时什么都不补() {
    let mut out = Vec::new();
    missing_expanded_dirs(
        &HashMap::new(),
        Path::new("/p"),
        &expanded_set(&["/p/src"]),
        &mut out,
    );
    assert!(out.is_empty());
}

/// 优先级表逐条(`PRIORITY = {C:6, D:5, M:4, A:3, R:2, '?':1}`)。
#[test]
fn 汇总优先级与原版一致() {
    let order = ["C", "D", "M", "A", "R", "?"];
    for pair in order.windows(2) {
        assert!(
            git_priority(pair[0]) > git_priority(pair[1]),
            "{} 应当排在 {} 前面",
            pair[0],
            pair[1]
        );
    }
    // 认不出的字母不参与汇总(优先级 0)
    assert_eq!(git_priority("X"), 0);
}

#[test]
fn 文件树头部动作按后端和忙碌状态收口() {
    let local = FileOperationContext {
        project_id: "p".into(),
        root: PathBuf::from("/work"),
        backend: FileBackendIdentity::Local,
        generation: 1,
    };
    let clip = FileClipboardEntry {
        project_id: "p".into(),
        root: PathBuf::from("/work"),
        backend: FileBackendIdentity::Local,
        generation: 1,
        source: PathBuf::from("/work/a.txt"),
        is_dir: false,
    };
    assert_eq!(
        header_action_capabilities(Some(&local), false, Some(&clip)),
        HeaderActionCapabilities {
            show_upload: false,
            mutations_enabled: true,
            paste_enabled: true,
        }
    );
    assert_eq!(
        header_action_capabilities(Some(&local), true, Some(&clip)),
        HeaderActionCapabilities {
            show_upload: false,
            mutations_enabled: false,
            paste_enabled: false,
        }
    );

    let mut remote = local.clone();
    remote.backend = FileBackendIdentity::Remote {
        connection_id: "ssh".into(),
        connection_fingerprint: 7,
    };
    let remote_caps = header_action_capabilities(Some(&remote), false, None);
    assert!(remote_caps.show_upload && remote_caps.mutations_enabled);
    assert!(!remote_caps.paste_enabled);

    let mut broken = remote;
    broken.backend = FileBackendIdentity::BrokenRemote;
    assert_eq!(
        header_action_capabilities(Some(&broken), false, None),
        HeaderActionCapabilities {
            show_upload: false,
            mutations_enabled: false,
            paste_enabled: false,
        }
    );
}
