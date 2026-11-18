//! 设置 →「快捷键」页:按 [`hotkeys::groups`] 分组列出键位表。可改的行点键帽录制新键位,
//! 行尾「恢复默认」、页顶「全部恢复默认」;只读的行标「不可更改」,tooltip 里说为什么。
//!
//! # 录制为什么走 `intercept_keystrokes`
//!
//! gpui 的按键派发是「先匹配绑定、后跑 key 监听」(`Window::dispatch_key_event`)。录制框
//! 要是挂普通的 `on_key_down`,按下的组合键先被 keymap 吃掉:Esc 关掉设置弹窗(Dialog 的
//! `escape → Cancel`)、Tab 挪焦点(Root 的焦点导航)、Ctrl+C 被 Root 的 Copy 拿走……
//! 录到的只剩「没人要的键」。gpui-pre 有 `App::intercept_keystrokes`(Zed 的 keymap 编辑器
//! 用的就是它):回调在**匹配绑定之前**被调用,回调里 `stop_propagation` 之后,这次按键
//! 既不派发 action 也不进 key 监听。
//!
//! # 拦截必须确定地撤销
//!
//! 拦截器是 App 级的:订阅活着,整个应用的键盘就归它。所以:
//!
//! - 三个订阅(拦截器、录制框失焦、窗口失活)都放在 [`HotkeyRecording`] 里,录制状态一清
//!   就一起 drop;
//! - 结束条件:录完 / Esc / 点了面板别处(录制框失焦)/ 换分页 / 窗口失活 / 设置关闭
//!   (视图释放,连带状态里的订阅);
//! - 回调里再兜一道安全阀:不是这个窗口、录制框没焦点、不在这一页、设置面板已不在覆盖物
//!   栈里 —— 先结束录制并**放行**这次按键。宁可漏录一次,也不能把键盘锁死。
//!
//! 撞上另一条可改键位时,录制先结束,等用户在那一行选「替换 / 取消」:选择按钮靠鼠标点,
//! 点下去面板根节点会抢焦点(录制框失焦),所以待选的冲突不挂在录制状态上。
//!
//! # 提示一律就地
//!
//! 设置面板是弹窗,toast 落在遮罩底下看不见:不合法、被占用、冲突、占用终端常用键的提醒,
//! 全挂在那一行底下。

use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeystrokeEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    WindowId, div, prelude::FluentBuilder, px,
};
use mt_ui::tooltip::TooltipExt as _;

use crate::hotkeys::{self, HotkeyDef, Overrides, Owner, Platform, Recorded, Verdict};
use crate::i18n::{t, tr};
use crate::overlay;
use crate::ui;

use super::widgets::page_root;
use super::{SettingsPage, SettingsView};

/// 快捷键页的界面状态。
#[derive(Default)]
pub(super) struct ShortcutsState {
    /// 正在录制的那一行。
    recording: Option<HotkeyRecording>,
    /// 录到的键位撞上另一条可改的,等用户选「替换 / 取消」。
    conflict: Option<HotkeyConflict>,
    /// 最近一次改键留下的就地提醒(占用终端常用键),挂在那一行底下:`(id, 文案)`。
    notice: Option<(&'static str, String)>,
    /// 「全部恢复默认」的就地确认展开着没有。
    reset_confirm: bool,
}

/// 正在录制的那一行。整个结构一 drop,三个订阅一起撤销(见模块注释)。
struct HotkeyRecording {
    id: &'static str,
    /// 录制框自己的焦点。拦截回调只在它有焦点时干活。
    focus: FocusHandle,
    window: WindowId,
    /// 上一次按下的组合为什么不收(就地提示,留在录制态)。
    problem: Option<RecordProblem>,
    _subs: [Subscription; 3],
}

/// 录到了但不收的那一下:按的是什么(显示串)、为什么不收。
struct RecordProblem {
    keys: String,
    kind: ProblemKind,
}

enum ProblemKind {
    Invalid(hotkeys::Invalid),
    Taken(Owner),
}

/// 录到的键位撞上另一条可改的。
struct HotkeyConflict {
    id: &'static str,
    keystroke: String,
    other: &'static str,
}

impl SettingsView {
    pub(super) fn render_shortcuts_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let platform = Platform::current();
        let smart = self.store.read(cx).config().smart_copy_paste;
        // 此刻 keymap 里生效的那份 —— 显示、「已修改」、判冲突都以它为准
        let overrides = hotkeys::applied_overrides();
        let mut root = page_root();

        if !overrides.is_empty() {
            root = root.child(self.render_reset_all(cx));
        }

        for (group_key, items) in hotkeys::groups() {
            let mut rows = div().flex().flex_col().gap(px(4.0));
            for def in items {
                rows = rows.child(self.render_hotkey_row(def, &overrides, platform, cx));
            }
            // 智能 Ctrl+C/V 开启时才存在,附在「复制粘贴」组末尾(终端自己消费,只读)
            if smart && group_key == "shortcuts.clipboard" {
                for (desc_key, keystroke) in hotkeys::SMART_COPY_PASTE {
                    rows = rows.child(read_only_row(
                        SharedString::from(format!("hotkey-ro-{desc_key}")),
                        t("settings", desc_key),
                        hotkeys::keystroke_label_for(keystroke, platform),
                        "shortcuts.readOnlyTerminal",
                    ));
                }
            }
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(ui::settings_section_title(t("settings", group_key)))
                    .child(rows),
            );
        }

        root.child(ui::hint(t("settings", "shortcuts.footer")))
            .into_any_element()
    }

    /// 页顶的「全部恢复默认」(有改动时才出),点一下先就地确认一次。
    fn render_reset_all(&self, cx: &mut Context<Self>) -> AnyElement {
        let bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(8.0));
        if !self.shortcuts.reset_confirm {
            return bar
                .child(
                    ui::ghost_button("hotkeys-reset-all", t("settings", "shortcuts.resetAll"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.leave_shortcuts_page(cx);
                            this.shortcuts.reset_confirm = true;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        bar.child(
            div()
                .text_size(ui::font_px(12.0))
                .text_color(ui::color_warning())
                .child(t("settings", "shortcuts.resetAllConfirm")),
        )
        .child(
            ui::danger_button(
                "hotkeys-reset-all-ok",
                t("settings", "shortcuts.resetAllOk"),
            )
            .on_click(cx.listener(|this, _, _window, cx| {
                this.leave_shortcuts_page(cx);
                this.store
                    .update(cx, |store, cx| store.reset_hotkeys(None, cx));
                cx.notify();
            })),
        )
        .child(
            ui::ghost_button("hotkeys-reset-all-cancel", t("settings", "common.cancel")).on_click(
                cx.listener(|this, _, _window, cx| {
                    this.shortcuts.reset_confirm = false;
                    cx.notify();
                }),
            ),
        )
        .into_any_element()
    }

    fn render_hotkey_row(
        &self,
        def: &'static HotkeyDef,
        overrides: &Overrides,
        platform: Platform,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = def.id;
        let desc = t("settings", def.desc_key);
        let label = hotkeys::label_for(def, overrides, platform);
        if let Some(reason) = hotkeys::read_only_reason(def) {
            return read_only_row(
                SharedString::from(format!("hotkey-ro-{id}")),
                desc,
                label.unwrap_or_default(),
                reason,
            );
        }

        let modified = hotkeys::is_modified(def, overrides, platform);
        let recording = self.shortcuts.recording.as_ref().filter(|r| r.id == id);
        let conflict = self.shortcuts.conflict.as_ref().filter(|c| c.id == id);
        let notice = self
            .shortcuts
            .notice
            .as_ref()
            .filter(|(notice_id, _)| *notice_id == id)
            .map(|(_, text)| text.clone());

        let control = match recording {
            // 录制框自己 `track_focus`:点它不会被面板根节点抢走焦点(那会被当成失焦结束录制)
            Some(rec) => div()
                .id(SharedString::from(format!("hotkey-rec-{id}")))
                .track_focus(&rec.focus)
                .flex_none()
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::accent())
                .bg(ui::accent_subtle())
                .text_size(ui::font_px(11.0))
                .text_color(ui::accent())
                .child(t("settings", "shortcuts.recording"))
                .into_any_element(),
            None => div()
                .id(SharedString::from(format!("hotkey-key-{id}")))
                .flex_none()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|el| el.opacity(0.7))
                .child(match label {
                    Some(label) => ui::kbd(label),
                    None => {
                        ui::kbd(t("settings", "shortcuts.unbound")).text_color(ui::text_muted())
                    }
                })
                .tip(t("settings", "shortcuts.clickToChange"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_hotkey_recording(id, window, cx);
                }))
                .into_any_element(),
        };
        let reset = (modified && recording.is_none()).then(|| {
            div()
                .id(SharedString::from(format!("hotkey-reset-{id}")))
                .flex_none()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .cursor_pointer()
                .hover(|el| el.text_color(ui::accent()))
                .child(t("settings", "shortcuts.resetOne"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.reset_hotkey(id, cx);
                }))
        });

        let head = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.0))
            .child(
                div()
                    .flex()
                    .min_w_0()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(desc),
                    )
                    .when(modified, |el| el.child(modified_badge())),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.0))
                    .children(reset)
                    .child(control),
            );

        let mut card = ui::settings_card()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(head);
        if let Some(rec) = recording {
            card = card.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("settings", "shortcuts.recordingHint")),
            );
            if let Some(problem) = &rec.problem {
                card = card.child(problem_line(problem));
            }
        }
        if let Some(conflict) = conflict {
            card = card.child(self.conflict_line(conflict, platform, cx));
        }
        if let Some(text) = notice {
            card = card.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_warning())
                    .child(text),
            );
        }
        card.into_any_element()
    }

    /// 「Ctrl+Shift+D 已被『向右分屏』占用  [替换(那一条解绑)] [取消]」。
    fn conflict_line(
        &self,
        conflict: &HotkeyConflict,
        platform: Platform,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = conflict.id;
        let other = hotkeys::find(conflict.other).map_or("", |def| t("settings", def.desc_key));
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_warning())
                    .child(tr!(
                        "settings",
                        "shortcuts.conflict",
                        keys = hotkeys::keystroke_label_for(&conflict.keystroke, platform),
                        name = other
                    )),
            )
            .child(
                ui::primary_button(
                    SharedString::from(format!("hotkey-replace-{id}")),
                    t("settings", "shortcuts.conflictReplace"),
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    let Some(conflict) = this.shortcuts.conflict.take() else {
                        return;
                    };
                    this.commit_hotkeys(
                        conflict.id,
                        &[
                            (conflict.other, None),
                            (conflict.id, Some(conflict.keystroke)),
                        ],
                        cx,
                    );
                })),
            )
            .child(
                ui::ghost_button(
                    SharedString::from(format!("hotkey-cancel-{id}")),
                    t("settings", "common.cancel"),
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.shortcuts.conflict = None;
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    // ── 录制 ──

    fn start_hotkey_recording(
        &mut self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.leave_shortcuts_page(cx);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let view = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = view.update(cx, |this, cx| this.on_hotkey_keystroke(event, window, cx));
        });
        // 点了面板别处 = 不录了
        let blur = cx.on_blur(&focus, window, |this, _window, cx| {
            this.finish_hotkey_recording(None, cx);
        });
        // 切到别的应用 = 不录了(回来时不该还有一个吞键的拦截器挂着)
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.finish_hotkey_recording(None, cx);
            }
        });
        self.shortcuts.recording = Some(HotkeyRecording {
            id,
            focus,
            window: window.window_handle().window_id(),
            problem: None,
            _subs: [intercept, blur, activation],
        });
        cx.notify();
    }

    /// 结束录制(撤掉拦截)。拿得到 `window` 且焦点还在录制框上时把焦点交还面板根节点 ——
    /// 录制框这一帧就不画了,焦点落空的话 ↑↓ 换页失灵。
    fn finish_hotkey_recording(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let Some(recording) = self.shortcuts.recording.take() else {
            return;
        };
        if let Some(window) = window
            && recording.focus.is_focused(window)
        {
            window.focus(&self.focus, cx);
        }
        drop(recording);
        cx.notify();
    }

    /// 离开快捷键页(或开始新一轮操作)时把这一页的临时状态清干净:录制、待选的冲突、
    /// 就地提醒、全部恢复默认的确认。换分页时由 `SettingsView::set_page` 调。
    pub(super) fn leave_shortcuts_page(&mut self, cx: &mut Context<Self>) {
        self.finish_hotkey_recording(None, cx);
        self.shortcuts.conflict = None;
        self.shortcuts.notice = None;
        self.shortcuts.reset_confirm = false;
    }

    /// 录制中每一次按键(拦截器回调,在匹配绑定之前)。
    fn on_hotkey_keystroke(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(recording) = self.shortcuts.recording.as_ref() else {
            return;
        };
        let id = recording.id;
        let alive = recording.window == window.window_handle().window_id()
            && recording.focus.is_focused(window)
            && self.page == SettingsPage::Shortcuts
            && overlay::contains(overlay::key(overlay::kind::SETTINGS));
        if !alive {
            // 安全阀:录制态已经名不副实,先撤掉拦截,这次按键放行照常派发
            self.finish_hotkey_recording(None, cx);
            return;
        }
        // 从这里起这次按键归录制:不派发 action、不进 key 监听 —— Esc 不会关掉设置弹窗,
        // Tab 不会挪焦点,Ctrl+Shift+T 不会去开终端
        cx.stop_propagation();

        let platform = Platform::current();
        let pressed = || hotkeys::keystroke_label_for(&event.keystroke.unparse(), platform);
        match hotkeys::interpret_recorded(&event.keystroke, platform) {
            Recorded::Pending => {}
            Recorded::Cancel => self.finish_hotkey_recording(Some(window), cx),
            Recorded::Unbind => {
                self.finish_hotkey_recording(Some(window), cx);
                self.commit_hotkeys(id, &[(id, None)], cx);
            }
            Recorded::Invalid(reason) => {
                self.set_record_problem(pressed(), ProblemKind::Invalid(reason), cx);
            }
            Recorded::Combo(keystroke) => {
                let smart = self.store.read(cx).config().smart_copy_paste;
                let overrides = hotkeys::applied_overrides();
                match hotkeys::judge(id, &keystroke, &overrides, platform, smart) {
                    Verdict::Unchanged => self.finish_hotkey_recording(Some(window), cx),
                    Verdict::Taken(owner) => {
                        let keys = hotkeys::keystroke_label_for(&keystroke, platform);
                        self.set_record_problem(keys, ProblemKind::Taken(owner), cx);
                    }
                    Verdict::Conflict(other) => {
                        self.finish_hotkey_recording(Some(window), cx);
                        self.shortcuts.conflict = Some(HotkeyConflict {
                            id,
                            keystroke,
                            other,
                        });
                        cx.notify();
                    }
                    Verdict::Accept => {
                        self.finish_hotkey_recording(Some(window), cx);
                        self.commit_hotkeys(id, &[(id, Some(keystroke))], cx);
                    }
                }
            }
        }
    }

    fn set_record_problem(&mut self, keys: String, kind: ProblemKind, cx: &mut Context<Self>) {
        if let Some(recording) = self.shortcuts.recording.as_mut() {
            recording.problem = Some(RecordProblem { keys, kind });
            cx.notify();
        }
    }

    /// 单行「恢复默认」。默认键位此刻若被另一条占着(用户先前把别的动作改到了这个键上),
    /// 照样走「替换 / 取消」,不叠出两条同键绑定。
    fn reset_hotkey(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.leave_shortcuts_page(cx);
        let Some(def) = hotkeys::find(id) else {
            return;
        };
        let platform = Platform::current();
        let default = hotkeys::default_keystroke_for(def, platform);
        let smart = self.store.read(cx).config().smart_copy_paste;
        let overrides = hotkeys::applied_overrides();
        match hotkeys::judge(id, &default, &overrides, platform, smart) {
            Verdict::Conflict(other) => {
                self.shortcuts.conflict = Some(HotkeyConflict {
                    id,
                    keystroke: default,
                    other,
                });
                cx.notify();
            }
            _ => {
                self.store
                    .update(cx, |store, cx| store.reset_hotkeys(Some(id), cx));
                cx.notify();
            }
        }
    }

    /// 写进配置并即时生效(`AppStore::set_hotkeys`)。新键位是终端常用键时就地提醒一句
    /// (允许保存)。
    fn commit_hotkeys(
        &mut self,
        id: &'static str,
        changes: &[(&str, Option<String>)],
        cx: &mut Context<Self>,
    ) {
        self.store
            .update(cx, |store, cx| store.set_hotkeys(changes, cx));
        self.shortcuts.notice = changes
            .iter()
            .find(|(changed, _)| *changed == id)
            .and_then(|(_, keystroke)| keystroke.as_deref())
            .filter(|keystroke| hotkeys::terminal_warning(keystroke))
            .map(|keystroke| {
                let keys = hotkeys::keystroke_label(keystroke);
                (
                    id,
                    tr!("settings", "shortcuts.terminalWarning", keys = keys),
                )
            });
        cx.notify();
    }
}

/// 只读的一行:描述 + 「不可更改」(tooltip 说为什么)+ 键帽。
fn read_only_row(
    tip_id: SharedString,
    desc: &'static str,
    keys: String,
    reason_key: &'static str,
) -> AnyElement {
    ui::settings_card()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .min_w_0()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(desc),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .id(tip_id)
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("settings", "shortcuts.readOnly"))
                        .tip(t("settings", reason_key)),
                )
                .child(ui::kbd(keys)),
        )
        .into_any_element()
}

/// 被改过的行在描述旁边挂的小标记。
fn modified_badge() -> gpui::Div {
    div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(3.0))
        .bg(ui::accent_subtle())
        .text_size(ui::font_px(11.0))
        .text_color(ui::accent())
        .child(t("settings", "shortcuts.modified"))
}

/// 录到了但不收:按的那个键 + 为什么。
fn problem_line(problem: &RecordProblem) -> gpui::Div {
    let message = match problem.kind {
        ProblemKind::Invalid(reason) => t("settings", reason.message_key()).to_string(),
        ProblemKind::Taken(Owner::Named(desc)) => tr!(
            "settings",
            "shortcuts.takenReadOnly",
            name = t("settings", desc)
        ),
        ProblemKind::Taken(Owner::Internal) => t("settings", "shortcuts.takenInternal").to_string(),
    };
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(ui::kbd(problem.keys.clone()))
        .child(
            div()
                .min_w_0()
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_error())
                .child(message),
        )
}
