//! 终端区右缘的「项目级终端面板」竖条。
//!
//! 一个项目下可以开多个**面板**([`crate::tree::ProjectPanel`]):每个面板自带
//! 一整棵分屏树,互不影响;终端区只渲染活动面板,其余面板的 PTY 在后台照跑。
//! 本竖条就是面板的切换器 —— 与 ActivityBar 同宽(44px)、只放图标,贴在终端区
//! 右缘(VS Code 终端面板右侧列表的位置)。
//!
//! 每颗按钮:面板里有 AI 会话就用它的品牌图标(第一有身份的 pane),否则用
//! 终端字形;右上角挂 AI 进度呼吸灯,右下角挂终端数角标;激活态与 ActivityBar
//! 按钮同款(底色 + 左缘 accent 竖条);完整名看 tooltip(自定义名 > 面板 N)。
//!
//! # 呼吸灯的口径([`panel_light_status`])
//!
//! 与项目级那颗灯(`AppStore::global_ai_status`)同一套语义,只是取样范围不同:
//! 只看该面板里「切过去就能看见」的 pane(各叶子的激活 tab,
//! [`SplitNode::visible_panes`](crate::tree::SplitNode::visible_panes)),
//! 后台 tab 不亮灯;`error` 压成 `idle` —— 一个 `exit 1` 的 shell 不该在竖条上
//! 亮红点、盖住真在跑的 AI。`ai-working` 档闪烁(与项目级灯同一颗
//! [`activity_bar::status_badge`]),idle 不挂灯。
//!
//! 交互对齐 tab 栏的手感:单击切换、双击改名、右键菜单(重命名/关闭,关闭走
//! [`crate::pane_actions::close_panel`] 的 AI 感知确认)。头部「+」新建面板,
//! 多 shell 时弹选择菜单(与 tab 栏「+」同一条 `<= 1` 的闸)。
//!
//! 显隐由 `AppStore::terminals_panel_visible` 持有(落 layout.db),开关在
//! ActivityBar;收起时整个元素不在树上,零开销 —— 数据全部现场从 store 读,
//! 不需要 SessionPanel 那套 visible/stale 闸。

use gpui::{
    ClickEvent, Context, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use mt_ui::icons::{AiVendor, BrandIcon, Geom, Ink, Shape, VectorIcon};
use mt_ui::tooltip::Tooltip;

use crate::activity_bar;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::pane_actions;
use crate::prompt;
use crate::store::AppStore;
use crate::terminal_area::{click_count, click_position};
use crate::tree::{PaneStatus, PaneState};
use crate::ui;

/// 竖条宽度 = ActivityBar 宽度(用户要求同宽、只放图标)。
pub const WIDTH: f32 = activity_bar::WIDTH;
/// 按钮尺寸/图标尺寸,与 ActivityBar 同款。
const BUTTON: f32 = 32.0;
const ICON: f32 = 18.0;

/// 单位方框换算(原版 viewBox `0 0 16 16` 的口径,与 activity_bar 一致)。
const fn u(v: f32) -> f32 {
    v / 16.0
}
const STROKE: f32 = 1.2 / 16.0;

/// 无 AI 会话的面板图标:终端窗口(圆角框 + 提示符 `>`)。
/// 与 [`activity_bar::SSH`] 相近但**不带光标下划线** —— 两个字形分别指
/// 「SSH 连接管理」与「一个终端面板」,并排出现时得有差异。
const PANEL_ICON: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(2.0),
            y: u(3.0),
            w: u(12.0),
            h: u(10.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(5.2), u(6.0)), (u(7.4), u(8.0)), (u(5.2), u(10.0))]),
    ),
];

/// 呼吸灯档位:可见 pane 里的最高 AI 档,`error` 压成 `idle`(口径见模块注释)。
/// **纯判定**,单测钉在这上面。
fn panel_light_status<'a>(visible: impl IntoIterator<Item = &'a PaneState>) -> PaneStatus {
    visible
        .into_iter()
        .map(|p| match p.status {
            PaneStatus::Error => PaneStatus::Idle,
            other => other,
        })
        .fold(PaneStatus::Idle, |acc, s| {
            if s.priority() > acc.priority() { s } else { acc }
        })
}

/// 一颗面板按钮的展示数据(渲染前从 store 一次性收齐)。
struct PanelItem {
    panel_id: String,
    /// tooltip 与改名默认值:自定义名 > 「面板 N」。
    title: String,
    /// 呼吸灯档位([`panel_light_status`])。
    status: PaneStatus,
    /// 面板里的终端总数(含后台 tab)—— 右下角角标。
    count: usize,
    vendor: Option<AiVendor>,
    active: bool,
}

pub struct TerminalsPanel {
    store: Entity<AppStore>,
}

impl TerminalsPanel {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        // 数据全在 store 里,任何变化(切面板/开关终端/状态灯)都只需重画
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self { store }
    }

    fn render_item(
        &self,
        project_id: &str,
        item: PanelItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let PanelItem {
            panel_id,
            title,
            status,
            count,
            vendor,
            active,
        } = item;
        let pid_click = project_id.to_string();
        let pid_menu = project_id.to_string();
        let panel_click = panel_id.clone();
        let panel_menu = panel_id.clone();
        let title_tip = SharedString::from(title);
        let icon_color = if active {
            ui::text_primary()
        } else {
            ui::text_muted()
        };

        div()
            .id(SharedString::from(format!("term-panel-{panel_id}")))
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .w(px(BUTTON))
            .h(px(BUTTON))
            .flex_none()
            .rounded(px(4.0))
            .cursor_pointer()
            .when(active, |el| el.bg(ui::border_subtle()))
            .hover(|el| el.bg(ui::border_subtle()))
            .child(match vendor {
                // 面板里跑着 AI:品牌图标当脸,一眼分清哪个面板是谁
                Some(vendor) => BrandIcon::new(Some(vendor))
                    .size(px(ICON))
                    .color(icon_color)
                    .into_any_element(),
                None => VectorIcon::new(PANEL_ICON, px(ICON))
                    .ink(icon_color)
                    .into_any_element(),
            })
            // AI 进度呼吸灯(与项目级那颗同一套徽标,`ai-working` 档闪烁;
            // 档位口径见 [`panel_light_status`],idle 不挂)
            .when(status != PaneStatus::Idle, |el| {
                el.child(activity_bar::status_badge(status))
            })
            // 终端数角标(右下角):这个面板里一共几个终端(含后台 tab)
            .child(
                div()
                    .absolute()
                    .bottom(px(-2.0))
                    .right(px(-2.0))
                    .min_w(px(13.0))
                    .h(px(13.0))
                    .px(px(2.0))
                    .rounded(px(6.5))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(ui::bg_elevated())
                    .border_1()
                    .border_color(ui::border_default())
                    .text_size(ui::font_px(8.0))
                    .text_color(if active {
                        ui::text_primary()
                    } else {
                        ui::text_muted()
                    })
                    .child(SharedString::from(count.to_string())),
            )
            // 激活态左缘 accent 竖条(与 strip_button 同款)
            .when(active, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px((BUTTON - 16.0) / 2.0))
                        .w(px(2.0))
                        .h(px(16.0))
                        .rounded(px(1.0))
                        .bg(ui::accent()),
                )
            })
            .tooltip(move |window, cx| Tooltip::new(title_tip.clone()).build(window, cx))
            // 单击切换,双击改名 —— 与 tab 同手感
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if click_count(event) >= 2 {
                    open_rename_panel(
                        this.store.clone(),
                        pid_click.clone(),
                        panel_click.clone(),
                        window,
                        cx,
                    );
                    return;
                }
                this.store.update(cx, |store, cx| {
                    store.switch_panel(&pid_click, &panel_click, window, cx)
                });
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = panel_menu_entries(&this.store, &pid_menu, &panel_menu);
                    menu::show(event.position, entries, window, cx);
                }),
            )
    }

    /// 「+」新建面板:多 shell 弹选择菜单,单 shell 直建(与 tab 栏「+」同规则)。
    fn render_new_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("term-panel-new")
            .flex()
            .items_center()
            .justify_center()
            .w(px(BUTTON))
            .h(px(BUTTON))
            .flex_none()
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(16.0))
            .text_color(ui::text_muted())
            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::accent()))
            .tooltip(move |window, cx| {
                Tooltip::new(t("terminalArea", "newPanel")).build(window, cx)
            })
            .child("+")
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                let Some(pid) = this.store.read(cx).active_project_id.clone() else {
                    return;
                };
                let (shells, launchers) =
                    pane_actions::new_terminal_menu_data(this.store.read(cx), &pid);
                if !pane_actions::should_show_new_terminal_menu(shells.len(), launchers.len()) {
                    this.store.update(cx, |store, cx| {
                        store.new_panel(&pid, None, window, cx);
                    });
                    return;
                }
                let entries = pane_actions::new_terminal_menu_entries(
                    shells,
                    launchers,
                    {
                        let store = this.store.clone();
                        let pid = pid.clone();
                        move |shell, window, cx| {
                            let pid = pid.clone();
                            store.update(cx, |store, cx| {
                                store.new_panel(&pid, Some(shell), window, cx);
                            });
                        }
                    },
                    {
                        let store = this.store.clone();
                        let pid = pid.clone();
                        move |launcher, window, cx| {
                            let pid = pid.clone();
                            store.update(cx, |store, cx| {
                                store.new_panel_from_launcher(&pid, &launcher, window, cx);
                            });
                        }
                    },
                );
                menu::show(click_position(event, window), entries, window, cx);
            }))
    }
}

/// 面板右键菜单:重命名 / 关闭面板(带 AI 感知确认)。
fn panel_menu_entries(
    store: &Entity<AppStore>,
    project_id: &str,
    panel_id: &str,
) -> Vec<menu::MenuEntry> {
    let rename = {
        let (store, pid, panel) = (store.clone(), project_id.to_string(), panel_id.to_string());
        MenuItem::new(t("terminalArea", "renamePanel"))
            .on_click(move |window, cx| {
                open_rename_panel(store.clone(), pid.clone(), panel.clone(), window, cx);
            })
            .into()
    };
    let close = {
        let (store, pid, panel) = (store.clone(), project_id.to_string(), panel_id.to_string());
        MenuItem::new(t("terminalArea", "closePanel"))
            .danger()
            .on_click(move |window, cx| {
                pane_actions::close_panel(store.clone(), pid.clone(), panel.clone(), window, cx);
            })
            .into()
    };
    vec![rename, menu::separator(), close]
}

/// 改面板名(双击 / 右键「重命名」共用)。默认值取当前显示名,清空 = 恢复序号名。
fn open_rename_panel(
    store: Entity<AppStore>,
    project_id: String,
    panel_id: String,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let current = store
        .read(cx)
        .project_state(&project_id)
        .map(|s| panel_title(s, &panel_id))
        .unwrap_or_default();
    prompt::show_prompt(
        t("terminalArea", "renamePanel"),
        t("terminalArea", "renamePanel"),
        current,
        move |value, _window, cx| {
            store.update(cx, |store, cx| {
                store.rename_panel(&project_id, &panel_id, &value, cx);
            });
        },
        window,
        cx,
    );
}

/// 面板显示名:自定义名 > 「面板 N」(N 是 1-based 列表位)。
fn panel_title(state: &crate::store::ProjectState, panel_id: &str) -> String {
    let Some(idx) = state.panels.iter().position(|p| p.id == panel_id) else {
        return String::new();
    };
    state.panels[idx]
        .custom_title
        .clone()
        .unwrap_or_else(|| tr!("terminalArea", "panelN", n = idx + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(status: PaneStatus) -> PaneState {
        let mut p = PaneState::new("pwsh");
        p.status = status;
        p
    }

    /// 呼吸灯与项目级灯同口径:取最高 AI 档,`error` 压成 `idle` 不上冒。
    #[test]
    fn 呼吸灯取最高档且error不上冒() {
        assert_eq!(panel_light_status([].iter()), PaneStatus::Idle, "空面板不亮");
        assert_eq!(
            panel_light_status([pane(PaneStatus::Idle), pane(PaneStatus::AiIdle)].iter()),
            PaneStatus::AiIdle
        );
        assert_eq!(
            panel_light_status([pane(PaneStatus::AiIdle), pane(PaneStatus::AiWorking)].iter()),
            PaneStatus::AiWorking
        );
        // 一个 exit 1 的 shell 不该亮红点,也不该盖住别格真在跑的 AI
        assert_eq!(
            panel_light_status([pane(PaneStatus::Error)].iter()),
            PaneStatus::Idle
        );
        assert_eq!(
            panel_light_status([pane(PaneStatus::Error), pane(PaneStatus::AiWorking)].iter()),
            PaneStatus::AiWorking
        );
    }
}

impl Render for TerminalsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 渲染前一次性收齐 —— listener 要拿走 cx,数据得先离开 store 的借用
        let (project_id, items) = {
            let store = self.store.read(cx);
            let project_id = store.active_project_id.clone();
            let mut items: Vec<PanelItem> = Vec::new();
            if let Some(pid) = project_id.as_deref()
                && let Some(state) = store.project_state(pid)
            {
                let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
                let active_id = state.active_panel().map(|p| p.id.clone());
                for panel in &state.panels {
                    // 品牌图标口径与 tab 栏一字不差:hook 认出的 agent 优先,
                    // 退到输入检测,三家之外走词匹配
                    let vendor = panel.layout.panes().into_iter().find_map(|p| {
                        if !p.shows_ai_session(auto_resume) {
                            return None;
                        }
                        let agent = p.ai_agent()?;
                        AiVendor::from_session_type(agent)
                            .or_else(|| AiVendor::infer(Some(agent), None))
                    });
                    items.push(PanelItem {
                        title: panel_title(state, &panel.id),
                        status: panel_light_status(panel.layout.visible_panes()),
                        count: panel.layout.panes().len(),
                        vendor,
                        active: active_id.as_deref() == Some(panel.id.as_str()),
                        panel_id: panel.id.clone(),
                    });
                }
            }
            (project_id, items)
        };

        let mut strip = div()
            .w(px(WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .py(px(8.0))
            .bg(ui::bg_surface())
            .border_l_1()
            .border_color(ui::border_subtle());

        if let Some(pid) = project_id.as_deref() {
            for item in items {
                strip = strip.child(self.render_item(pid, item, cx));
            }
            strip = strip.child(self.render_new_button(cx));
        }
        strip
    }
}
