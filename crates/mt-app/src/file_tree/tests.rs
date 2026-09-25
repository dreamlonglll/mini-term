use super::menu::FileMenuAction::*;
use super::menu::file_menu_actions;
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
            Some(MoveTo),
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
            Some(MoveTo),
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
            Some(MoveTo),
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
            Some(MoveTo),
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
            Some(MoveTo),
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

/// 重构前的汇总算法**原样**留作对照:render 时对每个目录行扫一遍整张状态表,
/// 取所有以 `rel/` 开头的条目里优先级最高的那个。
fn rollup_dir_label_by_scan<'a>(status: &'a HashMap<String, String>, rel: &str) -> Option<&'a str> {
    let prefix = if rel.ends_with('/') {
        rel.to_string()
    } else {
        format!("{rel}/")
    };
    let mut best: Option<(&str, u8)> = None;
    for (path, label) in status {
        if !path.starts_with(&prefix) {
            continue;
        }
        let p = git_priority(label);
        if p > best.map(|(_, bp)| bp).unwrap_or(0) {
            best = Some((label.as_str(), p));
        }
    }
    best.map(|(label, _)| label)
}

/// 重构前 `rows` 里那段 match 原样留作对照:自身有状态用自身的,目录才汇总。
fn label_by_scan(
    status: &HashMap<String, String>,
    rel: &str,
    is_dir: bool,
) -> Option<(String, bool)> {
    match status.get(rel) {
        Some(label) => Some((label.clone(), false)),
        None if is_dir => rollup_dir_label_by_scan(status, rel).map(|l| (l.to_string(), true)),
        None => None,
    }
}

fn status_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 被查的相对路径:表里每条路径的每一级祖先(带不带结尾 `/`)、路径本身,外加一组
/// 表里没有的与边角的(空串、`/`、同名前缀的兄弟、`..` 开头的)。
fn probe_rels(status: &HashMap<String, String>) -> Vec<String> {
    let mut rels: Vec<String> = [
        "", "/", ".", "a", "a/", "a//", "src", "src/", "srcx", "src/deep", "docs", "nope", "..",
        "../a", "a/b", "a/b/c", "x/y/z/w",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for path in status.keys() {
        rels.push(path.clone());
        rels.push(format!("{path}/"));
        for (slash, _) in path.match_indices('/') {
            rels.push(path[..slash].to_string());
            rels.push(path[..=slash].to_string());
        }
    }
    rels
}

/// 新旧两套在同一张表上逐个 rel、目录 / 文件两种行逐条比对。
fn assert_same_as_scan(status: &HashMap<String, String>) {
    let labels = GitLabels::new(status.clone());
    for rel in probe_rels(status) {
        for is_dir in [true, false] {
            assert_eq!(
                labels.label_for(&rel, is_dir),
                label_by_scan(status, &rel, is_dir),
                "rel={rel:?} is_dir={is_dir} 表={status:?}"
            );
        }
    }
}

/// 目录汇总取子树里优先级最高的那个字母,且**只认前缀是自己的**条目。
#[test]
fn 目录汇总取最高优先级() {
    let map = status_map(&[
        ("src/a.rs", "M"),
        ("src/b.rs", "C"),
        ("src/deep/c.rs", "A"),
        // 同名前缀的兄弟目录不许被算进来
        ("srcx/d.rs", "D"),
    ]);
    let labels = GitLabels::new(map.clone());

    for (rel, expect) in [
        ("src", Some("C")),
        ("src/deep", Some("A")),
        ("srcx", Some("D")),
        // 没有子项的目录不出徽章
        ("docs", None),
        // 文件自身那条不算「子树」(前缀要带 `/`)
        ("src/a.rs", None),
    ] {
        assert_eq!(rollup_dir_label_by_scan(&map, rel), expect, "旧算法 {rel}");
        assert_eq!(labels.dir_rollup(rel), expect, "预算表 {rel}");
    }
    // 行徽标:目录的汇总标「汇总来的」,文件自身的不标
    assert_eq!(labels.label_for("src", true), Some(("C".into(), true)));
    assert_eq!(
        labels.label_for("src/a.rs", false),
        Some(("M".into(), false))
    );
    // 文件没有自身状态就是没有,不汇总
    assert_eq!(labels.label_for("src", false), None);
}

/// 预算表与旧的逐行扫描逐条等价:手挑的边角表 + 一批伪随机表。
#[test]
fn 目录汇总预算与逐行扫描一致() {
    let cases: Vec<HashMap<String, String>> = vec![
        status_map(&[]),
        // 各状态组合:同一目录下六种字母 + 认不出的字母 / 空串(不参与汇总)
        status_map(&[
            ("a/m.rs", "M"),
            ("a/n.rs", "A"),
            ("a/d.rs", "D"),
            ("a/r.rs", "R"),
            ("a/u.rs", "?"),
            ("a/c.rs", "C"),
            ("b/x.rs", "X"),
            ("b/e.rs", ""),
        ]),
        // 只有认不出的字母:目录不出徽章
        status_map(&[("only/x.rs", "X")]),
        // 嵌套:深层高优先级要一路顶上去,中间层各取各的子树
        status_map(&[
            ("a/b/c/d/deep.rs", "C"),
            ("a/b/mid.rs", "M"),
            ("a/top.rs", "?"),
            ("a/b/c/other.rs", "A"),
        ]),
        // 先登记低的、后登记高的 / 先高后低(HashMap 次序不定,两种都放)
        status_map(&[("p/q/r/1.rs", "?"), ("p/q/r/2.rs", "D"), ("p/3.rs", "R")]),
        status_map(&[("p/q/r/1.rs", "D"), ("p/q/r/2.rs", "?"), ("p/q/4.rs", "C")]),
        // 目录自身有状态(未跟踪目录 / 子模块)+ 子树里还有更高的:自身优先
        status_map(&[("vendor", "?"), ("vendor/lib/x.rs", "C")]),
        // 结尾带 `/` 的条目、连续 `/`、`/` 开头、项目在仓库子目录里的 `../`
        status_map(&[
            ("newdir/", "?"),
            ("a//b.rs", "M"),
            ("/abs/c.rs", "D"),
            ("../outside/e.rs", "C"),
            ("../f.rs", "A"),
        ]),
        // 同名前缀的兄弟不许串
        status_map(&[("src/a.rs", "?"), ("srcx/b.rs", "C"), ("src.bak/c.rs", "D")]),
    ];
    for status in &cases {
        assert_same_as_scan(status);
    }

    // 伪随机:5 个段名 × 1..=4 层深度 × 8 种字母,200 张表。LCG 定种,失败可复现
    let segs = ["a", "b", "src", "deep", "x"];
    let labels = ["M", "A", "D", "R", "?", "C", "X", ""];
    let mut seed: u64 = 0x5eed;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n
    };
    for _ in 0..200 {
        let mut status = HashMap::new();
        for _ in 0..next(30) {
            let depth = 1 + next(4);
            let path: Vec<&str> = (0..depth).map(|_| segs[next(segs.len())]).collect();
            status.insert(path.join("/"), labels[next(labels.len())].to_string());
        }
        assert_same_as_scan(&status);
    }
}

fn git_file(path: &str, status: mt_project::git::GitStatus) -> mt_project::git::GitFileStatus {
    let label = match status {
        mt_project::git::GitStatus::Modified => "M",
        mt_project::git::GitStatus::Added => "A",
        mt_project::git::GitStatus::Deleted => "D",
        mt_project::git::GitStatus::Renamed => "R",
        mt_project::git::GitStatus::Untracked => "?",
        mt_project::git::GitStatus::Conflicted => "C",
    };
    mt_project::git::GitFileStatus {
        path: path.to_string(),
        old_path: None,
        status,
        status_label: label.to_string(),
    }
}

/// 两种分隔符的输入落到同一口径:状态表的键(后端给 `/`,万一带 `\` 也收)
/// 与行的 rel(Windows 项目是 `D:\proj\src` 这种反斜杠路径,POSIX / 远程是 `/`)
/// 都换成 `/` 分隔的相对路径,汇总照样查得到。
#[test]
fn 目录汇总认两种分隔符() {
    use mt_project::git::GitStatus;
    let table = git_status_table(vec![
        git_file("src\\deep\\a.rs", GitStatus::Modified),
        git_file("src/b.rs", GitStatus::Untracked),
        git_file("docs\\c.md", GitStatus::Conflicted),
    ]);
    assert!(table.contains_key("src/deep/a.rs"));
    assert!(table.contains_key("docs/c.md"));
    assert_same_as_scan(&table);
    let labels = GitLabels::new(table);

    // Windows 项目根 + 反斜杠行路径
    let win = "D:\\proj";
    assert_eq!(row_rel("D:\\proj\\src\\deep", win), "src/deep");
    assert_eq!(
        labels.label_for(&row_rel("D:\\proj\\src", win), true),
        Some(("M".into(), true))
    );
    assert_eq!(
        labels.label_for(&row_rel("D:\\proj\\src\\deep", win), true),
        Some(("M".into(), true))
    );
    assert_eq!(
        labels.label_for(&row_rel("D:\\proj\\src\\deep\\a.rs", win), false),
        Some(("M".into(), false))
    );
    assert_eq!(
        labels.label_for(&row_rel("D:\\proj\\docs", win), true),
        Some(("C".into(), true))
    );
    // POSIX 项目根 + 正斜杠行路径,查的是同一张表
    assert_eq!(
        labels.label_for(&row_rel("/p/src", "/p"), true),
        Some(("M".into(), true))
    );
    assert_eq!(
        labels.label_for(&row_rel("/p/src/b.rs", "/p"), false),
        Some(("?".into(), false))
    );
    assert_eq!(labels.label_for(&row_rel("/p/tests", "/p"), true), None);
}

// ─── store → 重画的闸 ─────────────────────────────────────

fn project_config(id: &str, path: &str) -> ProjectConfig {
    ProjectConfig::new(id, format!("{id}-name"), path)
}

/// 签名的全部输入。`sig()` 与 `FileTree::store_signature` 走同一个函数。
struct SigInput {
    active: Option<String>,
    project: Option<ProjectConfig>,
    fingerprint: Option<u64>,
    config: AppConfig,
    entries: HashMap<PathBuf, Vec<FileEntry>>,
    expanded: HashSet<String>,
    kinds: HashMap<String, Option<ProjectKind>>,
}

impl SigInput {
    /// `/p` 下:src(展开,内有 core 目录)、docs(折叠,内有 guide 目录)、
    /// target(被忽略)、readme.md。
    fn base() -> Self {
        Self {
            active: Some("p1".into()),
            project: Some(project_config("p1", "/p")),
            fingerprint: None,
            config: AppConfig {
                editors: vec![mt_config::EditorConfig {
                    name: "VS Code".into(),
                    command: "code".into(),
                }],
                ..AppConfig::default()
            },
            entries: listed(vec![
                (
                    "/p",
                    vec![
                        entry("src", "/p/src", true, false),
                        entry("docs", "/p/docs", true, false),
                        entry("target", "/p/target", true, true),
                        entry("readme.md", "/p/readme.md", false, false),
                    ],
                ),
                (
                    "/p/src",
                    vec![
                        entry("core", "/p/src/core", true, false),
                        entry("main.rs", "/p/src/main.rs", false, false),
                    ],
                ),
                (
                    "/p/docs",
                    vec![entry("guide", "/p/docs/guide", true, false)],
                ),
            ]),
            expanded: ["/p/src".to_string()].into_iter().collect(),
            kinds: HashMap::new(),
        }
    }

    fn sig(&self) -> u64 {
        store_render_signature(
            self.active.as_deref(),
            self.project.as_ref(),
            self.fingerprint,
            &self.config,
            &self.entries,
            &|key| self.expanded.contains(key),
            &|key| self.kinds.get(key).copied(),
        )
    }
}

/// render 读的每一项变了,指纹都要变 —— 漏一项那一项就不会跟着刷。
#[test]
fn 签名覆盖渲染依赖的每一项() {
    let base = SigInput::base();
    let base_sig = base.sig();
    assert_eq!(base_sig, SigInput::base().sig(), "同样的输入指纹必须稳定");

    type Mutation = (&'static str, fn(&mut SigInput));
    let mutations: &[Mutation] = &[
        ("换活动项目 id", |s| s.active = Some("p2".into())),
        ("没有活动项目", |s| {
            s.active = None;
            s.project = None;
        }),
        ("项目改名(头部标题)", |s| {
            s.project.as_mut().unwrap().name = "renamed".into()
        }),
        ("项目根路径", |s| {
            s.project.as_mut().unwrap().path = "/q".into()
        }),
        ("变成远程项目", |s| {
            s.project.as_mut().unwrap().ssh_connection_id = Some("ssh-1".into())
        }),
        ("连接指纹(连接配置原地改了)", |s| {
            s.fingerprint = Some(42)
        }),
        ("新增编辑器", |s| {
            s.config.editors.push(mt_config::EditorConfig {
                name: "Zed".into(),
                command: "zed".into(),
            })
        }),
        ("编辑器改名", |s| {
            s.config.editors[0].name = "Cursor".into()
        }),
        ("默认编辑器", |s| {
            s.config.default_editor = Some("VS Code".into())
        }),
        ("界面字号", |s| s.config.ui_font_size += 1.0),
        ("界面字族", |s| {
            s.config.ui_font_family = Some("Inter".into())
        }),
        ("主题", |s| s.config.theme = "light".into()),
        ("外置主题包", |s| {
            s.config.custom_theme_id = Some("nord".into())
        }),
        ("界面语言", |s| s.config.locale = Some("en".into())),
        ("展开一个可见的折叠目录", |s| {
            s.expanded.insert("/p/docs".into());
        }),
        ("折叠一个展开的目录", |s| {
            s.expanded.remove("/p/src");
        }),
        ("展开可见的二级目录", |s| {
            s.expanded.insert("/p/src/core".into());
        }),
        ("一级子目录探出技术栈", |s| {
            s.kinds.insert("/p/src".into(), Some(ProjectKind::Rust));
        }),
    ];
    for (what, mutate) in mutations {
        let mut input = SigInput::base();
        mutate(&mut input);
        assert_ne!(input.sig(), base_sig, "{what} 没改变指纹");
    }
}

/// render 不读的东西变了,指纹不许变 —— 这正是本闸要挡掉的那批 notify。
#[test]
fn 签名不受无关变化影响() {
    let base_sig = SigInput::base().sig();

    type Mutation = (&'static str, fn(&mut SigInput));
    let mutations: &[Mutation] = &[
        ("终端字号", |s| s.config.terminal_font_size += 2.0),
        ("三栏尺寸", |s| {
            s.config.layout_sizes = Some(vec![200.0, 800.0])
        }),
        ("提示音开关", |s| {
            s.config.ai_completion_sound = !s.config.ai_completion_sound
        }),
        ("终端跟随主题", |s| {
            s.config.terminal_follow_theme = !s.config.terminal_follow_theme
        }),
        ("项目描述", |s| {
            s.project.as_mut().unwrap().description = Some("desc".into())
        }),
        ("关联 SSH 范围", |s| {
            s.project.as_mut().unwrap().ssh_connection_ids = Some(vec!["a".into()])
        }),
        // 折叠着的 docs 底下的展开记录:画不出来,不算
        ("不可见目录的展开态", |s| {
            s.expanded.insert("/p/docs/guide".into());
        }),
        // 技术栈徽标只给一级、未被忽略的目录
        ("二级目录的技术栈", |s| {
            s.kinds.insert("/p/src/core".into(), Some(ProjectKind::Go));
        }),
        ("被忽略目录的技术栈", |s| {
            s.kinds.insert("/p/target".into(), Some(ProjectKind::Rust));
        }),
        // 「探过但识别不出」与「还没探」画出来一样
        ("探过但识别不出", |s| {
            s.kinds.insert("/p/src".into(), None);
        }),
    ];
    for (what, mutate) in mutations {
        let mut input = SigInput::base();
        mutate(&mut input);
        assert_eq!(input.sig(), base_sig, "{what} 不该改变指纹");
    }
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
