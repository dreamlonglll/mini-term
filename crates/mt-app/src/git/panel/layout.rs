use super::*;

/// 两块折叠区的会话级视图状态。**有意不落盘**。
#[derive(Clone, Copy)]
pub(super) struct SectionUi {
    pub(super) changes_open: bool,
    pub(super) history_open: bool,
    pub(super) ratio: f32,
}

thread_local! {
    pub(super) static SECTION_UI: RefCell<SectionUi> = const {
        RefCell::new(SectionUi {
            changes_open: true,
            history_open: true,
            ratio: 0.5,
        })
    };

    /// `src/utils/projectDataCache.ts` 的对应物:后端身份键 → (仓库列表, 选中仓库)。
    /// 换项目回来时先吃缓存再重新发现,避免每次都白屏一下。
    pub(super) static REPO_CACHE: RefCell<HashMap<String, (Vec<GitRepoInfo>, String)>> =
        RefCell::new(HashMap::new());

    /// 拉取方式(仓库栏「拉取」右键菜单里选)。会话级、**不落盘**,左键按它拉;
    /// `None` = 本次会话还没手选(或选回了「跟随 git 配置」):不带方式参数拉,完全交给
    /// 用户的 git 配置([`mt_project::git::git_pull_by_config`]),tooltip 照
    /// [`RepoMeta::pull_config`] 显示「跟随配置:…」。
    pub(super) static PULL_MODE: Cell<Option<PullMode>> = const { Cell::new(None) };
}

/// 区块比例的钳位(`GitHistory.tsx:17`)。
pub fn clamp_ratio(r: f32) -> f32 {
    r.clamp(0.15, 0.85)
}
#[derive(Clone, Copy)]
pub(super) struct SectionDrag {
    pub(super) start_y: Pixels,
    pub(super) start_ratio: f32,
    /// 两块内容加起来的高度(靠 canvas 量出来)。
    pub(super) total: f32,
}
/// 两块内容区怎么分(见模块注释「两块的高度」):返回「更改」区实际占的比例。
///
/// `total` 是两块加起来的高度,`ratio` 是拖拽比例,`changes` / `history` 是两块各自
/// 「全部摊开要多高」(说不准时 `None`,当作要多少给多少)。中缝的位置
/// `b = total × ratio`,再钳进 `[min(c, total − h), max(c, total − h)]`:
///
/// - 两块合起来放得下(`c + h ≤ total`):中缝只在「两块都不用滚」的范围里随比例走,
///   哪块都不会一边滚动、一边让另一块空着;
/// - 放不下:比分到的矮的那块收成内容高度、富余全给另一块,两块都不够时照比例。
///
/// 结果对比例单调,拖中缝时跟手(到了「再拖就有一块要滚 / 留白」的边界才停)。
pub(super) fn section_split(
    total: f32,
    ratio: f32,
    changes: Option<f32>,
    history: Option<f32>,
) -> f32 {
    if total <= 1.0 {
        return ratio;
    }
    let c = changes.unwrap_or(f32::INFINITY);
    let rest = history.map_or(f32::NEG_INFINITY, |h| total - h);
    let (lo, hi) = if c <= rest { (c, rest) } else { (rest, c) };
    (total * ratio).clamp(lo, hi).clamp(0.0, total) / total
}

/// 「更改」区不矮于 `floor`(提交区整块 + 一行列表,见 `GitChanges::min_height`):
/// 比例钳位的 15% 下限按面板总高算,面板不高时还不够放提交区,中缝拖到下限就把提交
/// 按钮裁掉、滚也滚不出来(E2E r4 B11)。面板矮到连 `floor` 都放不下时整块给「更改」,
/// 提交按钮优先于历史区。仍对比例单调。
pub(super) fn keep_changes_floor(split: f32, total: f32, floor: Option<f32>) -> f32 {
    match floor {
        Some(floor) if total > 1.0 => split.max((floor / total).min(1.0)),
        _ => split,
    }
}

/// 两份「想要的高度」算不算同一份(半像素以内当没变,免得量值抖动来回重画)。
pub(super) fn same_heights(a: (Option<f32>, Option<f32>), b: (Option<f32>, Option<f32>)) -> bool {
    let same = |x: Option<f32>, y: Option<f32>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() <= 0.5,
        (None, None) => true,
        _ => false,
    };
    same(a.0, b.0) && same(a.1, b.1)
}

/// 仓库栏 ↑↓ 计数的三种形态。左段挤不下时按「截仓库名 → 截分支名 → 计数收紧 →
/// 计数收进 tooltip」的次序让位(见 [`GitPanel::render_repo_bar`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncBadge {
    /// 两枚胶囊(↑n 绿、↓m 蓝)
    Full,
    /// 一段不带底的紧凑字(`↑2↓1`)
    Compact,
    /// 不画数字(分支徽章的 tooltip 里本来就逐行写着),分支图标右上角留一个小圆点
    /// 提示「有待推送 / 待拉取」(见 [`sync_dot_over`])
    Hidden,
}

/// 仓库名截断后至少留多宽(两个字 + 省略号);名字本来更短就按名字算。默认抽屉宽度下
/// 左段只有 170px 上下,仓库 + 分支两段的图标、留白就占去八十多,保底只能给这么多。
pub(super) const REPO_NAME_FLOOR: f32 = 36.0;

/// 仓库栏左段在「仓库名、分支名都截到保底」时的宽度构成。
#[derive(Debug, Clone, Copy)]
pub(super) struct BarFloor {
    /// 仓库名的保底宽(见 [`REPO_NAME_FLOOR`])
    pub(super) repo_name: f32,
    /// 有没有出处前缀(收到 0 之后还剩一个间距)
    pub(super) detail: bool,
    pub(super) worktree: bool,
    /// 分支徽章旁在转圈
    pub(super) busy: bool,
    pub(super) ahead: usize,
    pub(super) behind: usize,
    /// 界面字号缩放(`ui::font_px(1.0)`)
    pub(super) scale: f32,
}

/// 仓库触发器收到最窄时的宽:左右 6、图标 14、间距 6、(出处收到 0 还剩的)间距 6、
/// 名字保底、(worktree 图标的)间距 6 与 12、间距 6、▾ 12 加起来。各段尺寸照
/// [`GitPanel::render_repo_bar`] 的布局抄,也是触发器的 `min_w`。
pub(super) fn repo_floor_width(f: &BarFloor) -> f32 {
    let detail = if f.detail { 6.0 } else { 0.0 };
    let worktree = if f.worktree { 18.0 } else { 0.0 };
    12.0 + 14.0 + 6.0 + detail + f.repo_name + worktree + 18.0
}

/// 分支徽章(不含计数)收到最窄时的宽:左右 6 + 图标 14 + 间距 6 + 名字胶囊保底 40
/// +(转圈:间距 6 + 12)。
pub(super) fn branch_floor_width(f: &BarFloor) -> f32 {
    12.0 + 14.0 + 6.0 + 40.0 + if f.busy { 18.0 } else { 0.0 }
}

/// ↑↓ 计数按 `badge` 形态占多宽(含与前一项的间距)。胶囊里的数字是 11px 等宽字,
/// 一位按 0.62em 估(偏宽一点)。
pub(super) fn counters_width(f: &BarFloor, badge: SyncBadge) -> f32 {
    let digits = |n: usize| n.to_string().len() as f32 * 11.0 * f.scale * 0.62;
    let counts = [f.ahead, f.behind];
    let counts = counts.iter().filter(|n| **n > 0);
    match badge {
        // 每枚:与前一项的间距 6 + 左 5 + 箭头 10 + 间距 2 + 数字 + 右 7
        SyncBadge::Full => counts.map(|&n| 6.0 + 24.0 + digits(n)).sum(),
        // 一段:间距 6 + 左右 4 + 每项(箭头 10 + 数字)+ 两项之间 3
        SyncBadge::Compact => {
            let items: Vec<f32> = counts.map(|&n| 10.0 + digits(n)).collect();
            if items.is_empty() {
                0.0
            } else {
                14.0 + items.iter().sum::<f32>() + 3.0 * (items.len() - 1) as f32
            }
        }
        SyncBadge::Hidden => 0.0,
    }
}

/// 左段放下全部内容(名字截到保底、计数按 `badge` 形态)要多宽。两段之间的间距 4。
pub(super) fn bar_min_width(f: &BarFloor, badge: SyncBadge) -> f32 {
    repo_floor_width(f) + 4.0 + branch_floor_width(f) + counters_width(f, badge)
}

/// 左段宽 `width` 时计数取哪种形态。`width` 还没量出来(0)时先按完整的画。
pub(super) fn sync_badge_for(width: f32, floor: &BarFloor) -> SyncBadge {
    if width <= 0.0 || width >= bar_min_width(floor, SyncBadge::Full) {
        SyncBadge::Full
    } else if width >= bar_min_width(floor, SyncBadge::Compact) {
        SyncBadge::Compact
    } else {
        SyncBadge::Hidden
    }
}

/// 左段连「名字截到保底、计数收进 tooltip」都放不下(抽屉拖到最窄)时的让位,接在
/// [`SyncBadge`] 三档之后:先把仓库触发器收成只剩文件夹图标,再把分支徽章也收成只剩
/// 分支图标。两个入口始终露着、点得到,名字都在各自的 tooltip 里 —— 此前最窄 240 时
/// 仓库触发器按保底宽占满左段,分支徽章被整个裁掉,分支弹窗没了入口(E2E r4 A1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarSqueeze {
    /// 照常(名字截断到保底)
    None,
    /// 仓库触发器只留文件夹图标
    RepoIcon,
    /// 仓库触发器、分支徽章都只留图标(徽章转圈时圈顶替图标)
    Icons,
}

/// 收成图标的仓库触发器 / 分支徽章的宽:左右 6 + 图标 14。
pub(super) const ICON_ONLY_WIDTH: f32 = 26.0;

/// 左段宽 `width` 时取哪一档让位。`width` 还没量出来(0)时先按照常画。
pub(super) fn bar_squeeze_for(width: f32, floor: &BarFloor) -> BarSqueeze {
    if width <= 0.0 || width >= bar_min_width(floor, SyncBadge::Hidden) {
        BarSqueeze::None
    } else if width >= ICON_ONLY_WIDTH + 4.0 + branch_floor_width(floor) {
        BarSqueeze::RepoIcon
    } else {
        BarSqueeze::Icons
    }
}

/// 仓库路径的展示形态:本机原生分隔符、不带尾部分隔符。libgit2 的 workdir 是
/// `D:/x/proj/`,扫盘找到的子仓库是 `D:\x\proj\sub` —— 不归一的话同一处 tooltip
/// 两种写法。
pub(super) fn native_path_display(path: &str) -> String {
    let trimmed = mt_core::path_key::trim_trailing_separators(path);
    let native = if cfg!(windows) {
        trimmed.replace('/', "\\")
    } else {
        trimmed.to_string()
    };
    // 盘符根(`D:`)要留住分隔符,否则是「D 盘的当前目录」
    if native.len() == 2 && native.ends_with(':') {
        format!("{native}{}", std::path::MAIN_SEPARATOR)
    } else {
        native
    }
}

/// 区块折叠动画这一帧的两块 grow:从点标题那一刻屏上的 `from` 按进度 `p`(已缓动)
/// 补到这一帧现算的 `target`(见模块注释「折叠动画」)。两块各自线性插值 —— 起止两端的
/// grow 之和都是 1(或收起的那块为 0),中途也就落在两者之间,不会忽大忽小。
pub(super) fn blend_grows(from: (f32, f32), target: (f32, f32), p: f32) -> (f32, f32) {
    let p = p.clamp(0.0, 1.0);
    if p >= 1.0 {
        return target;
    }
    (
        from.0 + (target.0 - from.0) * p,
        from.1 + (target.1 - from.1) * p,
    )
}

/// 横幅这次开合要不要播:上一帧看的是**同一个仓库**、且那时与此刻数据都已到位。换了仓库、
/// 刚换项目(数据还没到)、抽屉重新打开(`seen` 被清掉)都直接到位 —— 那是整块上下文换了,
/// 不是「合并开始了 / 结束了」。
pub(super) fn banner_animates(seen: Option<&(String, bool)>, repo: &str, loaded: bool) -> bool {
    seen.is_some_and(|(prev, was_loaded)| prev == repo && *was_loaded && loaded)
}

/// 任意 flex-grow(gpui 的 `flex_grow()` 只会设成 1)。
pub(super) fn grow<E: Styled>(mut el: E, value: f32) -> E {
    el.style().flex_grow = Some(value);
    el
}

/// 任意 flex-shrink(gpui 只有 `flex_shrink()` = 1 与 `flex_shrink_0()`)。
pub(super) fn shrink<E: Styled>(mut el: E, value: f32) -> E {
    el.style().flex_shrink = Some(value);
    el
}
/// SectionHeader(`GitHistory.tsx:69-100`),走共用的 [`git_ui::SectionHeader`]。
/// `bordered` 只有下方「提交历史」用。`trailing` 是右侧的动作位(「更改」的视图切换
/// 按钮住这);共用图标按钮自己拦住冒泡,不会连带触发 header 的折叠。
/// `turns` 是折叠箭头这一帧的旋转(见 `fold_motion`)。
pub(super) fn section_header(
    id: &'static str,
    label: &'static str,
    open: bool,
    turns: f32,
    bordered: bool,
    trailing: Option<AnyElement>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut header = git_ui::SectionHeader::new(id, label)
        .collapsible(open)
        .chevron_turns(turns);
    if let Some(trailing) = trailing {
        header = header.trailing(trailing);
    }
    header
        .build()
        .when(bordered, |el| {
            el.border_t_1().border_color(ui::border_subtle())
        })
        .on_click(on_click)
        .into_any_element()
}

impl GitPanel {
    /// 点了某块的标题(状态已翻成 `open`):grow 从屏上的当前值起补、箭头转过去。
    pub(super) fn start_section_toggle(&mut self, key: &'static str, open: bool) {
        self.grow_from = self.grow_shown;
        self.grow_anim.restart();
        self.chevrons.flip_open(key, open, OPEN_TURNS);
    }

    /// 横幅的开合跟着 `meta.op` 走(见 [`banner_animates`]):状态变了才播,换了上下文直接到位。
    /// 收起动画里内容照 [`Self::banner_op`] 画最后那一条。
    pub(super) fn sync_banner(&mut self) {
        let op = self.meta.op;
        let shown = op.kind.is_some();
        if shown {
            self.banner_op = op;
        }
        let loaded = self.sig_observed_at.is_some();
        if banner_animates(self.banner_seen.as_ref(), &self.selected_repo, loaded) {
            self.banner.set_open(shown);
        } else if self.banner.is_open() != shown || self.banner.is_running() {
            self.banner.jump(shown);
        }
        let stale = self
            .banner_seen
            .as_ref()
            .is_none_or(|(repo, was_loaded)| *repo != self.selected_repo || *was_loaded != loaded);
        if stale {
            self.banner_seen = Some((self.selected_repo.clone(), loaded));
        }
    }

    /// 中缝拖拽手柄(`GitHistory.tsx:516-523`):骑在「提交历史」标题上沿的 6px 绝对定位条
    /// (上下各 3px)。
    ///
    /// ⚠️ 它必须是标题容器里**排在标题之后**的子元素:下半截压在标题上,gpui 的冒泡
    /// 按绘制顺序倒着派,后画的先拿到。原先手柄先画、标题后画 —— 按下时标题先记下一次
    /// 待定点击(手柄的 `stop_propagation` 轮到时已经晚了),拖拽时标题跟着光标走,松手
    /// 仍在标题上,于是「提交历史」被折叠(E2E 截到过)。现在手柄先拿到按下、就地拦住。
    ///
    /// 不用 `occlude` / `block_mouse_except_scroll` 挡身后的命中:拖拽期间光标一直骑在
    /// 手柄上,面板根上那对移动 / 松手监听要「根被悬停」才触发,挡掉就拖不动、也松不开。
    pub(super) fn render_section_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("git-section-handle")
            .absolute()
            .left_0()
            .right_0()
            .top(px(-3.0))
            .h(px(6.0))
            .cursor_row_resize()
            .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.4)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    // 从中缝**眼下**的位置起算:按内容让过位时它不在 `ratio` 那儿,
                    // 按 `ratio` 起算的话一按就跳
                    let ratio = clamp_ratio(this.effective_ratio);
                    SECTION_UI.with(|s| s.borrow_mut().ratio = ratio);
                    this.drag = Some(SectionDrag {
                        start_y: event.position.y,
                        start_ratio: ratio,
                        total: this.sections_height,
                    });
                }),
            )
            .into_any_element()
    }

    /// 左段的保底宽度构成(计数胶囊该取哪种形态的判据,见 [`sync_badge_for`])。
    pub(super) fn bar_floor(&self) -> BarFloor {
        let info = self.selected_repo_info();
        let scale = f32::from(ui::font_px(1.0));
        let name = info.map(|r| r.name.as_str()).unwrap_or_default();
        let viewing =
            self.view_branch.is_some() && self.view_branch.as_deref() != self.current_branch();
        let tracking = if viewing {
            None
        } else {
            self.meta.head_tracking.as_ref()
        };
        BarFloor {
            // 估宽了只是计数早一点收紧:左段本身 `overflow_hidden`,怎么估都不会压到按钮组
            repo_name: REPO_NAME_FLOOR.min(estimate_text_width(name, 13.0 * scale)),
            detail: info.is_some_and(|r| {
                repo_detail(&r.path.to_string_lossy(), self.project_path.as_deref()).is_some()
            }),
            worktree: info.is_some_and(|r| r.is_worktree),
            busy: self.busy.is_some_and(GitBusy::touches_branch),
            ahead: tracking.map_or(0, |t| t.ahead),
            behind: tracking.map_or(0, |t| t.behind),
            scale,
        }
    }
}
