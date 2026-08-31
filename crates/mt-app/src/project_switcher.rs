//! 项目快速切换器(Ctrl+Shift+P)。对应 `src/components/ProjectSwitcher.tsx`。
//!
//! 之前切项目只能用鼠标点侧栏,侧栏还能被折叠起来 —— 折叠状态下键盘完全够不到项目。
//! 匹配同时吃项目名和**分组路径**,「前端/webapp」这类同名项目靠分组区分。
//!
//! # 键盘导航为什么要绕一圈
//!
//! `↑` / `↓` 在 gpui-component 的 `Input` 里是**绑成 action 的**
//! (`KeyBinding::new("up", MoveUp, Some("Input"))`),而 gpui 的按键派发是
//! 「先匹配 action、后跑 key 监听」,且单行输入框的 `MoveUp` 处理器直接 return
//! **不** `cx.propagate()` —— 于是挂在外层容器上的 `on_key_down` 一辈子收不到方向键。
//!
//! 破法是让自己的绑定与 `Input` 的绑定**同深度**:上下文谓词写成
//! `"ProjectSwitcher > Input"`(见 `main.rs` 的键位表)。`depth_of` 对
//! `Descendant` 返回的是最深那一层的深度,与裸 `"Input"` 打平;打平之后
//! `bindings_for_input` 按**注册顺序倒序**决胜负,而壳的 `cx.bind_keys` 跑在
//! `gpui_component::init` 之后 —— 所以是我们先派发,且不 propagate,`MoveUp`
//! 根本轮不到。这条链子有单测钉住(见本模块 `tests::方向键绑定压过输入框自带的`)。
//!
//! `Enter` 不走这条路:单行输入框的 `Enter` 处理器**会** `cx.propagate()` 并且
//! 无条件 `cx.emit(InputEvent::PressEnter)`,订阅它更直白。
//!
//! # Esc 与遮罩
//!
//! 走 `gpui_component::dialog::Dialog` 白拿:Esc(`Dialog` 的 `Cancel` action)、
//! 点遮罩、焦点还原都是它的既有行为,`open_guarded` 再补上防叠开与覆盖物栈登记。

use gpui::{
    App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, actions, div,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::{AppConfig, ProjectTreeItem};

use crate::i18n::t;
use crate::overlay::kind;
use crate::prompt::{autofocus, close_guarded, open_guarded};
use crate::store::AppStore;
use crate::tree::PaneStatus;
use crate::ui;

actions!(
    mini_term,
    [
        /// 上一个候选(↑)。绑定在 `"ProjectSwitcher > Input"` 上,见模块注释。
        SwitcherPrev,
        /// 下一个候选(↓)。
        SwitcherNext,
    ]
);

/// 面板高度估算用的几个常数(与 `render` 里那几个 `px()` 一一对应)。
/// 顶部输入行 `py(10) ×2 + 输入框 ≈ 26`;底部提示条 `py(8) ×2 + 10px 文字 ≈ 17`;
/// 每行 `py(8) ×2 + 12px 主行 + 10px 副行`(行高按 1.5 算)。
const HEADER_H: f32 = 46.0;
const FOOTER_H: f32 = 33.0;
const LIST_PAD: f32 = 8.0;
const ROW_H: f32 = 49.0;
const EMPTY_H: f32 = 66.0;

/// 一个候选项目(已算好分组路径与命中位置)。
struct Row {
    id: String,
    name: String,
    path: String,
    group_path: Vec<String>,
    status: PaneStatus,
    needs_attention: bool,
    /// 名字上的命中字符下标(**char** 口径);分组路径命中时为空 —— 与原版一致
    /// (`fuzzyMatch(path)` 命中时不高亮名字)。
    hits: Vec<usize>,
}

pub struct ProjectSwitcher {
    store: Entity<AppStore>,
    query: Entity<InputState>,
    /// 高亮到第几项。
    cursor: usize,
    _subs: Vec<Subscription>,
}

/// 打开切换器。已经开着时是空操作。
pub fn open(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    // 守卫要在**建视图之前**判:`open_guarded` 拦下来时输入框已经建好、
    // `window.defer` 也排上了聚焦,而它永远不会被画出来 —— 焦点被送进虚空,
    // 终端从此收不到键。与 `prompt::show_prompt` 同一个坑。
    if crate::overlay::contains(crate::overlay::key(kind::PROJECT_SWITCHER)) {
        return;
    }
    let view = cx.new(|cx| ProjectSwitcher::new(store, window, cx));
    let input = view.read(cx).query.clone();

    open_guarded(kind::PROJECT_SWITCHER, window, cx, {
        let view = view.clone();
        move |dialog, window, cx| {
            // 原版是 `max-h-[60vh]`(内容少就矮一点)。`Dialog` 只吃固定高度,
            // 于是按候选条数估一个自然高度再夹到 60vh —— 常数是本文件下面那几个
            // 内边距/字号加出来的,估短了最后一行进滚动区,估长了下面留白,
            // 两种都不影响可用性。
            let count = view.read(cx).rows(cx).len();
            let natural = if count == 0 {
                HEADER_H + FOOTER_H + EMPTY_H
            } else {
                HEADER_H + FOOTER_H + LIST_PAD + count as f32 * ROW_H
            };
            let height = px(natural).min(window.viewport_size().height * 0.6);
            dialog
                // 头/尾都是自己画的分隔线,交给 Dialog 加边距会把线切断
                .p_0()
                // `close_button` 画的是 `IconName::Close`,而 0.5.1 不带 svg 资产
                // (渲染成空白,编译期无感);原版这个浮层也没有 ✕
                .close_button(false)
                // 原版 `panelClassName="w-[460px] max-h-[60vh]"` + `align="top"`
                // (`items-start pt-[10vh]`,正好是 Dialog 的默认 margin_top)
                .w(px(460.0))
                .child(div().h(height).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己的面板上,所以聚焦输入框必须**排到它后面**
    // (判据全文见 `prompt::autofocus`)
    autofocus(&input, window, cx);
}

impl ProjectSwitcher {
    fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("projectSwitcher", "placeholder"))
        });
        // `Enter` 不走 action:单行输入框的 `Enter` 处理器无条件
        // `cx.emit(InputEvent::PressEnter)`,订阅它比抢绑定直白。
        let sub = cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // 关键词一变就把游标拉回第一项(原版 onChange 里那句 setCursor(0))
                InputEvent::Change => {
                    this.cursor = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.commit(window, cx),
                _ => {}
            },
        );
        Self {
            store,
            query,
            cursor: 0,
            _subs: vec![sub],
        }
    }

    /// 当前候选集。每帧现算 —— 项目状态(AI 灯/完成标)随时在变,缓存反而要
    /// 额外的失效通道。项目数是几十条量级,重算成本可以忽略。
    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let config = store.config();
        let query = self.query.read(cx).value().to_string();

        let all: Vec<Row> = projects_with_group_path(config)
            .into_iter()
            .filter_map(|(id, group_path)| {
                let project = config.projects.iter().find(|p| p.id == id)?;
                let state = store.project_state(&id);
                Some(Row {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    path: project.path.clone(),
                    group_path,
                    status: state.map(|s| s.status).unwrap_or(PaneStatus::Idle),
                    needs_attention: state.is_some_and(|s| s.needs_attention),
                    hits: Vec::new(),
                })
            })
            .collect();

        filter_rows(all, &query)
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = self.rows(cx).len();
        self.cursor = next_cursor(self.cursor, len, delta);
        cx.notify();
    }

    /// 切到高亮的那个项目并关掉浮层。
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let Some(id) = rows.get(cursor).map(|r| r.id.clone()) else {
            return;
        };
        self.store
            .update(cx, |store, cx| store.set_active_project(&id, cx));
        // 关闭排到本轮 effect 之后:这会儿本实体正被 Dialog 的 builder 持有着,
        // 当场 `close_dialog` 等于在自己脚下抽地毯
        window.defer(cx, |window, cx| {
            close_guarded(kind::PROJECT_SWITCHER, window, cx);
        });
    }
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 子序列模糊匹配(`mt` 命中 `mini-term`),返回命中位置用于高亮。
///
/// 逐条照抄原版 `fuzzyMatch`:大小写不敏感、按顺序贪心取**最早**的那个位置、
/// 空查询返回「全中但不高亮」(空 `hits`)。
///
/// 下标是 **char** 口径(原版是 JS 的 UTF-16 码元口径),中文项目名下才不会
/// 把高亮切在半个字上。
pub fn fuzzy_match(text: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let mut hits = Vec::new();
    let mut from = 0usize;
    for ch in query.to_lowercase().chars() {
        let found = haystack[from.min(haystack.len())..]
            .iter()
            .position(|c| *c == ch)?
            + from.min(haystack.len());
        hits.push(found);
        from = found + 1;
    }
    Some(hits)
}

/// 按查询过滤 + 标注命中位置。空查询 = 全都要、都不高亮、保持侧栏顺序。
fn filter_rows(all: Vec<Row>, query: &str) -> Vec<Row> {
    let query = query.trim();
    if query.is_empty() {
        return all;
    }
    all.into_iter()
        .filter_map(|mut row| {
            if let Some(hits) = fuzzy_match(&row.name, query) {
                row.hits = hits;
                return Some(row);
            }
            // 名字没中就试分组路径,命中时不高亮名字(与原版一致)
            let path = row.group_path.join("/");
            fuzzy_match(&path, query).map(|_| row)
        })
        .collect()
}

/// 游标环形移动。列表为空时钉在 0。
pub fn next_cursor(cursor: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as i64;
    let next = (cursor as i64 + delta as i64).rem_euclid(len_i);
    next as usize
}

/// 展平 `projectTree`,给每个项目算出它的分组路径。对应
/// `src/utils/projectTree.ts::getProjectsWithGroupPath`。
///
/// 不在树里的项目(异常配置 / 还没写过 projectTree 的旧配置)追加到末尾、分组路径为空,
/// 与原版 `getOrderedTree` 的兜底口径一致。
pub fn projects_with_group_path(config: &AppConfig) -> Vec<(String, Vec<String>)> {
    fn walk(
        items: &[ProjectTreeItem],
        group_path: &[String],
        out: &mut Vec<(String, Vec<String>)>,
        seen: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                ProjectTreeItem::Group(group) => {
                    let mut deeper = group_path.to_vec();
                    deeper.push(group.name.clone());
                    walk(&group.children, &deeper, out, seen);
                }
                ProjectTreeItem::ProjectId(id) => {
                    if seen.iter().any(|s| s == id) {
                        continue;
                    }
                    seen.push(id.clone());
                    out.push((id.clone(), group_path.to_vec()));
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut seen = Vec::new();
    if let Some(tree) = config.project_tree.as_ref() {
        walk(tree, &[], &mut out, &mut seen);
    }
    // 树里没有的(以及树里有、projects 里没有的那些 id 会在上层 find 时被丢掉)
    for project in &config.projects {
        if !seen.iter().any(|s| *s == project.id) {
            out.push((project.id.clone(), Vec::new()));
        }
    }
    out
}

/// 第二行那句「分组 / 分组 · 路径」。分组为空时不带前缀,与原版三元一致。
pub fn subtitle(group_path: &[String], path: &str) -> String {
    if group_path.is_empty() {
        path.to_string()
    } else {
        format!("{} · {path}", group_path.join(" / "))
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 底部提示条里那颗键帽(`styles.css` 的 `.kbd`)。
fn kbd(label: &'static str) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(ui::bg_elevated())
        .border_1()
        .border_color(ui::border_default())
        .text_size(ui::font_px(10.0))
        .text_color(ui::text_secondary())
        .child(label)
}

fn hint(keys: &[&'static str], label: &'static str) -> impl IntoElement {
    let mut row = div().flex().items_center().gap(px(2.0));
    for key in keys {
        row = row.child(kbd(key));
    }
    row.child(
        div()
            .ml(px(4.0))
            .text_size(ui::font_px(10.0))
            .text_color(ui::text_muted())
            .child(label),
    )
}

impl Render for ProjectSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let active_id = self.store.read(cx).active_project_id.clone();
        // 候选变少之后游标可能越界(原版是一个 useEffect 夹一次)
        let cursor = self.cursor.min(rows.len().saturating_sub(1));

        let mut list = div()
            .id("project-switcher-list")
            .flex_1()
            .overflow_y_scroll()
            .py(px(4.0));

        if rows.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(24.0))
                    .text_center()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("projectSwitcher", "noMatch")),
            );
        }

        for (idx, row) in rows.iter().enumerate() {
            let is_cursor = idx == cursor;
            let is_active = Some(&row.id) == active_id.as_ref();

            // 名字 + 命中高亮。逐段发元素(与原版 `Highlight` 逐字发 span 同法,
            // 只是把相邻段合并了,见 `ui::highlight_runs`)
            let ranges: Vec<(usize, usize)> = row.hits.iter().map(|i| (*i, *i + 1)).collect();
            let mut name_line = div().flex().items_center().overflow_hidden();
            for (text, hit) in ui::highlight_runs(&row.name, &ranges) {
                name_line = name_line.child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .when(hit, |el| {
                            el.text_color(ui::accent()).font_weight(FontWeight::SEMIBOLD)
                        })
                        .when(!hit && is_cursor, |el| el.text_color(ui::accent()))
                        .when(!hit && !is_cursor, |el| el.text_color(ui::text_primary()))
                        .child(SharedString::from(text)),
                );
            }
            if is_active {
                name_line = name_line.child(
                    div()
                        .ml(px(6.0))
                        .flex_none()
                        .text_size(ui::font_px(10.0))
                        .text_color(ui::text_muted())
                        .child(t("projectSwitcher", "current")),
                );
            }

            list = list.child(
                div()
                    .id(SharedString::from(format!("switcher-{}", row.id)))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .mx(px(4.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .when(is_cursor, |el| el.bg(ui::accent_subtle()))
                    .when(!is_cursor, |el| el.hover(|el| el.bg(ui::border_subtle())))
                    // 原版是 onMouseEnter 把游标挪过来
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        if *hovered && this.cursor != idx {
                            this.cursor = idx;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.cursor = idx;
                        this.commit(window, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(name_line)
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(10.0))
                                    .text_color(ui::text_muted())
                                    .child(subtitle(&row.group_path, &row.path)),
                            ),
                    )
                    .when(row.needs_attention, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(ui::font_px(10.0))
                                .text_color(ui::color_success())
                                .child(t("panels", "done")),
                        )
                    })
                    .when(row.status != PaneStatus::Idle, |el| {
                        el.child(div().flex_none().child(ui::status_dot(row.status)))
                    }),
            );
        }

        div()
            // 方向键的上下文锚点。键位表在 `main.rs`,谓词是
            // `"ProjectSwitcher > Input"` —— 为什么非这么写见模块注释。
            .key_context("ProjectSwitcher")
            .on_action(cx.listener(|this, _: &SwitcherPrev, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitcherNext, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(Input::new(&self.query).cleanable(false)),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(ui::border_subtle())
                    .child(hint(&["↑", "↓"], t("projectSwitcher", "hintMove")))
                    .child(hint(&["Enter"], t("projectSwitcher", "hintOpen")))
                    .child(hint(&["Esc"], t("projectSwitcher", "hintClose"))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, KeyContext, Keymap, Keystroke};

    /// 子序列匹配:命中位置是**最早**的那一串,大小写不敏感。
    #[test]
    fn 模糊匹配取最早的子序列() {
        assert_eq!(fuzzy_match("mini-term", "mt"), Some(vec![0, 5]));
        assert_eq!(fuzzy_match("mini-term", "MT"), Some(vec![0, 5]));
        assert_eq!(fuzzy_match("mini-term", "mini"), Some(vec![0, 1, 2, 3]));
        assert_eq!(fuzzy_match("mini-term", "xyz"), None);
        // 顺序不对不算命中
        assert_eq!(fuzzy_match("abc", "cb"), None);
        // 空查询 = 全中且不高亮
        assert_eq!(fuzzy_match("abc", ""), Some(Vec::new()));
    }

    /// 下标按 char 计:中文名下高亮不会切在半个字上。
    #[test]
    fn 模糊匹配下标按字符计() {
        assert_eq!(fuzzy_match("前端项目", "项目"), Some(vec![2, 3]));
    }

    /// 名字没中就试分组路径,且此时**不高亮名字**。
    #[test]
    fn 分组路径也参与匹配() {
        let make = |name: &str, group: &[&str]| Row {
            id: name.to_string(),
            name: name.to_string(),
            path: "/tmp".into(),
            group_path: group.iter().map(|s| s.to_string()).collect(),
            status: PaneStatus::Idle,
            needs_attention: false,
            hits: Vec::new(),
        };
        let all = vec![make("webapp", &["前端"]), make("server", &["后端"])];
        let hit = filter_rows(all, "前端");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "webapp");
        assert!(hit[0].hits.is_empty(), "分组命中时不高亮名字");

        let all = vec![make("webapp", &["前端"])];
        let hit = filter_rows(all, "web");
        assert_eq!(hit[0].hits, vec![0, 1, 2]);

        // 空查询 = 全都要、保持顺序
        let all = vec![make("a", &[]), make("b", &[])];
        assert_eq!(filter_rows(all, "  ").len(), 2);
    }

    /// 游标环形移动,空列表钉在 0。
    #[test]
    fn 游标环形移动() {
        assert_eq!(next_cursor(0, 3, 1), 1);
        assert_eq!(next_cursor(2, 3, 1), 0, "到底折回第一项");
        assert_eq!(next_cursor(0, 3, -1), 2, "到顶折到最后一项");
        assert_eq!(next_cursor(0, 0, 1), 0);
        assert_eq!(next_cursor(5, 3, 1), 0, "越界的游标也要落回合法区间");
    }

    /// 分组路径展平:嵌套组拼成路径,树外项目追加到末尾且路径为空。
    #[test]
    fn 分组路径展平() {
        use mt_config::{ProjectGroup, ProjectTreeItem};
        let mut config = AppConfig::default();
        config.project_tree = Some(vec![
            ProjectTreeItem::ProjectId("top".into()),
            ProjectTreeItem::Group(ProjectGroup {
                id: "g1".into(),
                name: "前端".into(),
                collapsed: false,
                children: vec![
                    ProjectTreeItem::ProjectId("web".into()),
                    ProjectTreeItem::Group(ProjectGroup {
                        id: "g2".into(),
                        name: "实验".into(),
                        collapsed: false,
                        children: vec![ProjectTreeItem::ProjectId("lab".into())],
                    }),
                ],
            }),
            // 重复 id:只认第一次出现的位置
            ProjectTreeItem::ProjectId("web".into()),
        ]);
        config.projects = vec![project("top"), project("web"), project("lab"), project("orphan")];

        let flat = projects_with_group_path(&config);
        assert_eq!(
            flat,
            vec![
                ("top".to_string(), vec![]),
                ("web".to_string(), vec!["前端".to_string()]),
                ("lab".to_string(), vec!["前端".to_string(), "实验".to_string()]),
                ("orphan".to_string(), vec![]),
            ]
        );
    }

    /// 没有 projectTree 的旧配置:全部按 projects 的顺序、分组路径为空。
    #[test]
    fn 无分组树时按项目顺序() {
        let mut config = AppConfig::default();
        config.projects = vec![project("a"), project("b")];
        assert_eq!(
            projects_with_group_path(&config),
            vec![("a".to_string(), vec![]), ("b".to_string(), vec![])]
        );
    }

    #[test]
    fn 第二行文案() {
        assert_eq!(subtitle(&[], "D:/x"), "D:/x");
        assert_eq!(
            subtitle(&["前端".into(), "实验".into()], "D:/x"),
            "前端 / 实验 · D:/x"
        );
    }

    /// **机制回归测试**:`"ProjectSwitcher > Input"` 的绑定必须压过 `Input` 自带的
    /// `up`/`down`,否则焦点在输入框里时方向键只会移动光标,列表一动不动。
    ///
    /// 判据有两条,缺一不可:① 谓词深度打平(`Descendant` 取最深那层);
    /// ② 打平后按注册顺序**倒序**决胜负 —— 壳的 `cx.bind_keys` 跑在
    /// `gpui_component::init` 之后,所以壳的绑定后注册、先派发。
    #[test]
    fn 方向键绑定压过输入框自带的() {
        gpui::actions!(test_only, [InputMoveUp, SwitcherPrev]);

        let keymap = Keymap::new(vec![
            // 组件库先注册(gpui_component::init)
            KeyBinding::new("up", InputMoveUp, Some("Input")),
            // 壳后注册(main.rs 的 cx.bind_keys)
            KeyBinding::new("up", SwitcherPrev, Some("ProjectSwitcher > Input")),
        ]);
        let stack = context_stack(&["Workspace", "Dialog", "ProjectSwitcher", "Input"]);
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("up").unwrap()], &stack);

        assert!(!bindings.is_empty(), "up 至少要匹配到一条绑定");
        assert!(
            bindings[0].action().partial_eq(&SwitcherPrev),
            "第一个派发的必须是切换器的动作,实际是 {:?}",
            bindings[0].action()
        );

        // 反证:不在切换器里(普通输入框)时,方向键仍归输入框
        let stack = context_stack(&["Workspace", "Dialog", "Input"]);
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("up").unwrap()], &stack);
        assert!(
            bindings[0].action().partial_eq(&InputMoveUp),
            "切换器之外不许抢输入框的方向键"
        );
    }

    fn context_stack(names: &[&str]) -> Vec<KeyContext> {
        names
            .iter()
            .map(|n| KeyContext::parse(n).expect("上下文名必须能解析"))
            .collect()
    }

    fn project(id: &str) -> mt_config::ProjectConfig {
        mt_config::ProjectConfig {
            id: id.to_string(),
            name: id.to_string(),
            path: format!("/tmp/{id}"),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        }
    }
}
