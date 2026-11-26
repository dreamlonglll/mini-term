use super::*;

#[gpui::test]
fn local_repo_discovery_failure_clears_previous_repositories(cx: &mut gpui::TestAppContext) {
    cx.skip_drawing();
    cx.update(gpui_component::init);
    let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    cx.update_window(window.into(), |_, window, cx| {
        let panel = cx.new(|cx| GitPanel::new(store, window, cx));
        panel.update(cx, |this, cx| {
            this.project_context = Some(GitContext::local("/local-project"));
            this.repos = vec![GitRepoInfo {
                name: "old-repo".into(),
                path: "/local-project/old-repo".into(),
                current_branch: Some("main".into()),
                is_worktree: false,
            }];
            this.selected_repo = "/local-project/old-repo".into();
            this.repo_discovery_failed(&anyhow::anyhow!("discovery failed"), cx);
            this.push_repo_down(cx);
            assert!(this.repos.is_empty());
            assert!(this.selected_repo.is_empty());
            assert!(this.repo_context().is_none());
            assert!(this.load_error.is_none(), "local discovery remains silent");
        });
    })
    .unwrap();
    cx.run_until_parked();
}

/// 分支徽标的点击跑在面板自己的 update 里,这时打开分支弹窗不能再读面板 ——
/// v1.13.31 的 `BranchPopup::new` 里 `panel.read` 撞上借用,panic 落在窗口回调里
/// 不能展开,点一下徽标进程就 abort。
#[gpui::test]
fn branch_popup_opens_from_inside_panel_update(cx: &mut gpui::TestAppContext) {
    use crate::prompt::{close_guarded, is_open, kind};

    cx.skip_drawing();
    cx.update(gpui_component::init);
    let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    cx.update_window(window.into(), |_, window, cx| {
        let panel = cx.new(|cx| GitPanel::new(store, window, cx));
        panel.update(cx, |this, cx| this.open_branch_popup(window, cx));
        assert!(is_open(kind::GIT_BRANCHES));
        close_guarded(kind::GIT_BRANCHES, window, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// 折叠补间:起止两端精确,中途两块各自线性走,和不跑出 [起点和, 终点和] 之间。
#[test]
fn 区块折叠的_grow_补间端点精确() {
    // 两块都开(0.4 / 0.6)→ 折起「更改」(0 / 1)
    let from = (0.4, 0.6);
    let target = (0.0, 1.0);
    assert_eq!(blend_grows(from, target, 0.0), from);
    assert_eq!(blend_grows(from, target, 1.0), target);
    assert_eq!(blend_grows(from, target, 1.7), target, "越界钳住");
    let (c, h) = blend_grows(from, target, 0.5);
    assert!((c - 0.2).abs() < 1e-6 && (h - 0.8).abs() < 1e-6, "{c} {h}");
    assert!((c + h - 1.0).abs() < 1e-6, "两端和都是 1,中途也是");
    // 只开着「提交历史」再把它折起(0 / 1 → 0 / 0):grow 之和小于 1 时 flex 只分
    // 那一部分富余,高度随之连续收到 0
    let (c, h) = blend_grows((0.0, 1.0), (0.0, 0.0), 0.25);
    assert_eq!(c, 0.0);
    assert!((h - 0.75).abs() < 1e-6);
}

/// 横幅开合只在「同一个仓库、数据早已到位」时播;换仓库 / 数据刚到 / 抽屉重开直接到位。
#[test]
fn 横幅只在同仓库数据到位时播() {
    let seen = ("D:/repo".to_string(), true);
    assert!(banner_animates(Some(&seen), "D:/repo", true));
    assert!(!banner_animates(Some(&seen), "D:/other", true), "换了仓库");
    assert!(
        !banner_animates(Some(&seen), "D:/repo", false),
        "数据被清空(换项目)"
    );
    let unloaded = ("D:/repo".to_string(), false);
    assert!(
        !banner_animates(Some(&unloaded), "D:/repo", true),
        "数据刚到"
    );
    assert!(!banner_animates(None, "D:/repo", true), "抽屉刚重新打开");
}

/// 比例钳在 0.15~0.85。
#[test]
fn 区块比例钳位() {
    assert_eq!(clamp_ratio(0.5), 0.5);
    assert_eq!(clamp_ratio(0.0), 0.15);
    assert_eq!(clamp_ratio(-3.0), 0.15);
    assert_eq!(clamp_ratio(1.0), 0.85);
    assert_eq!(clamp_ratio(0.15), 0.15);
    assert_eq!(clamp_ratio(0.85), 0.85);
}

/// 「在终端中打开」:项目根仓库**不带** cwd 覆盖,子仓库/worktree 才带。
/// 判据是分隔符统一 + 去尾部分隔符后的字符串比较。
#[test]
fn 项目根仓库不带_cwd_覆盖() {
    let project = r"D:\Git\mini-term";
    // 完全相同
    assert!(is_project_root_repo(r"D:\Git\mini-term", project));
    // 只差一个尾部反斜杠 —— 仍然算同一个,不该带覆盖
    assert!(is_project_root_repo(r"D:\Git\mini-term\", project));
    // 多个尾部分隔符也要吃掉
    assert!(is_project_root_repo(r"D:\Git\mini-term\\", project));
    assert!(is_project_root_repo("/home/u/proj//", "/home/u/proj"));
    // libgit2 workdir 的形态(正斜杠 + 尾杠)—— 旧口径只去尾巴,这条判成
    // 「不是根」,Windows 上项目根仓库的「在终端中打开」会带覆盖并改页签名
    assert!(is_project_root_repo("D:/Git/mini-term/", project));
    // 子仓库:必须带覆盖
    assert!(!is_project_root_repo(r"D:\Git\mini-term\sub", project));
    assert!(!is_project_root_repo("D:/Git/mini-term/sub/", project));
}

/// 仓库出处只在「有信息量」时才给:项目根自己、项目的直接子目录都没有。
#[test]
fn 浅层仓库不补出处() {
    let project = r"D:\Git\washStatsion";
    // 项目根自己就是仓库
    assert_eq!(repo_detail(project, Some(project)), None);
    // libgit2 的 workdir 是正斜杠 + 尾部分隔符,归一化后仍要认出是同一个
    assert_eq!(repo_detail("D:/Git/washStatsion/", Some(project)), None);
    // 直接子目录:同一个父目录下叶子名天然唯一,补出来是空信息
    assert_eq!(repo_detail(r"D:\Git\washStatsion\web", Some(project)), None);
    assert_eq!(repo_detail("/home/u/proj/web", Some("/home/u/proj")), None);
}

/// monorepo 里的同名仓库靠父级路径区分 —— 这正是「一堆 api」那个场景。
#[test]
fn 同名仓库靠父级路径区分() {
    let project = r"D:\Git\mono";
    assert_eq!(
        repo_detail(r"D:\Git\mono\services\api", Some(project)),
        Some("services".to_string())
    );
    assert_eq!(
        repo_detail(r"D:\Git\mono\legacy\api", Some(project)),
        Some("legacy".to_string())
    );
    // 更深的层级把整条父级路径都给出来,两条出处必然不同
    assert_ne!(
        repo_detail(r"D:\Git\mono\a\x\api", Some(project)),
        repo_detail(r"D:\Git\mono\b\x\api", Some(project))
    );
    assert_eq!(
        repo_detail(r"D:\Git\mono\packages\tools\api", Some(project)),
        Some("packages/tools".to_string())
    );
}

/// 项目落在某个仓库的子目录里时(`discover_repo_limited` 向上找到的那种),
/// 仓库不在项目内 —— 给绝对路径,别只剩一个没有出处的叶子名。
#[test]
fn 项目外的仓库给绝对路径() {
    assert_eq!(
        repo_detail(r"D:\Git\mini-term", Some(r"D:\Git\mini-term\crates")),
        Some(native_path_display("D:/Git/mini-term"))
    );
    // 前缀只是「字符串像」不算在项目内:mono2 不是 mono 的子目录
    assert_eq!(
        repo_detail(r"D:\Git\mono2\api", Some(r"D:\Git\mono")),
        Some(native_path_display("D:/Git/mono2/api"))
    );
    // 还没挂上项目路径时同理(空路径不能当前缀,否则谁都算项目内)
    assert_eq!(
        repo_detail(r"D:\Git\mono\api", None),
        Some(native_path_display("D:/Git/mono/api"))
    );
    // 空仓库路径没什么可说的
    assert_eq!(repo_detail("", Some(r"D:\Git\mono")), None);
}

/// detached HEAD 的 `current_branch` 是 `"(1a2b3c4)"` —— 它**不在**
/// `get_repo_branches` 的结果里,所以 `viewBranch` 永远取不到它,
/// 也就永远不会被当分支名传给 `get_git_log`(那会 `bail!`)。
#[test]
fn detached_head_不当分支名查询() {
    let branches = [
        BranchInfo {
            name: "main".into(),
            is_head: false,
            is_remote: false,
            commit_hash: "abc".into(),
            upstream: Some("origin/main".into()),
        },
        BranchInfo {
            name: "origin/main".into(),
            is_head: false,
            is_remote: true,
            commit_hash: "abc".into(),
            upstream: None,
        },
    ];
    let detached = "(1a2b3c4)";
    assert!(
        !branches.iter().any(|b| b.name == detached),
        "括号短 hash 绝不会出现在分支列表里"
    );
    // viewBranch 只从分支列表里取 → 括号短 hash 进不去
    let picked: Option<&str> = branches
        .iter()
        .find(|b| b.name == detached)
        .map(|b| b.name.as_str());
    assert!(picked.is_none());
    assert!(is_detached_label(detached));
    assert!(!is_detached_label("main"));
    assert!(!is_detached_label("(wip"), "只有成对括号才算");
}

/// 游离 HEAD 的说明文案自己带括号:套进去的只能是剥掉括号的哈希,否则成
/// 「（(1a2b3c4)）」(E2E r4 A6)。两种语言都只剩一层括号。
#[test]
fn 游离提示只有一层括号() {
    assert_eq!(detached_hash("(1a2b3c4)"), "1a2b3c4");
    assert_eq!(
        detached_hash("main"),
        "main",
        "不是 detached 显示名的原样返回"
    );
    assert_eq!(detached_hash("(wip"), "(wip");
    for locale in crate::i18n::Locale::ALL {
        for key in ["detachedNote", "tipDetached"] {
            let template = mt_i18n::lookup(locale, "gitBranch", key).expect("文案在");
            let text = mt_i18n::interpolate(template, &[("commit", detached_hash("(1a2b3c4)"))]);
            assert!(text.contains("1a2b3c4"), "{text}");
            assert!(
                !text.contains("((") && !text.contains("（(") && !text.contains(")）"),
                "{locale}.{key}: {text}"
            );
        }
    }
    let tip = branch_tip_lines("(1a2b3c4)", None, true, false);
    assert!(!tip[0].contains('('), "{tip:?}");
}

/// 发现仓库回来:选中仓库没变、分支快照已有或在途时不再重取 —— 重取会作废「刷新」
/// 同时发出的那一趟,横幅要等扫盘做完再从头取(E2E r4 A11 横幅 3.3s 才消失)。
#[test]
fn 发现仓库后不作废在途的分支快照() {
    // 刷新:选中没变,分支快照在途 / 已到
    assert!(!branches_stale_after_discovery("D:/r", "D:/r", false, true));
    assert!(!branches_stale_after_discovery("D:/r", "D:/r", true, true));
    assert!(!branches_stale_after_discovery(
        "D:/r", "D:/r", false, false
    ));
    // 切项目吃了缓存:选中没变,但一份都没取过
    assert!(branches_stale_after_discovery("D:/r", "D:/r", true, false));
    // 选中仓库换了(原先的不见了 / 第一次发现)
    assert!(branches_stale_after_discovery("", "D:/r", true, false));
    assert!(branches_stale_after_discovery("D:/a", "D:/b", false, true));
}

/// 远程 Git helper 装好之后:只有停在错误上的远程项目面板重新发现仓库(E2E:装好了面板
/// 也恢复不了,只能重启);本地项目、没报错的面板不动。
#[test]
fn helper_装好后只重试报错的远程面板() {
    assert!(needs_retry_after_helper_installed(false, true));
    assert!(!needs_retry_after_helper_installed(false, false), "没报错");
    assert!(!needs_retry_after_helper_installed(true, true), "本地项目");
    assert!(!needs_retry_after_helper_installed(true, false));
}

fn sig(refs: u64, index: u64) -> RepoSignature {
    RepoSignature { refs, index }
}

fn after(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

/// pty 命中:`.git` 监听挂在选中仓库上才走签名,否则回落两个去抖。
#[test]
fn pty_命中挂着监听时改走签名() {
    assert_eq!(pty_route(true), PtyRoute::Signature);
    assert_eq!(pty_route(false), PtyRoute::Debounce);
}

/// 签名的差别:全同不刷;只有索引变了走轻量那条(不重取分支快照);引用变了或读不到
/// 一边整轮刷。
#[test]
fn 只动了索引走轻量路径() {
    let s = sig(1, 10);
    assert_eq!(sig_verdict(Some(s), Some(s)), SigVerdict::Unchanged);
    assert_eq!(
        sig_verdict(Some(s), Some(sig(1, 11))),
        SigVerdict::IndexOnly
    );
    assert_eq!(sig_verdict(Some(s), Some(sig(2, 10))), SigVerdict::Refs);
    assert_eq!(sig_verdict(Some(s), Some(sig(2, 11))), SigVerdict::Refs);
    assert_eq!(sig_verdict(None, Some(s)), SigVerdict::Refs, "还没有快照");
    assert_eq!(sig_verdict(Some(s), None), SigVerdict::Refs, "读不到当变了");
}

/// 检查结果的处置:不比手上的新就丢;快照在途就压着等它落地;否则当场比。
#[test]
fn 检查结果按先后处置() {
    let t0 = Instant::now();
    // 从没有过已知:在途就压着,不在途就比
    assert_eq!(check_fate(t0, None, false), CheckFate::Compare);
    assert_eq!(check_fate(t0, None, true), CheckFate::Defer);
    // 手上那份(或最近一轮整轮重取)晚于这次检查开读:丢,即便快照在途
    assert_eq!(
        check_fate(after(t0, 10), Some(after(t0, 20)), true),
        CheckFate::Covered
    );
    assert_eq!(
        check_fate(after(t0, 20), Some(after(t0, 20)), false),
        CheckFate::Covered,
        "同一刻:整轮重取读到的不早于它"
    );
    // 检查更新:写完操作后刷新已发出、快照还没回来 —— 压着,别当场判「变了」整轮再刷
    assert_eq!(check_fate(after(t0, 500), Some(t0), true), CheckFate::Defer);
    assert_eq!(
        check_fate(after(t0, 500), Some(t0), false),
        CheckFate::Compare
    );
}

/// 快照落地:手上那份签名是它发出之后读的才沿用(面板刚写完索引 / 检查刚比过)。
#[test]
fn 快照落地不拿旧签名盖新的() {
    let t0 = Instant::now();
    assert!(!keep_newer_observation(None, t0));
    assert!(
        !keep_newer_observation(Some(t0), after(t0, 1)),
        "手上的更旧"
    );
    assert!(!keep_newer_observation(Some(t0), t0), "同一刻按快照的");
    assert!(keep_newer_observation(Some(after(t0, 5)), t0));
}

/// 面板自己写完索引:引用那半一致、且不比手上的旧才收;读不到不收。
#[test]
fn 自己写完索引记作已知() {
    let t0 = Instant::now();
    let observed = |at: Instant, s: Option<RepoSignature>| RepoObservation {
        at,
        signature: s,
        op: None,
    };
    let known = Some(sig(1, 10));
    // 常态:暂存了一个文件,只有索引变了
    assert!(absorbs_own_write(
        known,
        Some(t0),
        &observed(after(t0, 5), Some(sig(1, 11)))
    ));
    assert!(absorbs_own_write(
        known,
        None,
        &observed(t0, Some(sig(1, 11)))
    ));
    // 期间别处动了引用:留给检查去判(要整轮刷)
    assert!(!absorbs_own_write(
        known,
        Some(t0),
        &observed(after(t0, 5), Some(sig(2, 11)))
    ));
    // 手上那份是之后读的:不拿旧的盖
    assert!(!absorbs_own_write(
        known,
        Some(after(t0, 10)),
        &observed(after(t0, 5), Some(sig(1, 11)))
    ));
    // 任一边读不到
    assert!(!absorbs_own_write(
        None,
        None,
        &observed(t0, Some(sig(1, 11)))
    ));
    assert!(!absorbs_own_write(
        known,
        Some(t0),
        &observed(after(t0, 5), None)
    ));
}

/// 端到端推一遍「面板里暂存一次」:写完读的签名被收下之后,紧接着那次 `.git` 检查
/// 读到的同一份签名判成没变 —— 「更改」不再重取第二遍、分支快照一次都不取。
#[test]
fn 面板暂存后紧接着的检查短路() {
    let t0 = Instant::now();
    let before = sig(1, 10);
    let written = sig(1, 11);
    // 已知:上一份快照(t0 发出)
    let mut known = Some(before);
    let mut known_at = Some(t0);
    // 写完那一刻(t0+100)读的
    let own = RepoObservation {
        at: after(t0, 100),
        signature: Some(written),
        op: None,
    };
    assert!(absorbs_own_write(known, known_at, &own));
    known = own.signature;
    known_at = Some(own.at);
    // 500ms 去抖后的检查(t0+600):读到的还是这份
    let check_at = after(t0, 600);
    assert_eq!(check_fate(check_at, known_at, false), CheckFate::Compare);
    assert_eq!(sig_verdict(known, Some(written)), SigVerdict::Unchanged);
}

/// 仓库栏写按钮的提示:面板自己在推送时(没占别人的闸,`gated = false`),其余按钮
/// 同样写「另一项 Git 操作正在进行」(E2E r4 A10 只有提交按钮写了);正在跑的那颗写
/// 「…中」;别的区占着闸时一律等;闲着照常。
#[test]
fn 写按钮忙时都说明原因() {
    use GitBusy::*;
    let push = Some(Push);
    assert_eq!(write_tip(push, false, Some(Push)), WriteTip::Running);
    for op in [Some(Fetch), Some(Pull), None] {
        assert_eq!(write_tip(push, false, op), WriteTip::Waiting, "{op:?}");
        assert_eq!(write_tip(None, true, op), WriteTip::Waiting, "{op:?}");
        assert_eq!(write_tip(None, false, op), WriteTip::Normal, "{op:?}");
    }
}

/// 分支徽章 tooltip:有上游时写出上游与 ↑↓ 的含义(为 0 的那项不写),同步时
/// 说「与上游同步」;没上游 / detached / 查看别的分支各有说法;末行都是操作提示。
#[test]
fn 分支徽章提示逐行() {
    let tracking = BranchTracking {
        branch: "main".into(),
        upstream: "origin/main".into(),
        ahead: 2,
        behind: 0,
    };
    let lines = branch_tip_lines("main", Some(&tracking), false, false);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "main");
    assert!(lines[1].contains("origin/main"));
    assert!(lines[2].contains('2'));
    assert_eq!(lines[3], t("gitBranch", "tipOpenBranches"));

    let synced = BranchTracking {
        ahead: 0,
        ..tracking.clone()
    };
    let lines = branch_tip_lines("main", Some(&synced), false, false);
    assert_eq!(lines[2], t("gitBranch", "tipInSync"));

    let lines = branch_tip_lines("dev", None, false, false);
    assert_eq!(lines[1], t("gitBranch", "tipNoUpstream"));

    let lines = branch_tip_lines("(1a2b3c4)", None, true, false);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("1a2b3c4"));

    let lines = branch_tip_lines("origin/x", Some(&tracking), false, true);
    assert_eq!(lines.len(), 2, "查看别的分支时不列计数");
    assert!(lines[0].contains("origin/x"));
}

/// 「更改」与「提交历史」分高度:放得下时中缝只在两块都不用滚的范围里随比例走;
/// 放不下时矮的那块收成内容高度、富余给另一块;说不准的一块当作要多少给多少。
#[test]
fn 两块高度按内容让位() {
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    // conflict-demo 那一例:更改要 600、历史只要 244,共 900,比例 0.5 ——
    // 历史让出富余,更改整块摊开不必再滚(中缝挪到 600)
    assert!(close(
        section_split(900.0, 0.5, Some(600.0), Some(244.0)),
        600.0 / 900.0
    ));
    // 反过来:更改只要 200、历史要 800 —— 更改让出富余(中缝挪到 200)
    assert!(close(
        section_split(900.0, 0.5, Some(200.0), Some(800.0)),
        200.0 / 900.0
    ));
    // 更改很少(空态 160)、历史还有下一页(说不准):更改收成内容,其余给历史
    assert!(close(
        section_split(900.0, 0.5, Some(160.0), None),
        160.0 / 900.0
    ));
    // 两块都放得下:比例落在 [c, total - h] 里就照比例
    assert!(close(
        section_split(900.0, 0.5, Some(300.0), Some(200.0)),
        0.5
    ));
    // 两块都放不下:照比例
    assert!(close(
        section_split(900.0, 0.4, Some(2000.0), Some(3000.0)),
        0.4
    ));
    assert!(close(section_split(900.0, 0.4, None, None), 0.4));
    // 还没量出总高:照比例
    assert!(close(section_split(0.0, 0.3, Some(10.0), Some(10.0)), 0.3));

    // 拖中缝时对比例单调(跟手,不会往回跳)
    let mut last = 0.0;
    for step in 0..=100 {
        let ratio = step as f32 / 100.0;
        let split = section_split(900.0, ratio, Some(600.0), Some(244.0));
        assert!(split + 1e-6 >= last, "ratio {ratio}: {split} < {last}");
        last = split;
    }
}

/// 中缝拖到 15% 下限时「更改」区仍不矮于提交区 + 一行列表(E2E r4 B11:总高 700、
/// 提交区约 190,15% 只有 105,提交按钮被裁掉)。放得下时不改比例;面板矮到放不下
/// 时整块给「更改」;还没量出来时照旧。
#[test]
fn 更改区不矮于提交区() {
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let floor = Some(234.0);
    // 每个允许的比例下,「更改」区的高度都 ≥ floor,且对比例单调
    let mut last = 0.0;
    for step in 15..=85 {
        let ratio = clamp_ratio(step as f32 / 100.0);
        let split = keep_changes_floor(
            section_split(700.0, ratio, Some(900.0), Some(1200.0)),
            700.0,
            floor,
        );
        assert!(split * 700.0 + 1e-3 >= 234.0, "ratio {ratio}: {split}");
        assert!(split + 1e-6 >= last, "ratio {ratio}: {split} < {last}");
        last = split;
    }
    // 比例本来就够:原样
    assert!(close(keep_changes_floor(0.5, 700.0, floor), 0.5));
    // 面板矮到放不下 floor:整块给「更改」
    assert!(close(keep_changes_floor(0.15, 200.0, floor), 1.0));
    // 还没量出来
    assert!(close(keep_changes_floor(0.15, 700.0, None), 0.15));
    assert!(close(keep_changes_floor(0.3, 0.0, floor), 0.3));
}

/// 仓库栏计数让位:宽度够放两枚胶囊就画胶囊;不够先收成紧凑字,再不够不画。
/// 门槛是「仓库名、分支名都截到保底之后」还要的宽。
#[test]
fn 计数胶囊按宽度逐档收() {
    let floor = BarFloor {
        repo_name: REPO_NAME_FLOOR,
        detail: false,
        worktree: false,
        busy: false,
        ahead: 2,
        behind: 1,
        scale: 1.0,
    };
    let full = bar_min_width(&floor, SyncBadge::Full);
    let compact = bar_min_width(&floor, SyncBadge::Compact);
    let hidden = bar_min_width(&floor, SyncBadge::Hidden);
    assert!(
        hidden < compact && compact < full,
        "{hidden} {compact} {full}"
    );
    assert_eq!(sync_badge_for(full + 20.0, &floor), SyncBadge::Full);
    assert_eq!(sync_badge_for(full - 1.0, &floor), SyncBadge::Compact);
    assert_eq!(sync_badge_for(compact - 1.0, &floor), SyncBadge::Hidden);
    // 还没量出来(0)先按完整的画
    assert_eq!(sync_badge_for(0.0, &floor), SyncBadge::Full);
    // 没有计数时三档一样宽
    let quiet = BarFloor {
        ahead: 0,
        behind: 0,
        ..floor
    };
    assert_eq!(
        bar_min_width(&quiet, SyncBadge::Full),
        bar_min_width(&quiet, SyncBadge::Hidden)
    );
    // 仓库名本来就短:保底按名字估,不硬撑到 48
    assert!(estimate_text_width("ui", 13.0) < REPO_NAME_FLOOR);
    assert!(estimate_text_width("中文仓库", 13.0) > estimate_text_width("abcd", 13.0));
}

/// 抽屉拖到最窄(240,左段只剩约 67px:按钮组 5 颗 + 贮藏数胶囊占去其余)时左段逐档
/// 让位:先把仓库触发器收成图标,再把分支徽章也收成图标 —— 分支弹窗的入口始终在
/// (E2E r4 A1:最窄时分支胶囊被整个裁掉)。每一档的内容都放得下左段。
#[test]
fn 最窄时分支入口仍在() {
    let floor = BarFloor {
        repo_name: REPO_NAME_FLOOR,
        detail: false,
        worktree: false,
        busy: false,
        ahead: 2,
        behind: 1,
        scale: 1.0,
    };
    let hidden = bar_min_width(&floor, SyncBadge::Hidden);
    assert_eq!(bar_squeeze_for(0.0, &floor), BarSqueeze::None, "还没量出来");
    assert_eq!(bar_squeeze_for(hidden, &floor), BarSqueeze::None);
    let repo_icon = ICON_ONLY_WIDTH + 4.0 + branch_floor_width(&floor);
    assert!(repo_icon < hidden);
    assert_eq!(bar_squeeze_for(hidden - 1.0, &floor), BarSqueeze::RepoIcon);
    assert_eq!(bar_squeeze_for(repo_icon, &floor), BarSqueeze::RepoIcon);
    assert_eq!(bar_squeeze_for(repo_icon - 1.0, &floor), BarSqueeze::Icons);
    // 最窄 240 的左段:两颗图标(26 + 间距 4 + 26)放得下
    let narrowest = 67.0;
    assert_eq!(bar_squeeze_for(narrowest, &floor), BarSqueeze::Icons);
    assert!(ICON_ONLY_WIDTH * 2.0 + 4.0 <= narrowest);
}

/// 仓库路径 tooltip:本机原生分隔符、不带尾部分隔符(libgit2 给的 `D:/x/proj/`
/// 与扫盘给的 `D:\x\proj\sub` 写成同一种样子)。
#[test]
fn 仓库路径按本机写法显示() {
    if cfg!(windows) {
        assert_eq!(
            native_path_display("D:/Git/playground/"),
            r"D:\Git\playground"
        );
        assert_eq!(native_path_display(r"D:\Git\a\sub\"), r"D:\Git\a\sub");
        assert_eq!(native_path_display("D:/"), r"D:\");
    } else {
        assert_eq!(native_path_display("/home/u/proj/"), "/home/u/proj");
    }
}

/// 改分支 / HEAD 的操作才让分支徽章转圈;同步类操作在自己的按钮上转。
#[test]
fn 忙碌态归属() {
    assert!(GitBusy::Checkout.touches_branch());
    assert!(GitBusy::Merge.touches_branch());
    assert!(!GitBusy::Fetch.touches_branch());
    assert!(!GitBusy::Push.touches_branch());
    assert!(!GitBusy::Continue.touches_branch());
}
