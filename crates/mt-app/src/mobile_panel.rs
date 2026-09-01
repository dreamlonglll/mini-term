//! 「移动端」面板(对照 `src/components/MobileRelayModal.tsx` 249 行 +
//! `AiLauncherSection.tsx` 212 行 + `RelayStatusBadge.tsx` 37 行)。
//!
//! 一站式:中转地址 / 桌面端密钥 → 连接状态 → 配对二维码 → 重置配对,
//! 中间嵌一段 AI 启动器的增删改。
//!
//! # 两处与原版的形态差异
//!
//! 1. **二维码自绘**。原版 `QRCode.toDataURL()` 出 data URL 塞 `<img>`;这里用
//!    `qrcode` crate 只取位矩阵,再用 `gpui::canvas` + `paint_quad` 逐模块画。
//!    底色**固定白**(不跟主题)—— 相机识别要高对比。静区 1 模块、纠错 M,
//!    与原版 `{ width: 260, margin: 1 }` 一字不差(几何见
//!    [`crate::mobile_relay::qr_module_px`])。配对码文本同屏可见,便于手输。
//! 2. **shell 选择走自建菜单**([`crate::menu`])而不是 `<select>` ——
//!    与 N 批的下拉风格一致,勾选态用「✓ 」文本方案。
//!
//! # 防叠开
//!
//! 走 [`crate::prompt::open_guarded`] + [`crate::overlay::kind::MOBILE_RELAY`]
//! (原版没有,是 audit 记的缺口)。重置配对的确认框是**另一种类**,照样叠得上去。

use gpui::{
    App, AppContext, Bounds, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, fill, point, prelude::FluentBuilder, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::AiLauncher;
use mt_ui::icons::{AiVendor, BrandIcon};

use crate::i18n::{t, tr};
use crate::menu;
use crate::mobile_relay::{
    self, QR_CANVAS_PX, QR_QUIET_MODULES, QrMatrix, RelayBridge, command_warning,
    launcher_draft_valid, launcher_subtitle, upsert_launcher,
};
use crate::prompt::{Confirm, autofocus, dialog_title, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

/// 面板宽度(原版 `w-[440px]`)。窄窗口下由
/// [`ui::clamp_dialog_width`] 压回窗口内。
const PANEL_W: f32 = 440.0;
/// 正文最大高度的**舒适上限**。原版是 `max-h-[76vh]`,实际生效值由
/// [`ui::clamp_dialog_body_height`] 按视口现算(U 批那条「定值 540px」的记档
/// 到此结清:定值在矮窗口上会把面板顶出视口底边)。
const BODY_MAX_H: f32 = 540.0;
/// 正文之外那圈的高度:`Dialog` 默认内边距 24 上 + 24 下、标题行约 19、
/// 标题与正文之间 `gap` 16(`dialog.rs:373/430-432`)。正文高度按它扣,
/// 面板整体才落在 76vh 内。
const CHROME_H: f32 = 83.0;

/// 编辑中的启动器草稿。`id` 为空串 = 新增(与原版 `DraftState` 一致)。
struct Draft {
    id: String,
    name: Entity<InputState>,
    command: Entity<InputState>,
    /// 绑定的 shell 名;`None` = 使用默认 shell。
    shell: Option<String>,
    /// 「允许编排」:用这条启动器起的会话能驱动别的 AI 会话(ADR 0003)。
    /// 新增草稿一律从 false 起 —— 编排能力只能是用户显式勾出来的。
    orchestration: bool,
}

pub struct MobilePanel {
    store: Entity<AppStore>,
    bridge: Entity<RelayBridge>,
    url: Entity<InputState>,
    key: Entity<InputState>,
    /// 位矩阵 + 配对码原文。**面板一关就随实体销毁** —— 留着的旧码可能已被
    /// 后续操作作废(原版关闭面板会 `setQrDataUrl(null)`)。
    qr: Option<(QrMatrix, String)>,
    /// 已经点过「生成」但码还没回来(渲染 `modal.qrWaiting`)。
    qr_requested: bool,
    draft: Option<Draft>,
    /// 地址 / 密钥两个输入框的回车订阅。
    _subs: Vec<Subscription>,
}

impl Render for MobilePanel {
    /// 状态盒子。真正的画面由 Dialog 的 builder 每帧重建(见 `modal.rs` 的说明)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl MobilePanel {
    /// 配对码到达([`RelayBridge`] 的泵调它)。
    pub fn set_pairing_code(&mut self, code: String, url: String, cx: &mut Context<Self>) {
        // 编码失败(理论上到不了:配对链接只有几十字节)时保持 `qr_requested`,
        // 面板继续显示「正在请求…」—— 与原版 `.catch(() => setQrDataUrl(null))` 同
        self.qr = mobile_relay::encode_qr(&url).map(|m| (m, code));
        cx.notify();
    }

    fn clear_qr(&mut self) {
        self.qr = None;
        self.qr_requested = false;
    }

    /// 「保存并连接」。地址框空着时什么都不做(原版按钮是 `disabled` 的,
    /// 回车那条路原版也走同一个 `applyRelaySettings` —— 只是它不判空,
    /// 这里补上,免得回车把配置里的地址清掉)。
    fn apply_now(&mut self, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        let key = self.key.read(cx).value().to_string();
        if url.is_empty() {
            return;
        }
        // 地址变了旧配对二维码即作废
        self.clear_qr();
        cx.notify();
        let bridge = self.bridge.clone();
        bridge.update(cx, |bridge, cx| bridge.apply_settings(&url, &key, cx));
    }
}

/// 打开「移动端」面板。
pub fn open(window: &mut Window, cx: &mut App) {
    // 守卫要在**建输入框之前**判:`open_guarded` 拦下来的时候焦点已经被新输入框
    // 抢走了(而它永远不会被画出来)—— 与 `prompt::show_prompt` 同一条
    if crate::overlay::contains(crate::overlay::key(kind::MOBILE_RELAY)) {
        return;
    }
    let Some(bridge) = mobile_relay::bridge(cx) else {
        return;
    };
    let store = AppStore::global(cx);
    let relay = store.read(cx).mobile_relay();

    let url = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("mobileRelay", "urlPlaceholder"))
            .default_value(relay.relay_url.clone())
    });
    // 原版是 `type="password"`;gpui-component 的对应物是 `masked` ——
    // 密钥不该明文常驻在屏幕上
    let key = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(t("mobileRelay", "keyPlaceholder"))
            .default_value(relay.desktop_key.clone())
    });

    // 打开即可直接改中转地址(密钥框是 masked,不该抢焦点)。聚焦排在
    // `open_guarded` 之后,判据见 `prompt::autofocus`
    let url_for_focus = url.clone();

    let state = cx.new(|cx| {
        // 两个输入框里按回车 = 点「保存并连接」(原版 `onKeyDown` 那两处)。
        // `InputState` 在单行模式下无条件 emit `PressEnter`,订阅它比抢键位直白。
        let on_enter = |this: &mut MobilePanel, _, event: &InputEvent, cx: &mut Context<_>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.apply_now(cx);
            }
        };
        let subs = vec![
            cx.subscribe(&url, on_enter),
            cx.subscribe(&key, on_enter),
        ];
        MobilePanel {
            store: store.clone(),
            bridge: bridge.clone(),
            url,
            key,
            qr: None,
            qr_requested: false,
            draft: None,
            _subs: subs,
        }
    });

    // 配对码的去处。面板销毁后弱引用升不上来,泵那边直接丢弃
    bridge.update(cx, |bridge, _| bridge.set_panel(Some(state.downgrade())));

    // 打开时兜底刷一次状态(原版 `invoke('mobile_relay_status')`)
    let status = bridge.read(cx).manager().current_status();
    store.update(cx, |store, cx| store.set_mobile_relay_status(status, cx));

    open_guarded(kind::MOBILE_RELAY, window, cx, move |dialog, window, cx| {
        // 原版 `w-[440px] max-h-[76vh]`(`MobileRelayModal.tsx:119`)——
        // 两个尺寸都按视口现算,窄/矮窗口下面板才不会出界
        let viewport = window.viewport_size();
        let body = render_body(
            &state,
            ui::clamp_dialog_body_height(px(BODY_MAX_H), viewport, 0.76, px(CHROME_H)),
            cx,
        );
        dialog
            // 面板没有底部按钮,右上角这颗 ✕ 是唯一看得见的出口 —— 必须自绘,
            // `Dialog::close_button` 画的是空白(见 `prompt::dialog_title`)
            .title(dialog_title(kind::MOBILE_RELAY, t("mobileRelay", "modal.title")))
            .w(ui::clamp_dialog_width(px(PANEL_W), viewport))
            // 面板内有未保存的地址/密钥输入与配对操作,误点外侧关闭会丢内容;
            // Esc 仍可退(原版 `closeOnOverlay={false}`)
            .overlay_closable(false)
            .child(body)
    });

    autofocus(&url_for_focus, window, cx);
}

// ─── 动作 ─────────────────────────────────────────────────────

/// 保存中转地址 + 桌面端密钥并重建连接(原版 `applyRelaySettings`)。
fn apply_settings(state: &Entity<MobilePanel>, url: String, key: String, cx: &mut App) {
    // 地址变了旧配对二维码即作废,一并清掉
    state.update(cx, |panel, cx| {
        panel.clear_qr();
        cx.notify();
    });
    let bridge = state.read(cx).bridge.clone();
    bridge.update(cx, |bridge, cx| bridge.apply_settings(&url, &key, cx));
}

fn request_pairing_code(state: &Entity<MobilePanel>, cx: &mut App) {
    let manager = state.read(cx).bridge.read(cx).manager();
    state.update(cx, |panel, cx| {
        panel.qr = None;
        panel.qr_requested = true;
        cx.notify();
    });
    if manager.request_pairing_code().is_err() {
        // 发不出去(连接刚断)→ 把「正在请求…」撤回,否则面板一直转
        state.update(cx, |panel, cx| {
            panel.qr_requested = false;
            cx.notify();
        });
    }
}

fn reset_pairing(state: &Entity<MobilePanel>, window: &mut Window, cx: &mut App) {
    let state = state.clone();
    Confirm::new(
        t("mobileRelay", "modal.resetPairing"),
        t("mobileRelay", "modal.resetConfirm"),
    )
    .open(
        move |_window, cx| {
            state.update(cx, |panel, cx| {
                panel.clear_qr();
                cx.notify();
            });
            // 失败静默(与原版 `.catch(() => {})` 同):结果经状态回调的
            // `paired` 字段推回来,不必在这里报错
            let _ = state.read(cx).bridge.read(cx).manager().reset_pairing();
        },
        window,
        cx,
    );
}

/// 落盘启动器名单 + 让中转重发全量快照。
fn save_launchers(state: &Entity<MobilePanel>, launchers: Vec<AiLauncher>, cx: &mut App) {
    let bridge = state.read(cx).bridge.clone();
    bridge.update(cx, |bridge, cx| bridge.save_launchers(launchers, cx));
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 面板一帧要用到的全部只读数据。
///
/// 先整块读出来再画:后面每个 `render_*` 都要 `&mut App`(建输入框 / 弹菜单),
/// 而 `state.read(cx)` 的借用会一路活到语句末 —— 两者在同一个表达式里打架。
struct Frame {
    relay_url: String,
    status: Option<mt_relay::MobileRelayStatusPayload>,
    paired: Option<bool>,
    connected: bool,
    url_value: String,
    qr: Option<(QrMatrix, String)>,
    qr_requested: bool,
    launchers: Vec<AiLauncher>,
    /// 草稿的轻量切面(输入框实体是 `Clone` 的句柄)。
    draft: Option<DraftFacet>,
    shells: Vec<String>,
}

/// 草稿在一帧里用到的只读切面。
///
/// 没有 `id`:保存那一刻是从 `panel.draft` 现读的(表单里的输入框实体也在那边),
/// 这份切面只服务渲染。
#[derive(Clone)]
struct DraftFacet {
    name: Entity<InputState>,
    command: Entity<InputState>,
    shell: Option<String>,
    orchestration: bool,
}

fn read_frame(state: &Entity<MobilePanel>, cx: &App) -> Frame {
    let panel = state.read(cx);
    let store = panel.store.read(cx);
    let relay = store.mobile_relay();
    let status = store.mobile_relay_status().cloned();
    Frame {
        relay_url: relay.relay_url,
        paired: status.as_ref().and_then(|s| s.paired),
        connected: status.as_ref().map(|s| s.status.as_str()) == Some("connected"),
        status,
        url_value: panel.url.read(cx).value().to_string(),
        qr: panel.qr.clone(),
        qr_requested: panel.qr_requested,
        launchers: relay.launchers,
        draft: panel.draft.as_ref().map(|d| DraftFacet {
            name: d.name.clone(),
            command: d.command.clone(),
            shell: d.shell.clone(),
            orchestration: d.orchestration,
        }),
        shells: store
            .config()
            .available_shells
            .iter()
            .map(|s| s.name.clone())
            .collect(),
    }
}

/// `max_body_h` 由调用方按视口现算(见 [`open`] 与
/// [`ui::clamp_dialog_body_height`]),不是常量 —— 矮窗口下定值会把面板
/// 顶出视口底边。
fn render_body(
    state: &Entity<MobilePanel>,
    max_body_h: gpui::Pixels,
    cx: &mut App,
) -> gpui::AnyElement {
    let frame = read_frame(state, cx);
    let Frame {
        relay_url,
        status,
        paired,
        connected,
        url_value,
        ..
    } = &frame;
    let (relay_url, paired, connected) = (relay_url.clone(), *paired, *connected);
    let status = status.clone();
    let url_value = url_value.clone();

    let mut body = div()
        .id("mobile-relay-body")
        .max_h(max_body_h)
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .flex()
        .flex_col()
        .gap(px(16.0))
        // 1. 说明段
        .child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("mobileRelay", "intro")),
        )
        // 2~8. 地址 / 密钥 / 两颗按钮
        .child(render_endpoint_section(state, &url_value, &relay_url, cx))
        // 9. AI 启动器(与是否连上中转无关,始终可编辑)
        .child(render_launchers(state, &frame, cx))
        // 10. 连接状态行
        .child(
            row_card()
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(t("mobileRelay", "statusLabel")),
                )
                .child(render_status_badge(status.as_ref())),
        );

    if relay_url.trim().is_empty() {
        // 11. 未配置:后面的配对/二维码整段都不渲染
        return body
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("mobileRelay", "modal.notConfigured")),
            )
            .into_any_element();
    }

    // 12. 配对状态行
    body = body.child(
        row_card()
            .child(
                div()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(t("mobileRelay", "modal.pairedLabel")),
            )
            .child(
                div()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_secondary())
                    .child(match paired {
                        Some(true) => t("mobileRelay", "modal.paired"),
                        Some(false) => t("mobileRelay", "modal.notPaired"),
                        None => t("mobileRelay", "modal.pairedUnknown"),
                    }),
            ),
    );

    if !connected {
        // 13'. 没连上就没有二维码可生成
        return body
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("mobileRelay", "modal.needConnected")),
            )
            .into_any_element();
    }

    let mut qr_section = div().flex().flex_col().gap(px(12.0));
    if let Some((matrix, code)) = frame.qr.as_ref() {
        qr_section = qr_section.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.0))
                .child(render_qr(matrix))
                // 配对码文本同屏可见:扫不出码时还能手输
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(SharedString::from(code.clone())),
                )
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("mobileRelay", "modal.qrHint")),
                ),
        );
    } else if frame.qr_requested {
        qr_section = qr_section.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("mobileRelay", "modal.qrWaiting")),
        );
    }

    // 14. 生成 / 重新生成;15. 重置配对(仅已配对时)
    let has_qr = frame.qr.is_some();
    let mut buttons = div().flex().gap(px(8.0)).child(
        accent_button(
            "relay-gen-qr",
            if has_qr {
                t("mobileRelay", "modal.regenerateQr")
            } else {
                t("mobileRelay", "modal.generateQr")
            },
        )
        .on_click({
            let state = state.clone();
            move |_: &ClickEvent, _window, cx| request_pairing_code(&state, cx)
        }),
    );
    if paired == Some(true) {
        buttons = buttons.child(
            danger_ghost_button("relay-reset-pairing", t("mobileRelay", "modal.resetPairing"))
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window, cx| reset_pairing(&state, window, cx)
                }),
        );
    }

    body.child(qr_section.child(buttons)).into_any_element()
}

/// 地址 / 密钥 / 「保存并连接」/「断开并清除」。
fn render_endpoint_section(
    state: &Entity<MobilePanel>,
    url_value: &str,
    saved_url: &str,
    cx: &mut App,
) -> impl IntoElement {
    let (url_input, key_input) = {
        let panel = state.read(cx);
        (panel.url.clone(), panel.key.clone())
    };
    let can_apply = !url_value.trim().is_empty();
    // 「断开并清除」在地址框与已存地址都空时才灰掉
    let can_clear = can_apply || !saved_url.trim().is_empty();

    div()
        .flex()
        .flex_col()
        .child(field_label(t("mobileRelay", "urlLabel")))
        .child(Input::new(&url_input))
        .child(div().mt(px(12.0)).child(field_label(t("mobileRelay", "keyLabel"))))
        .child(Input::new(&key_input))
        .child(
            div()
                .mt(px(4.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("mobileRelay", "keyHint")),
        )
        .child(
            div()
                .mt(px(8.0))
                .flex()
                .gap(px(8.0))
                .child(
                    accent_button("relay-apply", t("mobileRelay", "apply"))
                        .opacity(if can_apply { 1.0 } else { 0.4 })
                        .on_click({
                            let state = state.clone();
                            move |_: &ClickEvent, _window, cx| {
                                state.update(cx, |panel, cx| panel.apply_now(cx));
                            }
                        }),
                )
                .child(
                    ui::ghost_button("relay-clear", t("mobileRelay", "clear"))
                        .opacity(if can_clear { 1.0 } else { 0.4 })
                        .on_click({
                            let state = state.clone();
                            move |_: &ClickEvent, window, cx| {
                                // 地址框与已存地址都空 = 原版的 disabled 态,
                                // 点了不该白写一次配置(密钥会被顺手 trim 落盘)
                                if !can_clear {
                                    return;
                                }
                                // 清地址但**保留密钥与启动器** —— 它们与「这次是否
                                // 建连」无关,别让用户重填
                                let (key, url_input) = {
                                    let panel = state.read(cx);
                                    (
                                        panel.key.read(cx).value().to_string(),
                                        panel.url.clone(),
                                    )
                                };
                                url_input.update(cx, |input, cx| input.set_value("", window, cx));
                                apply_settings(&state, String::new(), key, cx);
                            }
                        }),
                ),
        )
}

/// AI 启动器段(`AiLauncherSection.tsx`)。
fn render_launchers(
    state: &Entity<MobilePanel>,
    frame: &Frame,
    cx: &mut App,
) -> impl IntoElement {
    let launchers = frame.launchers.clone();
    let editing = frame.draft.is_some();

    let mut section = div()
        .flex()
        .flex_col()
        .child(field_label(t("mobileRelay", "launchers.title")))
        .child(
            div()
                .mb(px(8.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("mobileRelay", "launchers.intro")),
        );

    // 空名单警告(**红字**:手机将无法发起新会话)
    if launchers.is_empty() && !editing {
        section = section.child(
            div()
                .mb(px(8.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_error())
                .child(t("mobileRelay", "launchers.empty")),
        );
    }

    let mut list = div().flex().flex_col().gap(px(6.0));
    for launcher in &launchers {
        list = list.child(render_launcher_row(state, launcher, &launchers));
    }
    section = section.child(list);

    // 编辑中的行**仍然显示**,草稿表单渲染在列表下方(与 `modal.rs` 的终端配置
    // 「编辑中的行让位给表单」不同形,别照搬那边)
    match frame.draft.as_ref() {
        Some(draft) => section.child(render_draft_form(state, draft, frame, cx)),
        None => section.child(
            div().mt(px(8.0)).child(
                ui::ghost_button(
                    "launcher-add",
                    format!("+ {}", t("mobileRelay", "launchers.add")),
                )
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window, cx| {
                        let name = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(t("mobileRelay", "launchers.namePlaceholder"))
                        });
                        let command = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(t("mobileRelay", "launchers.commandPlaceholder"))
                        });
                        let name_for_focus = name.clone();
                        state.update(cx, |panel, cx| {
                            panel.draft = Some(Draft {
                                id: String::new(),
                                name,
                                command,
                                shell: None,
                                // 新启动器默认**不**带编排能力(fail-closed)
                                orchestration: false,
                            });
                            cx.notify();
                        });
                        // 点了「+ 添加」就该能直接敲名字,不必再点一下表单
                        autofocus(&name_for_focus, window, cx);
                    }
                }),
            ),
        ),
    }
}

/// 一条启动器:品牌图标 + 名称 + 副行 + 编辑 / 删除。
fn render_launcher_row(
    state: &Entity<MobilePanel>,
    launcher: &AiLauncher,
    all: &[AiLauncher],
) -> impl IntoElement {
    let id = launcher.id.clone();
    let name = launcher.name.clone();
    let command = launcher.command.clone();
    let shell = launcher.shell.clone();
    let orchestration = launcher.orchestration;
    let subtitle = launcher_subtitle(shell.as_deref(), &command);
    // 从命令文本推断品牌(识别不出回退 Bot)
    let vendor = AiVendor::infer(None, Some(&command));
    let rest: Vec<AiLauncher> = all.iter().filter(|l| l.id != id).cloned().collect();

    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex_none()
                .text_color(ui::text_secondary())
                .child(BrandIcon::new(vendor).size(px(16.0))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(SharedString::from(name.clone())),
                        )
                        // 授了编排能力的条目在列表里一眼看得出来 —— 这是个
                        // 权限位,不该只在编辑表单里才可见
                        .when(orchestration, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .px(px(6.0))
                                    .rounded(px(3.0))
                                    .bg(ui::accent_muted())
                                    .text_size(ui::font_px(10.0))
                                    .text_color(ui::accent())
                                    .child(t("mobileRelay", "launchers.orchestrationBadge")),
                            )
                        }),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(SharedString::from(subtitle)),
                ),
        )
        .child(
            ui::ghost_button(
                SharedString::from(format!("launcher-edit-{id}")),
                t("mobileRelay", "launchers.edit"),
            )
            .on_click({
                let state = state.clone();
                let id = id.clone();
                let name = name.clone();
                let command = command.clone();
                let shell = shell.clone();
                move |_: &ClickEvent, window, cx| {
                    let name_input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(t("mobileRelay", "launchers.namePlaceholder"))
                            .default_value(name.clone())
                    });
                    let command_input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(t("mobileRelay", "launchers.commandPlaceholder"))
                            .default_value(command.clone())
                    });
                    let name_for_focus = name_input.clone();
                    state.update(cx, |panel, cx| {
                        panel.draft = Some(Draft {
                            id: id.clone(),
                            name: name_input,
                            command: command_input,
                            shell: shell.clone(),
                            orchestration,
                        });
                        cx.notify();
                    });
                    // 点了「编辑」就该能直接改名字
                    autofocus(&name_for_focus, window, cx);
                }
            }),
        )
        .child(
            ui::danger_button(
                SharedString::from(format!("launcher-del-{id}")),
                t("mobileRelay", "launchers.delete"),
            )
            // **无二次确认**(原版就是直接删)
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, _window, cx| save_launchers(&state, rest.clone(), cx)
            }),
        )
}

/// 草稿表单:名称 / shell 选择 / 命令 / 警告 / 保存 / 取消。
fn render_draft_form(
    state: &Entity<MobilePanel>,
    draft: &DraftFacet,
    frame: &Frame,
    cx: &mut App,
) -> impl IntoElement {
    let (name_input, command_input, shell) = (
        draft.name.clone(),
        draft.command.clone(),
        draft.shell.clone(),
    );
    let orchestration = draft.orchestration;
    let name_value = name_input.read(cx).value().to_string();
    let command_value = command_input.read(cx).value().to_string();
    let shell_label = shell
        .clone()
        .map(SharedString::from)
        .unwrap_or_else(|| t("mobileRelay", "launchers.defaultShell").into());
    // 同步纯函数,原版那套 `cancelled` 防竞态整个不需要
    let warn = command_warning(&command_value);
    let can_save = launcher_draft_valid(&name_value, &command_value);
    let shells = frame.shells.clone();
    let all = frame.launchers.clone();

    let mut form = div()
        .mt(px(8.0))
        .p(px(12.0))
        .rounded(px(4.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(ui::border_default())
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(Input::new(&name_input))
        // shell 选择:自建菜单的 picker 按钮(第一项「使用默认 shell」)
        .child(
            div()
                .id("launcher-shell-picker")
                .flex()
                .items_center()
                .justify_between()
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(4.0))
                .bg(ui::bg_surface())
                .border_1()
                .border_color(ui::border_default())
                .cursor_pointer()
                .hover(|el| el.border_color(ui::accent()))
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(shell_label)
                .child(div().text_color(ui::text_muted()).child("▾"))
                .on_click({
                    let state = state.clone();
                    let current = shell.clone();
                    move |event: &ClickEvent, window, cx| {
                        let mut entries = Vec::new();
                        let mark = |on: bool| if on { "✓ " } else { "   " };
                        entries.push(menu::item(
                            format!(
                                "{}{}",
                                mark(current.is_none()),
                                t("mobileRelay", "launchers.defaultShell")
                            ),
                            {
                                let state = state.clone();
                                move |_window, cx| set_draft_shell(&state, None, cx)
                            },
                        ));
                        for name in &shells {
                            let selected = current.as_deref() == Some(name.as_str());
                            entries.push(menu::item(format!("{}{name}", mark(selected)), {
                                let state = state.clone();
                                let name = name.clone();
                                move |_window, cx| set_draft_shell(&state, Some(name.clone()), cx)
                            }));
                        }
                        menu::show(event.position(), entries, window, cx);
                    }
                }),
        )
        .child(Input::new(&command_input));

    if warn {
        // **黄字不是红字** —— 它不阻塞保存
        form = form.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_ai_working())
                .child(t("mobileRelay", "launchers.commandWarning")),
        );
    }

    // 「允许编排」开关(ADR 0003 的信任根:编排能力**只能**从这里授予)。
    // 与命令警告同为表单的收尾段,但它是**授权**不是提示,所以带一行说明。
    form = form.child(
        div()
            .id("launcher-orchestration")
            .flex()
            .items_start()
            .gap(px(8.0))
            .cursor_pointer()
            .child(
                div()
                    .mt(px(2.0))
                    .child(ui::checkbox("launcher-orchestration-box", orchestration)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(t("mobileRelay", "launchers.orchestration")),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(t("mobileRelay", "launchers.orchestrationHint")),
                    ),
            )
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, _window, cx| {
                    state.update(cx, |panel, cx| {
                        if let Some(draft) = panel.draft.as_mut() {
                            draft.orchestration = !draft.orchestration;
                        }
                        cx.notify();
                    });
                }
            }),
    );

    form.child(
        div()
            .flex()
            .gap(px(8.0))
            .child(
                accent_button("launcher-save", t("mobileRelay", "launchers.save"))
                    .opacity(if can_save { 1.0 } else { 0.4 })
                    .on_click({
                        let state = state.clone();
                        move |_: &ClickEvent, _window, cx| {
                            let (name, command, shell, id, orchestration) = {
                                let panel = state.read(cx);
                                let Some(draft) = panel.draft.as_ref() else {
                                    return;
                                };
                                (
                                    draft.name.read(cx).value().to_string(),
                                    draft.command.read(cx).value().to_string(),
                                    draft.shell.clone(),
                                    draft.id.clone(),
                                    draft.orchestration,
                                )
                            };
                            if !launcher_draft_valid(&name, &command) {
                                return;
                            }
                            let next = upsert_launcher(
                                &all,
                                &id,
                                &name,
                                shell.as_deref(),
                                &command,
                                orchestration,
                            );
                            // 先收表单再落盘(原版同序)
                            state.update(cx, |panel, cx| {
                                panel.draft = None;
                                cx.notify();
                            });
                            save_launchers(&state, next, cx);
                        }
                    }),
            )
            .child(
                ui::ghost_button("launcher-cancel", t("mobileRelay", "launchers.cancel")).on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, _window, cx| {
                        state.update(cx, |panel, cx| {
                            panel.draft = None;
                            cx.notify();
                        });
                    }
                }),
            ),
    )
}

fn set_draft_shell(state: &Entity<MobilePanel>, shell: Option<String>, cx: &mut App) {
    state.update(cx, |panel, cx| {
        if let Some(draft) = panel.draft.as_mut() {
            draft.shell = shell;
        }
        cx.notify();
    });
}

/// 连接状态徽章(`RelayStatusBadge.tsx`):彩色圆点 + 文案。
///
/// `connecting` / `reconnecting` 原版带 `animate-blink`;这里**不做闪烁**
/// (用户机器 `prefers-reduced-motion: reduce`,原本就看不见),
/// **但颜色必须对**:三种「配置问题」终态是红色 —— 它们**已停止重连**,
/// 画成灰色的话用户会一直等一个永远不会来的重连。
fn render_status_badge(status: Option<&mt_relay::MobileRelayStatusPayload>) -> impl IntoElement {
    let raw = status.map(|s| s.status.as_str()).unwrap_or("disconnected");
    let color = match raw {
        "connected" => ui::color_success(),
        "connecting" | "reconnecting" => ui::color_ai_working(),
        "versionMismatch" | "authFailed" | "keyNotConfigured" => ui::color_error(),
        // "disconnected" 与认不出的串一样回落淡色
        _ => ui::text_muted(),
    };
    // 动态 key 必须写成 match(不能拼字符串):`t()` 的 debug_assert 与
    // `i18n.rs` 的 `USED_KEYS` 表都要求 key 是字面量
    let text: SharedString = match raw {
        "connected" => t("mobileRelay", "status.connected").into(),
        "connecting" => t("mobileRelay", "status.connecting").into(),
        "reconnecting" => t("mobileRelay", "status.reconnecting").into(),
        "authFailed" => t("mobileRelay", "status.authFailed").into(),
        "keyNotConfigured" => t("mobileRelay", "status.keyNotConfigured").into(),
        "versionMismatch" => tr!(
            "mobileRelay",
            "status.versionMismatch",
            expected = status
                .and_then(|s| s.expected_version)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into()),
            actual = status
                .and_then(|s| s.actual_version)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into()),
        )
        .into(),
        _ => t("mobileRelay", "status.disconnected").into(),
    };

    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .max_w(px(PANEL_W * 0.7))
        .child(
            div()
                .flex_none()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(color),
        )
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_secondary())
                .child(text),
        )
}

/// 二维码本体。**白底固定**,不跟主题 —— 相机识别需要高对比。
fn render_qr(matrix: &QrMatrix) -> impl IntoElement {
    let width = matrix.width;
    let module = matrix.module_px;
    let draw = matrix.draw_px;
    let dark = matrix.dark.clone();

    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(QR_CANVAS_PX))
        .h(px(QR_CANVAS_PX))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_subtle())
        .bg(gpui::white())
        .child(
            div().w(px(draw)).h(px(draw)).child(
                canvas(
                    |_bounds: Bounds<Pixels>, _window, _cx| {},
                    move |bounds, _prepaint, window, _cx| {
                        // 静区已经含在 draw 里,模块从 (quiet, quiet) 起画
                        let origin = bounds.origin;
                        let offset = module * QR_QUIET_MODULES as f32;
                        for y in 0..width {
                            for x in 0..width {
                                if !dark[y * width + x] {
                                    continue;
                                }
                                let rect = Bounds::new(
                                    point(
                                        origin.x + px(offset + x as f32 * module),
                                        origin.y + px(offset + y as f32 * module),
                                    ),
                                    size(px(module), px(module)),
                                );
                                window.paint_quad(fill(rect, gpui::black()));
                            }
                        }
                    },
                )
                .size_full(),
            ),
        )
}

// ─── 小件 ─────────────────────────────────────────────────────

/// 段标签(原版 `text-base text-[--text-muted] uppercase tracking-[0.1em] mb-2`)。
///
/// 复用设置页那款「大写 + 字距的灰色小标题」——**不是** [`ui::section_title`]
/// (那个是用量面板的「竖条 + 文字」),只把间距改回原版的 `mb-2` = 8px。
fn field_label(text: impl Into<SharedString>) -> gpui::Div {
    ui::settings_section_title(text).mb(px(8.0))
}

/// 状态 / 配对那两行的卡片容器。
fn row_card() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(ui::border_subtle())
}

/// accent 系实心按钮(原版 `bg-[--accent-muted] text-[--accent] border-[--accent]`)。
///
/// 与 [`ui::primary_button`] 不同款:那个是**实心 accent 底 + 反白字**,
/// 这个是淡底 + accent 字 + accent 边 —— 「移动端」面板通篇用的是后者。
fn accent_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(16.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(ui::accent_muted())
        .border_1()
        .border_color(ui::accent())
        .text_size(ui::font_px(13.0))
        .text_color(ui::accent())
        .cursor_pointer()
        .hover(|el| el.opacity(0.9))
        .child(label.into())
}

/// 「重置配对」那款:ghost 底 + error 字 + hover error 边框。
/// 现有三种按钮之外的第四款(见规格 §8)。
fn danger_ghost_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(16.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(ui::border_default())
        .text_size(ui::font_px(13.0))
        .text_color(ui::color_error())
        .cursor_pointer()
        .hover(|el| el.border_color(ui::color_error()))
        .child(label.into())
}
