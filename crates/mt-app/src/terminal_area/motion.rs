use super::*;

// ─── 方向性换场(面板切换 / 最大化开合 / 叶内切 tab)────────────
//
// 形态是**整幅 push**(浏览器翻页那种):新内容按方向推入的同时,旧内容同向
// 推出。方向跟随入口控件的排列轴 —— 叶内切 tab 走**左右**(tab 栏横排:往右边
// 的 tab 切,新内容从右进、旧内容向左出);切面板走**上下**(右缘竖条纵排:
// 往下面的面板切,新面板从下进、旧面板向上出);反向一律镜像。内容全程不透明,
// 不用淡入(opacity 从 0 起播读作「闪一下」,用户明确否掉)。
//
// 机制分两半:
// - **状态记录**([`TabSwap`] / [`AreaSwap`]):render 里对比上一帧的
//   「谁在台上」,变了就记一条带方向与序号的过渡、起一只到点摘除的计时器。
//   出场层只在记录在场时渲染 —— 没切换就零包装,也不会因为重挂而误播;
// - **画法**:relative + overflow_hidden 的壳里放两个 absolute 全尺寸层
//   (出场层画旧内容、进场层画新内容),各自补间 `left`(容器宽度的分数,
//   [`push_enter_x`] / [`push_exit_x`]),`with_animation` 的 id 带序号。
//
// ⚠️ 三条红线:
// 1. **零尺寸变化** —— margin/宽高类补间会让 pane 内容框逐帧变宽窄,
//    `TerminalView` 一路 resize 到 PTY(SIGWINCH 刷屏);absolute 层只动
//    `left`,尺寸从头到尾不变,PTY 一次 resize 都不会收到;
// 2. **不过减弱动效的闸,始终播放** —— 与抽屉进出场/pane-enter 同一先例
//    (原版 reduce 段显式豁免这一族换场动画),用户系统开着「减少动画」时
//    这批动画是被点名要保留的;
// 3. **同一个 pane 实体不许同帧双挂** —— 最大化开合的前后两个视图共享同一批
//    pane,出场层只好省略(enter-only);切 tab / 切面板的新旧两侧 pane 天然
//    互斥,才有资格上双层。关掉激活 tab(或把它拖去别的组)时旧 pane 已不在本组,
//    同样只播进场:新激活的 pane 从被关那一侧推入(见 [`tab_swap_plan`])。
//
// 布局变了之后的补位(tab chip 新建 / 关闭 / 拖拽落位的位移补间、关掉分屏后邻居从旧
// 格子扩张到新格子)是另一族,零尺寸变化、按 prepaint 拿到的终位当帧补偿,见 [`fx`]。

/// 分屏进场的滑入偏移(新格子从右滑到位;它没有「旧内容」,不走 push)。
pub(super) const SWAP_SLIDE_PX: f32 = 14.0;
/// 叶内切 tab 的换场时长(比整块略快,切 tab 是高频操作;规格 `TERMINAL_SWAP`)。
pub(super) const TAB_SWAP_MS: u64 = mt_ui::motion::TERMINAL_SWAP.duration.as_millis() as u64;

/// 分屏进场滑入这一帧的偏移量。**纯函数**,单测钉在这上面:终点必须精确归零,
/// 残留半像素会让终端内容永久错位半格。
pub(super) fn swap_slide_offset(delta: f32) -> f32 {
    SWAP_SLIDE_PX * (1.0 - delta.clamp(0.0, 1.0))
}

/// push 进场层这一帧的横向位置(容器宽度的分数)。`dir = +1` 时从 `+1.0`
/// (整幅在右侧屏外)推进到 `0.0`。**纯函数**,端点单测钉住。
pub(super) fn push_enter_x(dir: f32, delta: f32) -> f32 {
    dir * (1.0 - delta.clamp(0.0, 1.0))
}

/// push 出场层这一帧的横向位置。`dir = +1` 时从 `0.0` 推出到 `-1.0`
/// (整幅移出左侧)—— 与进场层首尾相接,像一条整带往左拉。
pub(super) fn push_exit_x(dir: f32, delta: f32) -> f32 {
    -dir * delta.clamp(0.0, 1.0)
}

/// Reveal 裁剪窗的补间:`from → to` 的线性插值(进度已被 easing 处理过)。
/// 端点必须精确到位 —— 起点差一像素是「从错的格子放大」,终点差一像素是
/// 裁剪窗永远盖不满整幅。
pub(super) fn reveal_lerp(from: f32, to: f32, delta: f32) -> f32 {
    let d = delta.clamp(0.0, 1.0);
    from + (to - from) * d
}

/// 最大化 / 还原裁剪窗矩形的插值(进度已被 easing 处理过)。终点严丝合缝返回 `to`。
pub(super) fn reveal_clip_rect(from: RevealFrom, to: RevealFrom, delta: f32) -> RevealFrom {
    let d = delta.clamp(0.0, 1.0);
    if d >= 1.0 {
        return to;
    }
    RevealFrom {
        left: reveal_lerp(from.left, to.left, d),
        top: reveal_lerp(from.top, to.top, d),
        width: reveal_lerp(from.width, to.width, d),
        height: reveal_lerp(from.height, to.height, d),
    }
}

/// 单时钟驱动的裁剪窗展开 / 收回元素(最大化与还原共用)。
///
/// 布局交给全尺寸内容(PTY 零逐帧 resize);prepaint 时将内容左上角通过 `with_element_offset`
/// 钉在裁剪窗左上角 `(clip.left, clip.top)`,再用 `with_content_mask` 裁出当前裁剪窗:
/// 1. 裁剪窗与内容偏移出自同一帧同一个 `Transition` 进度,彻底消除双 `with_animation` 的 1px 抖动;
/// 2. 内容左上角跟随裁剪窗从原位滑到 `(0, 0)`,右/下分屏展开时始终直接展示该终端的 tab 栏与正文,
///    不再露出纯黑空白区。
pub(super) struct ClipReveal {
    child: AnyElement,
    clip: RevealFrom,
}

pub(super) fn clip_reveal(child: impl IntoElement, clip: RevealFrom) -> ClipReveal {
    ClipReveal {
        child: child.into_any_element(),
        clip,
    }
}

impl gpui::Element for ClipReveal {
    type RequestLayoutState = ();
    type PrepaintState = gpui::ContentMask<Pixels>;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mask = gpui::ContentMask {
            bounds: Bounds::new(
                point(
                    bounds.origin.x + px(self.clip.left),
                    bounds.origin.y + px(self.clip.top),
                ),
                gpui::size(px(self.clip.width.max(0.0)), px(self.clip.height.max(0.0))),
            ),
        };
        let offset = point(px(self.clip.left), px(self.clip.top));
        window.with_content_mask(Some(mask), |window| {
            window.with_element_offset(offset, |window| {
                self.child.prepaint(window, cx);
            });
        });
        mask
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        mask: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(*mask), |window| {
            self.child.paint(window, cx);
        });
    }
}
/// 一个分组页签区的横向滚动:滚动句柄 + 激活页签的跟随记录。
#[derive(Default)]
pub(super) struct TabStripScroll {
    pub(super) handle: ScrollHandle,
    pub(super) follow: TabFollow,
}

/// 「激活页签滚进视野」的触发判据。
///
/// 只在激活页签**变了**(换了一个、或同一个被挪了位置)时滚一次:每帧都滚的话,用户自己
/// 横向滚动页签区会被立刻拽回去。变化恰好发生在页签区还没排过版的那一帧(刚建出来的
/// 分组)时先挂着,等排过版再滚 —— gpui 的 `scroll_to_item` 在滚动区头一次排版之前
/// 还不知道它是横向滚动的,当场滚等于白滚。
#[derive(Default, Debug, PartialEq)]
pub(super) struct TabFollow {
    /// 上一次见到的激活页签:(pane id, 组内下标)。
    pub(super) shown: Option<(String, usize)>,
    /// 变化已记下、还没滚成。
    pub(super) pending: bool,
}

impl TabFollow {
    /// 记下这一帧的激活页签;返回这一帧要不要把它滚进视野。
    /// `laid_out`:页签区至少排过一次版。
    pub(super) fn observe(&mut self, active_id: &str, index: usize, laid_out: bool) -> bool {
        let same = self
            .shown
            .as_ref()
            .is_some_and(|(id, i)| id == active_id && *i == index);
        if !same {
            self.shown = Some((active_id.to_string(), index));
            self.pending = true;
        }
        if self.pending && laid_out {
            self.pending = false;
            return true;
        }
        false
    }
}

/// 叶内切 tab 这一次怎么推:`(方向, 出场层画不画旧 pane)`。
///
/// - 旧 pane 还在本组(`old_idx = Some`):普通切 tab,往右边的 tab 切新内容从右进
///   (`+1`),往左镜像;新旧两层都画。
/// - 旧 pane 已不在本组(关掉了激活 tab / 把它拖去了别的组):只播进场,新激活的
///   pane 从**被关那一侧**推入 —— 方向按「从被关的那一格切到它」算:它原先在被关那格
///   的右边(现在顶到了被关的下标上,`new_idx >= old_prev_idx`)就从右进,在左边就从左进。
///   与关面板的「只做进场」同一口径。
///
/// `old_prev_idx` 是旧激活 pane 上一帧在组里的下标,`new_idx` 是新激活 pane 现在的下标。
pub(super) fn tab_swap_plan(
    old_idx: Option<usize>,
    old_prev_idx: usize,
    new_idx: usize,
) -> (f32, bool) {
    match old_idx {
        Some(old) => (if new_idx > old { 1.0 } else { -1.0 }, true),
        None => (if new_idx >= old_prev_idx { 1.0 } else { -1.0 }, false),
    }
}

/// 上一次以展开态画到某个叶子时它的激活 pane(见 [`TerminalArea::last_leaf_active`])。
pub(super) struct LeafActive {
    pub(super) pane_id: String,
    /// 激活 pane 当时在组里的下标。
    pub(super) index: usize,
    /// 记下它的那一轮 render 的代次([`TerminalArea::render_gen`])。
    pub(super) drawn_gen: u64,
}

/// 记录是不是**上一轮(或这一轮)**render 画到这一叶时留下的。
///
/// 不是 = 这一叶有一阵子没以展开态画过:缩在最大化的折叠条里(折叠条不走 `render_leaf`)、
/// 在别的面板 / 项目里。那期间激活项的变化(折叠条上右键关掉激活 tab、从终端列表跳过去)
/// 不是「刚刚发生」的,再画到时只对齐记录、不起过渡 —— 否则还原 / 切回来的同时
/// 这一叶又横推一次。
pub(super) fn leaf_drawn_recently(drawn_gen: u64, current_gen: u64) -> bool {
    drawn_gen.saturating_add(1) >= current_gen
}

/// 叶内切 tab 的一次 push 过渡。
pub(super) struct TabSwap {
    pub(super) seq: u64,
    /// `+1.0` = 目标在右侧(新内容从右推入、旧内容向左推出);`-1.0` 反之。
    pub(super) dir: f32,
    /// 出场层要画的旧 pane。`None` = 旧 pane 已不在本组(关掉了 / 拖走了),只播进场。
    pub(super) old_pane_id: Option<String>,
    /// 到点摘除出场层(丢句柄即取消)。
    pub(super) _timer: Task<()>,
}

/// 整块内容(面板切换 / 最大化开合)的一次换场。折叠条上换铺满组是瞬切,不进这里。
///
/// 统一由单一 [`mt_ui::motion::Transition`] 驱动进度并在播完后由 render 自动收掉,
/// 不再依赖外部定时器与多条独立 `with_animation` 竞速。
pub(super) struct AreaSwap {
    /// Push/Drift 的方向号(`+1` = 从下方推入 / 上浮落位)。Reveal 不用。
    pub(super) dir: f32,
    pub(super) motion: AreaMotion,
    pub(super) exit: AreaExitSrc,
    pub(super) transition: mt_ui::motion::Transition,
}

/// 换场的运动形态。
#[derive(Clone, Copy)]
pub(super) enum AreaMotion {
    /// 整幅竖直 push(切面板)。
    Push,
    /// 14px 竖直落位 —— 只剩兜底路径(算不出矩形时的最大化/还原)。
    Drift,
    /// 最大化:裁剪窗从原格子的矩形**展开**到整幅(`from` 为相对终端区
    /// 原点的旧格矩形)。内容从第 0 帧就按最终全尺寸排版 —— 终端只在真正
    /// 最大化那一下 resize 一次,展开过程零 PTY resize。
    Reveal { from: RevealFrom },
    /// 还原:[`Reveal`](Self::Reveal) 的反向 —— 裁剪窗从整幅**收回**到该格
    /// 还原后的目标矩形(按分屏比例算,见 [`leaf_rect_in`])。飞行层里是旧铺满
    /// 的那一整组(拿 `exit` 的 MaxLeaf 画:tab 栏 + 终端主体 + 折叠区占位,与
    /// [`TerminalArea::render_maximized`] 逐层同高),底下的整树把这一组整个挖空
    /// (`suppress_leaf`)—— 同一个终端实体不许同帧双挂,tab 的焦点句柄与
    /// `tab_rects` 也只许一处在写。
    ///
    /// 最大化状态下**换铺满组刻意没有动画**:整块过渡在满屏尺度上一律读作
    /// 闪烁(push / 淡化都试过,用户点名不要),瞬时切换即可。
    RevealBack { to: RevealFrom },
}

/// Reveal 的起点矩形(相对终端区原点,px)。
#[derive(Clone, Copy)]
pub(super) struct RevealFrom {
    pub(super) left: f32,
    pub(super) top: f32,
    pub(super) width: f32,
    pub(super) height: f32,
}

/// push 出场层画什么。
#[derive(Clone)]
pub(super) enum AreaExitSrc {
    /// 没有出场层(微滑;或旧内容已不可得)。
    None,
    /// 旧面板的整树(切面板)。
    Panel(String),
    /// 还原飞行层:旧铺满的**那一整组**(带 tab 栏)+ 按旧折叠区同高的占位。
    ///
    /// 飞行层里终端的尺寸必须与铺满态一字不差,差出一点就是还原第一帧多一次 PTY
    /// resize、画面跳一下 —— 早先这里只画终端主体、漏了铺满组自己那条 `TAB_BAR_H`
    /// 的 tab 栏,终端凭空高出 30px。现在整组照 `render_leaf` 画,底下的整树把这组
    /// 挖空(见 [`AreaMotion::RevealBack`]),tab 栏只在飞行层上挂一份。
    /// `bar_count` 是铺满时折叠条的条数(= **还原前那一帧**的叶子数 − 1,
    /// 见 [`restore_bar_count`])。
    MaxLeaf { leaf_id: String, bar_count: usize },
}

impl TerminalArea {
    /// 叶内切 tab 的方向性 push 过渡检测(见文件头「换场」注释)。
    ///
    /// **必须在借出 store 之前调** —— 要改 `self`、还要 spawn 计时器,两样都要
    /// `&mut`。返回的是本帧要画的那一场过渡(拷成 owned:store 马上要长借)。
    pub(super) fn note_leaf_tab_swap(
        &mut self,
        leaf_id: &str,
        panes: &[PaneState],
        active_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<(u64, f32, Option<String>)> {
        let animations_enabled = self.animations_enabled(cx);
        let now_gen = self.render_gen;
        let new_idx = panes.iter().position(|p| p.id == active_id);
        match self.last_leaf_active.get(leaf_id) {
            Some(rec) if rec.pane_id != active_id => {
                let (old, old_prev_idx) = (rec.pane_id.clone(), rec.index);
                let recent = leaf_drawn_recently(rec.drawn_gen, now_gen);
                let old_idx = panes.iter().position(|p| p.id == old);
                let swap = new_idx.map(|n| tab_swap_plan(old_idx, old_prev_idx, n));
                if !animations_enabled || !recent {
                    // 开关关着 / 这一叶隔了几轮才再画到(变化不是刚发生的):只对齐记录,
                    // 不起过渡(也把可能在飞的收掉)
                    self.tab_swaps.remove(leaf_id);
                } else if let Some((dir, keeps_old)) = swap {
                    self.swap_seq += 1;
                    let seq = self.swap_seq;
                    let lid = leaf_id.to_string();
                    let timer = cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(TAB_SWAP_MS))
                            .await;
                        let _ = this.update(cx, |area: &mut TerminalArea, cx| {
                            if area.tab_swaps.get(&lid).is_some_and(|s| s.seq == seq) {
                                area.tab_swaps.remove(&lid);
                                cx.notify();
                            }
                        });
                    });
                    self.tab_swaps.insert(
                        leaf_id.to_string(),
                        TabSwap {
                            seq,
                            dir,
                            // 旧 pane 已不在本组(被 × 掉 / 拖去别的组):实体要么已回收、
                            // 要么正画在别的组里 —— 出场层一律不画,只播进场
                            old_pane_id: keeps_old.then_some(old),
                            _timer: timer,
                        },
                    );
                } else {
                    self.tab_swaps.remove(leaf_id);
                }
                match new_idx {
                    Some(n) => {
                        self.last_leaf_active.insert(
                            leaf_id.to_string(),
                            LeafActive {
                                pane_id: active_id.to_string(),
                                index: n,
                                drawn_gen: now_gen,
                            },
                        );
                    }
                    None => {
                        if let Some(rec) = self.last_leaf_active.get_mut(leaf_id) {
                            rec.drawn_gen = now_gen;
                        }
                    }
                }
            }
            Some(_) => {
                // 激活项没换,但它在组里的位置可能挪了(拖拽重排 / 关掉它左边的 tab):
                // 下标跟着记,下次关掉它时才算得出被关的那一侧。代次每轮都刷(原地改,
                // 不分配)
                if let Some(rec) = self.last_leaf_active.get_mut(leaf_id) {
                    if let Some(n) = new_idx {
                        rec.index = n;
                    }
                    rec.drawn_gen = now_gen;
                }
            }
            None => {
                if let Some(n) = new_idx {
                    self.last_leaf_active.insert(
                        leaf_id.to_string(),
                        LeafActive {
                            pane_id: active_id.to_string(),
                            index: n,
                            drawn_gen: now_gen,
                        },
                    );
                }
            }
        }
        self.tab_swaps
            .get(leaf_id)
            .map(|s| (s.seq, s.dir, s.old_pane_id.clone()))
    }
}
