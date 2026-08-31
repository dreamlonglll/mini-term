//! 「查看会话分支」悬停展开的**家族面板**。对应 `src/components/BranchFamilyPanel.tsx`。
//!
//! ```text
//! pane 右键 ─→ 菜单项「查看会话分支」悬停
//!               └─ menu.rs 的自绘子菜单挂载点(MenuItem::submenu_element)
//!                    └─ BranchFamilyPanel(本模块)
//!                         ├─ background: get_ai_sessions(项目路径)
//!                         ├─ background: scan_session_lineage(+ 自记账边)
//!                         └─ 主线程:合并边 → 建森林 → **单支过滤** → 平铺出行
//! ```
//!
//! 悬停展开 / 互斥 / 定位 / 随菜单关闭全部由菜单机制接管(见 [`crate::menu`] 的
//! 「自定义元素子菜单」段),本模块只管内容:连线、标题、图标、在跑状态与节点点击。
//!
//! # 与 AI 历史面板树视图([`crate::session_panel`])的差别
//!
//! 同样是会话树,取数与呈现刻意不同:
//!
//! | | 树视图 | 家族面板 |
//! |---|---|---|
//! | 画什么 | 整片森林(项目全部会话) | **只画 pane 当前会话所在那一支** |
//! | 谁触发 | 抽屉展开 | pane 右键悬停(每次现拉) |
//! | 分页 | 有(`display_count`) | 无(单支不会长到要分页) |
//!
//! # 两处照抄原版的取舍
//!
//! - **行标题用 `LineageEdge::branch_title`**(分叉后第一问):fork 是整份复制,
//!   标题字段连同首条消息一起继承自根会话,分支之间全同名 —— 真正区分一条分支的
//!   是它岔开后干了什么。没有(分支还没提问)才回落会话标题。
//! - **行图标按最新模型推厂商**(`AiVendor::for_session`):claude CLI 挂 GLM /
//!   DeepSeek 中转是常见用法,CLI ≠ 模型厂商。pane tab 的图标刻意**不用**这个
//!   口径(它表达「跑的是哪个 CLI」,与状态灯 / hook 语义绑定)。

use std::cell::RefCell;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use mt_ui::tooltip::Tooltip;
use mt_ai::sessions::{AiSession, LineageEdge};
use mt_ui::icons::{AiVendor, BrandIcon};

use crate::i18n::t;
use crate::menu;
use crate::pane_actions;
use crate::session_branch::{
    BranchMenuSegment, build_session_tree, find_family_root, flatten_session_tree,
    merge_lineage_edges,
};
use crate::session_panel::jump_to_session;
use crate::store::AppStore;
use crate::ui;

/// 卡片宽度。与 `BranchFamilyPanel.tsx` 的 `CARD_WIDTH` 同值。
const CARD_WIDTH: f32 = 340.0;
/// 列表区最大高度(原版 `max-h-[300px]`)。
const MAX_LIST_HEIGHT: f32 = 300.0;

/// 面板里的一行(已经拍平、带好连线前缀与显示标题)。
///
/// 没有 `PartialEq`:`AiSession` 是 mt-ai 的类型、不带这个 derive,
/// 而它的公开 API 只增不改。测试比对逐字段来(反正只关心 id / 前缀 / 标题)。
#[derive(Debug, Clone)]
pub struct FamilyRow {
    pub session: AiSession,
    /// 行首连线前缀(`│ ├ └`),根为空串。**等宽字体**下才对得齐。
    pub prefix: String,
    /// 显示标题:分叉后第一问 → 回落会话标题。
    pub title: String,
}

/// 会话列表 + 分支边 + 目标会话 id → 该会话所在**单支家族**的连线行。
///
/// 纯函数(不碰磁盘、不碰 gpui),对应 `sessionJump.ts::fetchFamilyRows` 的后半段。
/// 目标会话不在列表里(记录被清理 / 超出扫描窗口)时返回空表 —— 面板显示
/// 「该会话没有分支记录」,与原版 `findFamilyRoot` 返回 null 同。
pub fn build_family_rows(
    sessions: &[AiSession],
    edges: &[LineageEdge],
    session_id: &str,
) -> Vec<FamilyRow> {
    let Some(target) = sessions.iter().position(|s| s.id == session_id) else {
        return Vec::new();
    };
    let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
    let timestamps: Vec<String> = sessions.iter().map(|s| s.timestamp.clone()).collect();
    let roots = build_session_tree(&ids, &timestamps, edges);
    let Some(family) = find_family_root(&roots, target) else {
        return Vec::new();
    };
    flatten_session_tree(std::slice::from_ref(family))
        .into_iter()
        .filter_map(|row| {
            let session = sessions.get(row.index)?;
            let title = row
                .edge
                .and_then(|i| edges.get(i))
                .and_then(|e| e.branch_title.clone())
                .unwrap_or_else(|| session.title.clone());
            Some(FamilyRow {
                session: session.clone(),
                prefix: row.prefix,
                title,
            })
        })
        .collect()
}

// ─── 菜单项(tab 右键与终端右键共用) ───────────────────────────

/// 「分支会话到新分屏」。
///
/// 新 pane 是新进程,原会话里「本会话允许」的权限授权不迁移(CLI 官方行为)。
pub fn fork_menu_item(
    store: &Entity<AppStore>,
    project_id: String,
    pane_id: String,
) -> menu::MenuEntry {
    let store = store.clone();
    menu::item(t("paneGroup", "forkSession"), move |window, cx| {
        pane_actions::fork_pane_session(
            store.clone(),
            project_id.clone(),
            pane_id.clone(),
            window,
            cx,
        );
    })
}

/// 「查看会话分支」—— 悬停展开家族面板。
///
/// 面板实体**懒建一次**并缓存在闭包里:`submenu_element` 在子菜单展开期间每次
/// 菜单重绘都会调一遍,每次新建一个实体 = 每次重扫一遍磁盘,而且因为实体每帧
/// 都换新的,永远停在「加载中」。缓存的生命周期 = 这一次菜单开着的期间
/// (菜单收起 → entries drop → 闭包 drop → 实体释放,等价于原版的 `root.unmount()`)。
pub fn view_branches_menu_item(
    store: &Entity<AppStore>,
    project_path: String,
    session_id: String,
) -> menu::MenuEntry {
    let store = store.clone();
    let cached: RefCell<Option<Entity<BranchFamilyPanel>>> = RefCell::new(None);
    menu::MenuItem::new(t("paneGroup", "viewSessionBranches"))
        .submenu_element(move |_window, cx| {
            let mut slot = cached.borrow_mut();
            let panel = slot.get_or_insert_with(|| {
                let store = store.clone();
                let path = project_path.clone();
                let id = session_id.clone();
                cx.new(|cx| BranchFamilyPanel::new(store, path, id, cx))
            });
            panel.clone().into_any_element()
        })
        .into()
}

/// 「分支会话(未获会话身份,需注册 Hook 事件)」置灰提示。
pub fn needs_identity_menu_item() -> menu::MenuEntry {
    menu::MenuItem::new(t("paneGroup", "forkNeedsIdentity"))
        .disabled(true)
        .into()
}

/// 分支那一段的完整菜单项(**含前导分隔线**),按 `segment` 的形态给。
///
/// 终端本体右键用这个整段;tab 右键因为要走它自己的项序表,逐项单独取。
/// 两处出的项与顺序必须一致 —— 用户在哪儿右键都该找得到同一个入口。
pub fn branch_menu_entries(
    store: &Entity<AppStore>,
    project_id: &str,
    pane_id: &str,
    project_path: String,
    segment: &BranchMenuSegment,
) -> Vec<menu::MenuEntry> {
    match segment {
        BranchMenuSegment::Fork { session_id, .. } => vec![
            menu::separator(),
            fork_menu_item(store, project_id.to_string(), pane_id.to_string()),
            view_branches_menu_item(store, project_path, session_id.clone()),
        ],
        BranchMenuSegment::NeedsIdentity => {
            vec![menu::separator(), needs_identity_menu_item()]
        }
        BranchMenuSegment::None => Vec::new(),
    }
}

pub struct BranchFamilyPanel {
    store: Entity<AppStore>,
    /// 当前 pane 的会话 id:高亮「← 当前」,点击禁用。
    session_id: String,
    /// `None` = 还在拉;`Some(空)` = 这个会话没有分支记录。
    rows: Option<Vec<FamilyRow>>,
    /// 取数任务。面板随菜单收起而 drop,还没回来的扫描跟着取消(正是想要的:
    /// 菜单都关了,结果没人看)。**节点点击的跳转任务不放这儿** —— 那条要活过
    /// 面板自己的死亡,见 `render_row` 里的 `detach`。
    _tasks: Vec<Task<()>>,
}

impl BranchFamilyPanel {
    /// 建面板并**当场开始拉数据**(原版是 `useEffect` 在首帧后拉)。
    ///
    /// `project_path` 只在取数时用一次,不存字段 —— 面板活不过一次菜单开合。
    /// 不收 `project_id`:节点点击走 [`jump_to_session`],它取的是**活动项目**
    /// (能右键到的 pane 必然在活动项目里,与树视图同一条口径)。
    pub fn new(
        store: Entity<AppStore>,
        project_path: String,
        session_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        // 自记账边:mini-term 自己发起的 fork 当场记下的 child→parent。
        // **必须传给 mt-ai** —— Claude 的 CLI fork 不写磁盘指针,这些边的
        // 「分叉后第一问」标题只能由它拿父子文件比对补出。
        let saved = store.read(cx).config().session_lineage.clone();
        let bookkept: Vec<mt_ai::sessions::BookkeptLineageEdge> = saved
            .iter()
            .map(|e| mt_ai::sessions::BookkeptLineageEdge {
                agent: e.agent.clone(),
                session_id: e.session_id.clone(),
                parent_session_id: e.parent_session_id.clone(),
                fork_point_uuid: e.fork_point_uuid.clone(),
            })
            .collect();
        // 同一批边留一份 `LineageEdge` 形态,给「扫描整个失败但自记账还在」的窗口
        // 兜底合并(与 `session_panel` 合两次同一条理由)
        let fallback: Vec<LineageEdge> = saved
            .iter()
            .map(|e| LineageEdge {
                agent: e.agent.clone(),
                session_id: e.session_id.clone(),
                parent_session_id: e.parent_session_id.clone(),
                fork_point_uuid: e.fork_point_uuid.clone(),
                branch_title: None,
            })
            .collect();

        let target = session_id.clone();
        // 两个函数都是**同步磁盘遍历**,落在 GPUI 主线程上就是整个窗口卡住
        // (与 `session_panel` 的三个慢函数同一条红线)
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let sessions = mt_ai::sessions::get_ai_sessions(project_path.clone());
                    let disk =
                        mt_ai::sessions::scan_session_lineage(project_path, Some(bookkept));
                    (sessions, disk)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                // 会话列表取不到 = 按「没有分支记录」处理(原版 catch → setRows([]));
                // 分支边扫描内部逐文件容错,永远给得出一个 Vec
                let (sessions, disk) = result;
                let sessions = sessions.unwrap_or_default();
                let edges = merge_lineage_edges(disk, fallback);
                this.rows = Some(build_family_rows(&sessions, &edges, &target));
                cx.notify();
            });
        });

        Self {
            store,
            session_id,
            rows: None,
            _tasks: vec![task],
        }
    }

    /// 一条居中的灰字提示(加载中 / 空)。
    fn hint(text: impl Into<SharedString>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(12.0))
            .flex()
            .justify_center()
            .text_color(ui::text_muted())
            .child(text.into())
    }
}

impl Render for BranchFamilyPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("branch-family-list")
            .max_h(px(MAX_LIST_HEIGHT))
            .overflow_y_scroll()
            .p(px(4.0))
            .flex()
            .flex_col();

        match self.rows.as_ref() {
            None => list = list.child(Self::hint(t("sessionList", "loading"))),
            Some(rows) if rows.is_empty() => {
                list = list.child(Self::hint(t("paneGroup", "branchPopover.empty")));
            }
            Some(rows) => {
                for row in rows.clone() {
                    list = list.child(self.render_row(row, cx));
                }
            }
        }

        div()
            .w(px(CARD_WIDTH))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_strong())
            .bg(ui::bg_overlay())
            .shadow_lg()
            .text_size(ui::font_px(12.0))
            // 面板挂在菜单项里,菜单面板已经 occlude 了;这里再挡一道,
            // 免得滚动条上的按下穿到底下去
            .occlude()
            .child(list)
    }
}

impl BranchFamilyPanel {
    fn render_row(&self, row: FamilyRow, cx: &mut Context<Self>) -> impl IntoElement {
        let session = row.session;
        let is_current = session.id == self.session_id;
        // 在跑徽章:三条件齐备才算(见 `AppStore::find_live_session_pane`)
        let live = self.store.read(cx).find_live_session_pane(&session.id);
        let vendor = AiVendor::for_session(&session.session_type, session.model.as_deref());
        let tip: SharedString = row.title.clone().into();
        let clicked = session.clone();

        div()
            .id(SharedString::from(format!("branch-family-{}", session.id)))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(6.0))
            .py(px(4.0))
            .rounded(px(4.0))
            // 当前那一行不可点(原版 `cursor-default` + 常亮底色)
            .when(is_current, |el| el.bg(ui::border_subtle()))
            .when(!is_current, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(ui::border_subtle()))
                    .on_click(cx.listener(move |this: &mut Self, _, window, cx| {
                        // 原版靠事件冒泡到 document 的关闭监听把菜单收掉;GPUI 侧
                        // 本面板嵌在菜单项里、被菜单面板的 `occlude` 挡着,点击够
                        // 不到全窗遮罩 —— 必须显式收菜单。
                        //
                        // 顺序是**先收菜单、再跳**(与 menu.rs「先还焦点再跑动作」
                        // 同一条理由):菜单关闭会把焦点还给打开它之前那个元素,
                        // 反过来的话刚激活/新建的终端当场被抢走光标。
                        //
                        // 跳转必须 defer 出去:收菜单会连带 drop 本面板(实体被
                        // 菜单项的闭包持有),在自己的 listener 里同步跑后续动作
                        // 等于站在正在塌的楼上。返回的 Task 同理只能 `detach`,
                        // 挂回 `self._tasks` 会随面板一起被取消。
                        let store = this.store.clone();
                        let session = clicked.clone();
                        window.defer(cx, move |window, cx| {
                            jump_to_session(&store, session, window, cx).detach();
                        });
                        crate::menu::close(window, cx);
                    }))
            })
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            // 连线前缀:**等宽字体 + 不换行 + 不截断**,`│├└` 才对得齐
            .when(!row.prefix.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family("monospace")
                        .whitespace_nowrap()
                        .text_color(ui::text_muted())
                        .child(row.prefix.clone()),
                )
            })
            .when_some(live, |el, (_, _, status)| {
                el.child(ui::status_dot(status))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(12.0))
                    .h(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        BrandIcon::new(vendor)
                            .size(px(12.0))
                            // VectorIcon 自己画,不继承 text_color
                            .color(ui::text_secondary()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(ui::text_secondary())
                    .child(row.title),
            )
            .when(is_current, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(ui::accent())
                        .child(t("paneGroup", "branchPopover.current")),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: &str, ts: &str) -> AiSession {
        AiSession {
            id: id.to_string(),
            session_type: "claude".to_string(),
            title: title.to_string(),
            timestamp: ts.to_string(),
            model: None,
            wsl_distro: None,
            ssh_connection_id: None,
        }
    }

    fn edge(child: &str, parent: &str, branch_title: Option<&str>) -> LineageEdge {
        LineageEdge {
            agent: "claude".to_string(),
            session_id: child.to_string(),
            parent_session_id: parent.to_string(),
            fork_point_uuid: None,
            branch_title: branch_title.map(str::to_string),
        }
    }

    /// 逐字段摘要(`FamilyRow` 不带 `PartialEq`,见类型注释)。
    fn shape(rows: &[FamilyRow]) -> Vec<(&str, &str, &str)> {
        rows.iter()
            .map(|r| {
                (
                    r.session.id.as_str(),
                    r.prefix.as_str(),
                    r.title.as_str(),
                )
            })
            .collect()
    }

    /// 单支过滤:只画目标会话那一支,别的家族整棵不进结果。
    #[test]
    fn 家族面板只画单支() {
        let sessions = vec![
            session("r1", "根一", "1"),
            session("c1", "复制来的标题", "2"),
            session("r2", "根二", "3"),
            session("c2", "另一支", "4"),
        ];
        let edges = vec![
            edge("c1", "r1", Some("改走流式")),
            edge("c2", "r2", None),
        ];

        let rows = build_family_rows(&sessions, &edges, "c1");
        assert_eq!(
            rows.iter().map(|r| r.session.id.as_str()).collect::<Vec<_>>(),
            vec!["r1", "c1"],
            "r2 那一支一行都不许出现"
        );
        assert_eq!(rows[0].prefix, "");
        assert_eq!(rows[1].prefix, "└─ ");

        // 从根出发也拿到同一支
        let from_root = build_family_rows(&sessions, &edges, "r1");
        assert_eq!(
            shape(&from_root),
            shape(&rows),
            "从根还是从叶进来,画出来是同一支"
        );
    }

    /// 分支行标题优先用「分叉后第一问」——fork 整份复制会让标题继承根会话,
    /// 分支之间全同名。
    #[test]
    fn 分支行标题优先取分叉后第一问() {
        let sessions = vec![
            session("r1", "根一", "1"),
            session("c1", "根一", "2"),
            session("c2", "根一", "3"),
        ];
        let edges = vec![
            edge("c1", "r1", Some("改走流式")),
            // 分支还没提问 → 回落会话标题
            edge("c2", "r1", None),
        ];
        let rows = build_family_rows(&sessions, &edges, "r1");
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["根一", "改走流式", "根一"]);
        // 根自己永远用会话标题(它没有入边)
        assert!(edges.iter().all(|e| e.session_id != "r1"));
    }

    /// 会话没有任何分支 → 一行(自己),不是空表。
    #[test]
    fn 无分支的会话画一行() {
        let sessions = vec![session("solo", "一个人", "1")];
        let rows = build_family_rows(&sessions, &[], "solo");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefix, "");
        assert_eq!(rows[0].title, "一个人");
    }

    /// 目标会话不在列表里(记录被清理 / 超出扫描窗口)→ 空表,
    /// 面板显示「该会话没有分支记录」而不是画错一支。
    #[test]
    fn 目标不在列表时返回空表() {
        let sessions = vec![session("a", "甲", "1"), session("b", "乙", "2")];
        assert!(build_family_rows(&sessions, &[edge("b", "a", None)], "没见过").is_empty());
        assert!(build_family_rows(&[], &[], "任意").is_empty());
    }

    /// 深一层的连线前缀照旧由 `session_branch` 算(这里钉住它没被单支过滤改坏)。
    #[test]
    fn 单支过滤后连线前缀仍然正确() {
        let sessions = vec![
            session("r", "根", "1"),
            session("a", "甲", "2"),
            session("b", "乙", "3"),
            session("a1", "甲一", "4"),
        ];
        let edges = vec![
            edge("a", "r", None),
            edge("b", "r", None),
            edge("a1", "a", None),
        ];
        let rows = build_family_rows(&sessions, &edges, "a1");
        assert_eq!(
            rows.iter().map(|r| r.prefix.as_str()).collect::<Vec<_>>(),
            vec!["", "├─ ", "│  └─ ", "└─ "],
        );
        assert_eq!(
            rows.iter().map(|r| r.session.id.as_str()).collect::<Vec<_>>(),
            vec!["r", "a", "a1", "b"],
        );
    }
}
