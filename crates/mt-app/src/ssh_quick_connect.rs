//! 终端右键「SSH 连接 ▸」悬停展开的**快速连接面板**。
//!
//! ```text
//! 终端右键 ─→ 菜单项「SSH 连接」悬停 / →
//!              └─ menu.rs 的自绘子菜单挂载点(MenuItem::submenu_element)
//!                   └─ SshQuickPanel(本模块):搜索框 + 分组列表 + 底栏
//! ```
//!
//! 之前这里是一张纯文字子菜单:分组小标题 + 连接名。名字相近的连接(「prod」
//! 「prod-2」)光看名字分不出连的是哪台机器,连接一多还得一路往下找。面板补了三样:
//!
//! - **每行带出 `user@host:port` 与认证方式**(私钥 / 已存密码两枚小签),认机器
//!   不必再开 SSH 管理面板对照;
//! - **悬停即聚焦的搜索框**:名字走与项目切换器同一个 [`fuzzy_match`](带高亮),
//!   `user@host:port` 与分组名走子串匹配,↑↓ 选、↵ 连;
//! - **分组可折叠**:点标题收起 / 展开,折叠态落盘
//!   (`AppConfig::ssh_menu_collapsed_groups`),下次右键还是那个样子 —— 面板实体
//!   每次开菜单都新建,只记在面板上等于每次右键都得重折一遍;
//! - **底栏直达「新建连接 / 管理连接」**,一条连接都没有时给一张能点的空态卡,
//!   而不是一项置灰的「SSH 连接(暂无)」。
//!
//! # 焦点与键盘
//!
//! 菜单基件的焦点只有菜单容器一个(见 [`crate::menu`] 模块注释),本面板是**借走
//! 焦点**的那一类自绘子面板:每次画出来都把焦点交给搜索框([`SshQuickPanel::claim_focus`]),
//! 输入法才有落点。借走之后的分工:
//!
//! - ↑ / ↓ 走 action(`SshQuickPrev/Next`,谓词 `"SshQuickConnect > Input"`,
//!   为什么必须与 `Input` 同深度见 `project_switcher` 模块注释);
//! - ↵ 订阅 `InputEvent::PressEnter`;
//! - Esc 由单行输入框以 `cx.propagate()` 冒到菜单容器,整个菜单关闭、焦点还给终端;
//! - 菜单容器在焦点被借走期间**只认 Esc**,本面板收起(悬停到别的菜单项)时由菜单
//!   把焦点收回 —— 这两条在 `menu.rs` 里(`owns_focus` / `reclaim_focus`)。
//!
//! # 分桶口径
//!
//! 照抄原子菜单的**就地分桶**(按连接表原序读下来,未分组桶留在它自然出现的位置),
//! 与三个 SSH 弹窗用的 [`crate::ssh_conn::build_group_buckets`]「具名组在前、未分组
//! 恒在最后」**不是同一个口径**,别混用。

use std::cell::RefCell;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::SshConnection;
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::i18n::t;
use crate::menu;
use crate::project_switcher::{fuzzy_match, next_cursor};
use crate::ssh_conn::connection_summary;
use crate::store::AppStore;
use crate::ui;

actions!(
    mini_term,
    [
        /// 上一条连接(↑)。绑定在 `"SshQuickConnect > Input"` 上,见模块注释。
        SshQuickPrev,
        /// 下一条连接(↓)。
        SshQuickNext,
    ]
);

/// 面板宽度 / 列表最大高度(13px 基准,用时过 `ui::font_px`)。与命令库浮层同宽:
/// 两块都是「搜索 + 分组 + 名字配一行等宽摘要」,并排看是同一家的东西。
const PANEL_WIDTH: f32 = 372.0;
const LIST_MAX_HEIGHT: f32 = 360.0;
/// 行首服务器图标的底座边长 / 图标尺寸。
const TILE: f32 = 26.0;
const TILE_ICON: f32 = 14.0;

// ─── 图标(几何原样搬 Lucide,ISC 许可;画法与 `git_ui::icons` 同) ───────────

const LUCIDE: (f32, f32, f32) = (0.0, 0.0, 24.0);
const STROKE: f32 = 1.3 / 16.0;

const fn u(v: f32) -> f32 {
    v / 24.0
}

const fn path(d: &'static str) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Path {
            d,
            view: LUCIDE,
            even_odd: false,
        },
    )
}

const fn ring(cx: f32, cy: f32, r: f32) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(cx), u(cy)),
            r: u(r),
        },
    )
}

const fn rect(x: f32, y: f32, w: f32, h: f32, round: f32) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(x),
            y: u(y),
            w: u(w),
            h: u(h),
            round: u(round),
        },
    )
}

/// 实心小圆点(Lucide 里 `h.01` 那种靠圆头线帽成点的替身)。
const fn dot(cx: f32, cy: f32) -> Shape {
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (u(cx), u(cy)),
            r: u(1.4),
        },
    )
}

/// 服务器(`server`)。
const ICON_SERVER: &[Shape] = &[
    rect(2.0, 2.0, 20.0, 8.0, 2.0),
    rect(2.0, 14.0, 20.0, 8.0, 2.0),
    dot(6.0, 6.0),
    dot(6.0, 18.0),
];
/// 私钥(`key-round`)。
const ICON_KEY: &[Shape] = &[
    path(
        "M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z",
    ),
    dot(16.5, 7.5),
];
/// 已存密码(`lock`)。
const ICON_LOCK: &[Shape] = &[
    rect(3.0, 11.0, 18.0, 11.0, 2.0),
    path("M7 11V7a5 5 0 0 1 10 0v4"),
];
/// 搜索(`search`)。
const ICON_SEARCH: &[Shape] = &[ring(11.0, 11.0, 8.0), path("m21 21-4.3-4.3")];
/// 新建(`plus`)。
const ICON_PLUS: &[Shape] = &[path("M5 12h14"), path("M12 5v14")];
/// 分组展开 / 折叠(`chevron-down` / `chevron-right`)。
const ICON_CHEVRON_DOWN: &[Shape] = &[path("m6 9 6 6 6-6")];
const ICON_CHEVRON_RIGHT: &[Shape] = &[path("m9 18 6-6-6-6")];
/// 管理(`settings-2`)。
const ICON_MANAGE: &[Shape] = &[
    path("M20 7h-9"),
    path("M14 17H5"),
    ring(17.0, 17.0, 3.0),
    ring(7.0, 7.0, 3.0),
];

// ─── 菜单项 ───────────────────────────────────────────────────

/// 终端右键里的「SSH 连接」—— 悬停展开快速连接面板。**恒在**:一条连接都没有时
/// 面板是一张带「新建连接」的空态卡(原版那一项置灰的「SSH 连接(暂无)」点不动,
/// 用户得自己去边条找 SSH 管理面板)。
///
/// 面板实体懒建一次、缓存在闭包里(理由同 `branch_family::view_branches_menu_item`:
/// `submenu_element` 每次菜单重绘都调一遍,每次新建等于每帧丢掉搜索词与游标)。
/// 缓存活到这一次菜单关闭为止,悬停别处再回来,搜索词还在。
pub fn menu_entry(store: Entity<AppStore>, pty_id: u32) -> menu::MenuEntry {
    let cached: RefCell<Option<Entity<SshQuickPanel>>> = RefCell::new(None);
    menu::MenuItem::new(t("terminal", "sshConnect"))
        .submenu_element(move |window, cx| {
            let panel = cached
                .borrow_mut()
                .get_or_insert_with(|| {
                    let store = store.clone();
                    cx.new(|cx| SshQuickPanel::new(store, pty_id, window, cx))
                })
                .clone();
            SshQuickPanel::claim_focus(&panel, window, cx);
            panel.into_any_element()
        })
        .into()
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 未分组那一桶在折叠表(`AppConfig::ssh_menu_collapsed_groups`)里的键。
/// 组名 trim 后不可能是空串(空串即未分组),所以不会与具名组撞上。
const UNGROUPED_KEY: &str = "";

/// 面板里的一个分组桶(已按搜索词过滤,搜不中的空桶不出现)。
///
/// 不派生 `PartialEq`:`SshConnection` 住在 mt-core,不带这个 derive。
#[derive(Debug, Clone)]
pub struct QuickBucket {
    /// `None` = 未分组。
    pub group: Option<String>,
    /// 折叠着:只画标题,`rows` 为空。
    pub collapsed: bool,
    /// 这一组一共几条(不看折叠与搜索)。
    pub total: usize,
    pub rows: Vec<QuickRow>,
}

impl QuickBucket {
    /// 折叠表里的键。
    pub fn key(&self) -> &str {
        self.group.as_deref().unwrap_or(UNGROUPED_KEY)
    }

    /// 标题上的数:折叠着报全组条数,否则报眼下列出来的条数(搜索时 = 命中数)。
    pub fn count(&self) -> usize {
        if self.collapsed {
            self.total
        } else {
            self.rows.len()
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuickRow {
    pub conn: SshConnection,
    /// 名字上的命中字符下标(**char** 口径);命中落在 `user@host` / 分组上时为空
    /// —— 与项目切换器「分组路径命中不高亮名字」同一条。
    pub hits: Vec<usize>,
}

/// 一条连接的认证方式小签。都没有(走 ssh-agent / 连接时手输密码)就不画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBadge {
    /// 配了私钥文件(`ssh -i`)。
    Key,
    /// 存了密码(连接时自动填充)。
    Password,
}

/// 归一化分组名:trim 后空串视为未分组(与 `ssh_conn::normalize_group` 同)。
fn group_of(conn: &SshConnection) -> Option<&str> {
    conn.group.as_deref().map(str::trim).filter(|g| !g.is_empty())
}

/// 按 `group` 归类,**保持首次出现顺序**且未分组桶留在它自然出现的位置
/// (口径见模块注释「分桶口径」)。
fn buckets_in_order(connections: &[SshConnection]) -> Vec<(Option<String>, Vec<SshConnection>)> {
    let mut buckets: Vec<(Option<String>, Vec<SshConnection>)> = Vec::new();
    for conn in connections {
        let group = group_of(conn).map(str::to_string);
        match buckets.iter_mut().find(|(g, _)| *g == group) {
            Some((_, items)) => items.push(conn.clone()),
            None => buckets.push((group, vec![conn.clone()])),
        }
    }
    buckets
}

/// 大小写不敏感的子串匹配。
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// 一条连接对查询词的命中:名字(子序列,带高亮)→ `user@host:port`(子串)→
/// 分组名(子串),先中先算。`None` = 不中。
///
/// 后两者**不用子序列**:摘要串又长又全是 `.`、数字与常见字母,子序列匹配在上面
/// 几乎逢查必中(搜「prod」会把 `deploy@db-root.example.com` 也捞出来)。用户搜
/// 主机时敲的就是 IP / 域名的一截,子串才是预期。
fn match_conn(conn: &SshConnection, query: &str) -> Option<Vec<usize>> {
    if let Some(hits) = fuzzy_match(&conn.name, query) {
        return Some(hits);
    }
    if contains_ci(&connection_summary(conn), query) {
        return Some(Vec::new());
    }
    group_of(conn)
        .filter(|group| contains_ci(group, query))
        .map(|_| Vec::new())
}

/// 连接表 + 搜索词 + 折叠表 → 面板要画的桶。空查询 = 全都要、都不高亮、保持原序。
///
/// 折叠两条例外:**搜索时不生效**(折起来的组里搜得中的也要列出来,同命令库
/// 浮层);**不画分组标题时不生效**(全是未分组连接,标题都没有,折起来就再也
/// 点不开了 —— 折叠表里可能还留着当初有具名组时折的那个空串)。
pub fn quick_buckets(
    connections: &[SshConnection],
    query: &str,
    collapsed: &[String],
) -> Vec<QuickBucket> {
    let query = query.trim();
    let foldable = query.is_empty() && shows_group_headers(connections);
    buckets_in_order(connections)
        .into_iter()
        .filter_map(|(group, conns)| {
            let total = conns.len();
            let key = group.as_deref().unwrap_or(UNGROUPED_KEY);
            let is_collapsed = foldable && collapsed.iter().any(|k| k == key);
            let rows: Vec<QuickRow> = if is_collapsed {
                Vec::new()
            } else {
                conns
                    .into_iter()
                    .filter_map(|conn| {
                        let hits = if query.is_empty() {
                            Vec::new()
                        } else {
                            match_conn(&conn, query)?
                        };
                        Some(QuickRow { conn, hits })
                    })
                    .collect()
            };
            (is_collapsed || !rows.is_empty()).then_some(QuickBucket {
                group,
                collapsed: is_collapsed,
                total,
                rows,
            })
        })
        .collect()
}

/// 切换一组的折叠态,顺带把已经不存在的组(改名 / 解散 / 连接删光)从折叠表里
/// 摘掉 —— 不摘的话,哪天再建一个同名组,它一出生就是折着的。
pub fn toggle_collapsed(collapsed: &mut Vec<String>, key: &str, connections: &[SshConnection]) {
    collapsed.retain(|k| {
        connections
            .iter()
            .any(|c| group_of(c).unwrap_or(UNGROUPED_KEY) == k.as_str())
    });
    match collapsed.iter().position(|k| k == key) {
        Some(pos) => {
            collapsed.remove(pos);
        }
        None => collapsed.push(key.to_string()),
    }
}

/// 画不画分组标题。按**整张连接表**判(不按过滤后的结果):搜索时标题不该跟着
/// 结果一会儿有一会儿没。全是未分组连接时一个标题都不画 —— 原版
/// `bucket.group || hasNamedGroup` 那条。
pub fn shows_group_headers(connections: &[SshConnection]) -> bool {
    connections.iter().any(|c| group_of(c).is_some())
}

/// 行尾的认证小签,私钥在前。
pub fn auth_badges(conn: &SshConnection) -> Vec<AuthBadge> {
    let mut badges = Vec::new();
    if conn
        .identity_file
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
    {
        badges.push(AuthBadge::Key);
    }
    if conn.password.as_deref().is_some_and(|p| !p.is_empty()) {
        badges.push(AuthBadge::Password);
    }
    badges
}

// ─── 面板 ─────────────────────────────────────────────────────

pub struct SshQuickPanel {
    store: Entity<AppStore>,
    /// 连到哪个终端。
    pty_id: u32,
    query: Entity<InputState>,
    /// 键盘游标:过滤后**连接行**里的第几条(分组标题不算)。
    cursor: usize,
    /// 下一帧把游标那一行滚进视口。只由键盘挪游标时置位 —— 悬停挪游标时那一行
    /// 本来就在鼠标底下,再滚一次只会让列表在手底下跳。
    reveal_cursor: bool,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl SshQuickPanel {
    fn new(
        store: Entity<AppStore>,
        pty_id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("terminal", "sshSearchPlaceholder"))
        });
        // `Enter` 不走 action:单行输入框的 `Enter` 处理器无条件
        // `cx.emit(InputEvent::PressEnter)`,订阅它比抢绑定直白(同项目切换器)
        let sub = cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.cursor = 0;
                    this.reveal_cursor = true;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.commit(window, cx),
                _ => {}
            },
        );
        Self {
            store,
            pty_id,
            query,
            cursor: 0,
            reveal_cursor: false,
            scroll: ScrollHandle::new(),
            _subs: vec![sub],
        }
    }

    /// 面板每次被画出来都调一次:焦点不在搜索框上就排一手聚焦。
    ///
    /// 必须**每次**判而不是只在建实体时聚焦一次:悬停到别的菜单项时面板收起、
    /// 菜单把焦点收回容器(`menu.rs` 的 `reclaim_focus`),再悬停回来时面板复用
    /// 同一个实体,得重新把焦点要回来。聚焦排到 `defer` 里:这会儿正在画菜单,
    /// 画到一半改焦点没有必要冒这个险(`prompt::autofocus` 同一手法)。
    ///
    /// 一条连接都没有时不画搜索框,焦点留在菜单容器上(Esc 照样管用)。
    fn claim_focus(panel: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let (input, store) = {
            let panel = panel.read(cx);
            (panel.query.clone(), panel.store.clone())
        };
        if store.read(cx).ssh_connections().is_empty() {
            return;
        }
        if input.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        window.defer(cx, move |window, cx| {
            input.update(cx, |state, cx| state.focus(window, cx));
        });
    }

    /// 当前要画的桶(每帧现算:连接表是几十条量级,缓存反而要失效通道)。
    fn buckets(&self, cx: &App) -> Vec<QuickBucket> {
        let query = self.query.read(cx).value().to_string();
        let store = self.store.read(cx);
        quick_buckets(
            store.ssh_connections(),
            &query,
            &store.config().ssh_menu_collapsed_groups,
        )
    }

    /// 点分组标题:折叠 / 展开这一组并落盘(下次右键还是这个样子)。
    ///
    /// 游标尽量**留在原来那条连接上** —— 折起来的是它上面的组时,下标会整体前移,
    /// 只按下标留着就会跳到别的连接上;它自己被折进去了才回到第一条。
    fn toggle_group(&mut self, key: String, cx: &mut Context<Self>) {
        let flat_ids = |buckets: Vec<QuickBucket>| -> Vec<String> {
            buckets
                .into_iter()
                .flat_map(|b| b.rows)
                .map(|r| r.conn.id)
                .collect()
        };
        let current = flat_ids(self.buckets(cx)).into_iter().nth(self.cursor);
        self.store.update(cx, |store, cx| {
            let connections = store.ssh_connections().to_vec();
            store.patch_config(
                |config| toggle_collapsed(&mut config.ssh_menu_collapsed_groups, &key, &connections),
                cx,
            );
        });
        let after = flat_ids(self.buckets(cx));
        self.cursor = current
            .and_then(|id| after.iter().position(|a| *a == id))
            .unwrap_or(0);
        cx.notify();
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len: usize = self.buckets(cx).iter().map(|b| b.rows.len()).sum();
        self.cursor = next_cursor(self.cursor, len, delta);
        self.reveal_cursor = true;
        cx.notify();
    }

    /// 连游标所在那一条。
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows: Vec<QuickRow> = self.buckets(cx).into_iter().flat_map(|b| b.rows).collect();
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let Some(row) = rows.into_iter().nth(cursor) else {
            return;
        };
        self.connect(row.conn, window, cx);
    }

    /// 在目标终端里连这条 SSH。
    ///
    /// 顺序与 `branch_family` 的节点点击同一条理由:**先收菜单**(把焦点还给终端),
    /// 动作 defer 出去 —— 收菜单会连带 drop 本面板(实体被菜单项的闭包持有),
    /// 在自己的 listener 里同步跑后续动作等于站在正在塌的楼上。
    fn connect(&mut self, conn: SshConnection, window: &mut Window, cx: &mut Context<Self>) {
        let pty_id = self.pty_id;
        window.defer(cx, move |window, cx| {
            crate::pane::connect_ssh(pty_id, conn, window, cx);
        });
        menu::close(window, cx);
    }

    /// 底栏 / 空态卡那几颗按钮:收菜单,再开 SSH 管理面板。开弹窗也必须排在收菜单
    /// **之后** —— 菜单关闭时的焦点还原会把弹窗刚抢到的焦点覆盖掉。
    fn open_ssh_panel(adding: bool, window: &mut Window, cx: &mut App) {
        window.defer(cx, move |window, cx| {
            if adding {
                crate::ssh_panel::open_adding(None, window, cx);
            } else {
                crate::ssh_panel::open(window, cx);
            }
        });
        menu::close(window, cx);
    }

    // ─── 渲染 ─────────────────────────────────────────────

    /// 分组标题:整行可点,折叠 / 展开这一组。搜索时折叠不生效,标题就不该像个
    /// 能点的东西 —— 箭头隐去(留着占位,组名不左右跳)、不挂点击(同命令库浮层)。
    fn render_group_header(
        &self,
        bucket: &QuickBucket,
        searching: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = bucket.key().to_string();
        let name: SharedString = match &bucket.group {
            Some(g) => g.clone().into(),
            None => t("terminal", "ungrouped").into(),
        };
        let chevron = if bucket.collapsed {
            ICON_CHEVRON_RIGHT
        } else {
            ICON_CHEVRON_DOWN
        };
        div()
            .id(SharedString::from(format!("ssh-quick-grp-{key}")))
            .flex()
            .items_center()
            .gap(px(6.0))
            .mx(px(4.0))
            .mt(px(2.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(4.0))
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .when(!searching, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_secondary()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.toggle_group(key.clone(), cx);
                    }))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(11.0))
                    .h(px(11.0))
                    .when(searching, |el| el.opacity(0.0))
                    .child(VectorIcon::new(chevron, px(11.0)).ink(ui::text_muted())),
            )
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(div().flex_none().child(bucket.count().to_string()))
    }

    /// 认证小签:图标 + 一两个字,弱化描边,不抢名字的视线。
    fn render_badge(badge: AuthBadge) -> impl IntoElement {
        let (shapes, label) = match badge {
            AuthBadge::Key => (ICON_KEY, t("terminal", "sshAuthKey")),
            AuthBadge::Password => (ICON_LOCK, t("terminal", "sshAuthPassword")),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(3.0))
            .px(px(5.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(10.0))
            .text_color(ui::text_muted())
            .child(VectorIcon::new(shapes, px(10.0)).ink(ui::text_muted()))
            .child(label)
    }

    fn render_row(
        &self,
        row: &QuickRow,
        idx: usize,
        is_cursor: bool,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let conn = row.conn.clone();

        // 名字 + 命中高亮。没有命中时走单个 truncate 块(长名字收省略号);有命中时
        // 逐段发元素(同项目切换器,见 `ui::highlight_runs`)
        let name = if row.hits.is_empty() {
            div()
                .min_w_0()
                .truncate()
                .text_color(ui::text_primary())
                .child(SharedString::from(conn.name.clone()))
        } else {
            let ranges: Vec<(usize, usize)> = row.hits.iter().map(|i| (*i, *i + 1)).collect();
            let mut line = div().flex().items_center().min_w_0().overflow_hidden();
            for (text, hit) in ui::highlight_runs(&conn.name, &ranges) {
                line = line.child(
                    div()
                        .flex_none()
                        .when(hit, |el| {
                            el.text_color(ui::accent()).font_weight(FontWeight::SEMIBOLD)
                        })
                        .when(!hit, |el| el.text_color(ui::text_primary()))
                        .child(SharedString::from(text)),
                );
            }
            line
        };

        div()
            .id(SharedString::from(format!("ssh-quick-{}", conn.id)))
            .flex()
            .items_center()
            .gap(px(10.0))
            .mx(px(4.0))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .when(is_cursor, |el| el.bg(ui::accent_subtle()))
            // 面板底色就是 `bg_overlay`,悬停再刷它等于没刷,改用 `border_subtle`
            // (同命令库浮层)
            .when(!is_cursor, |el| el.hover(|el| el.bg(ui::border_subtle())))
            // 悬停把游标挪过来:高亮只有一处,不会「键盘选一行、鼠标悬停另一行」两处亮
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if *hovered && this.cursor != idx {
                    this.cursor = idx;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.connect(conn.clone(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(TILE))
                    .h(px(TILE))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if is_cursor {
                        ui::accent_muted()
                    } else {
                        ui::border_subtle()
                    })
                    .child(VectorIcon::new(ICON_SERVER, px(TILE_ICON)).ink(if is_cursor {
                        ui::accent()
                    } else {
                        ui::text_secondary()
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(name.text_size(ui::font_px(12.5)))
                    .child(
                        div()
                            .truncate()
                            // 与终端同一字族:`"monospace"` 这种泛型族名在 Windows 的
                            // DirectWrite 下解析不到,会静默回落成界面字体
                            .font_family(mono)
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(SharedString::from(connection_summary(&row.conn))),
                    ),
            )
            .children(
                auth_badges(&row.conn)
                    .into_iter()
                    .map(Self::render_badge),
            )
    }

    fn text_button(
        id: &'static str,
        shapes: &'static [Shape],
        label: &'static str,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_secondary())
            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
            .child(VectorIcon::new(shapes, px(12.0)).ink(ui::text_secondary()))
            .child(label)
    }

    fn render_footer(&self) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .px(px(6.0))
            .py(px(5.0))
            .border_t_1()
            .border_color(ui::border_subtle())
            .child(
                Self::text_button("ssh-quick-new", ICON_PLUS, t("terminal", "sshNewConnection"))
                    .on_click(|_: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        Self::open_ssh_panel(true, window, cx);
                    }),
            )
            .child(
                Self::text_button("ssh-quick-manage", ICON_MANAGE, t("terminal", "sshManage"))
                    .on_click(|_: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        Self::open_ssh_panel(false, window, cx);
                    }),
            )
            .child(
                div()
                    .ml_auto()
                    .pr(px(6.0))
                    .text_size(ui::font_px(10.5))
                    .text_color(ui::text_muted())
                    .whitespace_nowrap()
                    .child(t("terminal", "sshFooterHint")),
            )
    }

    /// 一条连接都没有:图标 + 两行说明 + 一颗实打实的「新建连接」。
    fn render_empty(&self) -> impl IntoElement {
        div()
            .px(px(20.0))
            .py(px(24.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(44.0))
                    .h(px(44.0))
                    .rounded(px(10.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(ui::border_subtle())
                    .child(VectorIcon::new(ICON_SERVER, px(22.0)).ink(ui::text_secondary())),
            )
            .child(
                div()
                    .pt(px(4.0))
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(t("terminal", "sshEmpty")),
            )
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .text_center()
                    .child(t("terminal", "sshEmptyHint")),
            )
            .child(
                div()
                    .id("ssh-quick-empty-new")
                    .mt(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(14.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(ui::accent())
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::accent())
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::accent_subtle()))
                    .child(VectorIcon::new(ICON_PLUS, px(12.0)).ink(ui::accent()))
                    .child(t("terminal", "sshNewConnection"))
                    .on_click(|_: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        Self::open_ssh_panel(true, window, cx);
                    }),
            )
    }
}

impl Render for SshQuickPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let connections = store.ssh_connections();
        let empty = connections.is_empty();
        let show_headers = shows_group_headers(connections);
        let mono = crate::pane_preview::preview_style(store).font_family;
        let searching = !self.query.read(cx).value().trim().is_empty();
        let buckets = self.buckets(cx);
        let total: usize = buckets.iter().map(|b| b.rows.len()).sum();
        // 候选变少之后游标可能越界(同项目切换器)
        let cursor = self.cursor.min(total.saturating_sub(1));

        let panel = div()
            // 方向键的上下文锚点。键位表在 `hotkeys.rs`,谓词是
            // `"SshQuickConnect > Input"` —— 为什么非这么写见模块注释
            .key_context("SshQuickConnect")
            .on_action(cx.listener(|this, _: &SshQuickPrev, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SshQuickNext, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .w(ui::font_px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            // 浮层用 `bg_overlay`(主题包下恒不透明),与菜单面板同一张底
            .bg(ui::bg_overlay())
            .shadow_lg()
            .text_size(ui::font_px(12.0))
            // 面板挂在菜单项里,菜单面板已经 occlude 了;这里再挡一道,
            // 免得列表滚动区上的按下穿到底下去(同 `branch_family`)
            .occlude();

        if empty {
            return panel.child(self.render_empty());
        }

        let mut list = div()
            .id("ssh-quick-list")
            .track_scroll(&self.scroll)
            .w_full()
            .max_h(ui::font_px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .py(px(4.0))
            .flex()
            .flex_col();

        if buckets.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(20.0))
                    .text_center()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("terminal", "sshNoMatch")),
            );
        } else {
            let mut flat_idx = 0usize;
            // 列表容器的第几个子元素(分组标题也占位),给 `scroll_to_item` 用
            let mut child_idx = 0usize;
            let mut cursor_child = None;
            for bucket in &buckets {
                if show_headers {
                    list = list.child(self.render_group_header(bucket, searching, cx));
                    child_idx += 1;
                }
                for row in &bucket.rows {
                    let is_cursor = flat_idx == cursor;
                    if is_cursor {
                        cursor_child = Some(child_idx);
                    }
                    list = list.child(self.render_row(row, flat_idx, is_cursor, mono.clone(), cx));
                    flat_idx += 1;
                    child_idx += 1;
                }
            }
            if self.reveal_cursor {
                self.reveal_cursor = false;
                if let Some(ix) = cursor_child {
                    self.scroll.scroll_to_item(ix);
                }
            }
        }

        panel
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(8.0))
                    .pb(px(4.0))
                    .child(
                        Input::new(&self.query)
                            .cleanable(false)
                            .prefix(VectorIcon::new(ICON_SEARCH, px(13.0)).ink(ui::text_muted())),
                    ),
            )
            .child(list)
            .child(self.render_footer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, group: Option<&str>) -> SshConnection {
        SshConnection {
            id: format!("id-{name}"),
            name: name.to_string(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: group.map(str::to_string),
            extra: Default::default(),
        }
    }

    fn at(name: &str, user: &str, host: &str, port: u16, group: Option<&str>) -> SshConnection {
        SshConnection {
            user: user.into(),
            host: host.into(),
            port,
            ..conn(name, group)
        }
    }

    /// 不折叠任何组时的视图(大半用例只关心分桶与匹配)。
    fn view(list: &[SshConnection], query: &str) -> Vec<QuickBucket> {
        quick_buckets(list, query, &[])
    }

    fn names(buckets: &[QuickBucket]) -> Vec<Vec<&str>> {
        buckets
            .iter()
            .map(|b| b.rows.iter().map(|r| r.conn.name.as_str()).collect())
            .collect()
    }

    /// 分桶保持**首次出现顺序**,未分组桶留在它自然出现的位置
    /// (与三个 SSH 弹窗那份「未分组恒在最后」的口径**不同**,别混用)。
    #[test]
    fn 按首次出现顺序分桶() {
        let list = vec![
            conn("a", None),
            conn("b", Some("内网")),
            conn("c", None),
            conn("d", Some("内网")),
            conn("e", Some("客户A")),
        ];
        let buckets = view(&list, "");
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].group, None, "未分组桶先出现就排第一");
        assert_eq!(buckets[1].group.as_deref(), Some("内网"));
        assert_eq!(buckets[2].group.as_deref(), Some("客户A"));
        assert_eq!(names(&buckets), vec![vec!["a", "c"], vec!["b", "d"], vec!["e"]]);
        assert!(
            buckets.iter().flat_map(|b| &b.rows).all(|r| r.hits.is_empty()),
            "空查询不高亮"
        );
    }

    /// 空白组名视为未分组(与 `normalizeGroup` 同),且不触发分组标题。
    #[test]
    fn 空白组名算未分组() {
        let list = vec![conn("a", Some("  ")), conn("b", Some(""))];
        let buckets = view(&list, "");
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].group, None);
        assert!(!shows_group_headers(&list), "全是未分组时一个标题都不画");
        assert!(shows_group_headers(&[conn("a", None), conn("b", Some("内网"))]));
    }

    /// 名字走子序列匹配并带高亮(char 口径)。
    #[test]
    fn 名字模糊匹配带高亮() {
        let list = vec![conn("prod-api", None), conn("staging", None)];
        let buckets = view(&list, "pa");
        assert_eq!(names(&buckets), vec![vec!["prod-api"]]);
        assert_eq!(buckets[0].rows[0].hits, vec![0, 5]);

        let buckets = view(&[conn("生产数据库", None)], "数据");
        assert_eq!(buckets[0].rows[0].hits, vec![2, 3]);
    }

    /// `user@host:port` 与分组名走**子串**匹配,命中时不高亮名字。
    #[test]
    fn 主机用户分组走子串匹配() {
        let list = vec![
            at("甲", "root", "10.0.0.5", 2222, None),
            at("乙", "deploy", "db.example.com", 22, Some("客户A")),
        ];
        let buckets = view(&list, "10.0.0");
        assert_eq!(names(&buckets), vec![vec!["甲"]]);
        assert!(buckets[0].rows[0].hits.is_empty(), "命中在摘要上,不高亮名字");

        assert_eq!(names(&view(&list, ":2222")), vec![vec!["甲"]]);
        assert_eq!(names(&view(&list, "DEPLOY@")), vec![vec!["乙"]], "大小写不敏感");
        assert_eq!(names(&view(&list, "客户")), vec![vec!["乙"]]);

        // 摘要上不做子序列:d..b..e 散落在 deploy@db.example.com 里也不算中
        let buckets = view(&list, "dbe");
        assert!(buckets.is_empty(), "实际: {:?}", names(&buckets));
    }

    /// 过滤后空桶整个不出现,查询词首尾空白不算数。
    #[test]
    fn 过滤后空桶不出现() {
        let list = vec![conn("a", Some("组一")), conn("bb", Some("组二"))];
        let buckets = view(&list, "  bb ");
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].group.as_deref(), Some("组二"));
        assert!(view(&list, "zzz").is_empty());
    }

    /// 折叠的组只出标题(`rows` 为空、标题报全组条数),位置不变,别的组照常。
    #[test]
    fn 折叠的组只出标题() {
        let list = vec![
            conn("a", None),
            conn("b", Some("内网")),
            conn("c", Some("内网")),
            conn("d", Some("客户A")),
        ];
        let buckets = quick_buckets(&list, "", &["内网".to_string(), String::new()]);
        assert_eq!(
            buckets.iter().map(|b| (b.key(), b.collapsed, b.count())).collect::<Vec<_>>(),
            vec![("", true, 1), ("内网", true, 2), ("客户A", false, 1)],
            "未分组桶记空串,同样折得起来"
        );
        assert!(buckets[0].rows.is_empty() && buckets[1].rows.is_empty());
        assert_eq!(names(&buckets), vec![vec![], vec![], vec!["d"]]);
    }

    /// 搜索时折叠不生效:折起来的组里搜得中的照样列出来,搜不中的组整个不出现。
    #[test]
    fn 搜索时折叠不生效() {
        let list = vec![conn("prod", Some("内网")), conn("dev", Some("客户A"))];
        let collapsed = vec!["内网".to_string(), "客户A".to_string()];
        let buckets = quick_buckets(&list, "prod", &collapsed);
        assert_eq!(names(&buckets), vec![vec!["prod"]]);
        assert!(!buckets[0].collapsed);
        assert_eq!(buckets[0].count(), 1);
    }

    /// 全是未分组连接时不画标题,折叠表里残留的空串不能把整张列表折没。
    #[test]
    fn 没有分组标题时折叠不生效() {
        let list = vec![conn("a", None), conn("b", None)];
        let buckets = quick_buckets(&list, "", &[String::new()]);
        assert_eq!(names(&buckets), vec![vec!["a", "b"]]);
        assert!(!buckets[0].collapsed);
    }

    /// 切换折叠:有就摘、没有就加,顺带清掉已经不存在的组。
    #[test]
    fn 切换折叠并清掉失效的组() {
        let list = vec![conn("a", None), conn("b", Some("内网"))];
        let mut collapsed = vec!["早就解散了".to_string(), "内网".to_string()];
        toggle_collapsed(&mut collapsed, "", &list);
        assert_eq!(collapsed, vec!["内网".to_string(), String::new()]);
        toggle_collapsed(&mut collapsed, "内网", &list);
        assert_eq!(collapsed, vec![String::new()]);
        // 组名带首尾空白的连接:按 trim 后的名字认
        let spaced = vec![conn("c", Some("  内网 "))];
        let mut collapsed = vec!["内网".to_string()];
        toggle_collapsed(&mut collapsed, "其它", &spaced);
        assert_eq!(collapsed, vec!["内网".to_string(), "其它".to_string()]);
    }

    /// 认证小签:私钥在前、密码在后;空白路径 / 空密码不算配了。
    #[test]
    fn 认证小签() {
        let mut c = conn("a", None);
        assert!(auth_badges(&c).is_empty());
        c.identity_file = Some("   ".into());
        c.password = Some(String::new());
        assert!(auth_badges(&c).is_empty(), "空白私钥路径 / 空密码等于没配");
        c.identity_file = Some("~/.ssh/id_ed25519".into());
        c.password = Some("enc:v1:xxxx".into());
        assert_eq!(auth_badges(&c), vec![AuthBadge::Key, AuthBadge::Password]);
    }
}
