//! 「移动端」面板(对照 `src/components/MobileRelayModal.tsx` 249 行 +
//! `RelayStatusBadge.tsx` 37 行)。
//!
//! 一站式:中转地址 / 桌面端密钥 → 连接状态 → 配对二维码 → 重置配对。
//! AI 启动器的增删改(原 `AiLauncherSection.tsx`)2026-09-02 迁到了设置 → Shell
//! (`settings::pages_launchers`),这里只留一句指路与「打开设置」入口 ——
//! 启动器同时是「新建终端」菜单的内容,放在 shell 列表旁边才找得到。
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
    Window, canvas, div, fill, point, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::i18n::{t, tr};
use crate::mobile_relay::{self, QR_CANVAS_PX, QR_QUIET_MODULES, QrMatrix, RelayBridge};
use crate::prompt::{Confirm, autofocus, close_guarded, dialog_title, kind, open_guarded};
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
        // 9. AI 启动器已迁入设置 → Shell,这里只留指路牌 + 入口
        .child(render_launchers_pointer())
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

/// AI 启动器的指路牌:增删改在设置 → Shell(`settings::pages_launchers`)。
///
/// 「打开设置」先收掉本面板再开设置:设置面板是 60vw 宽的大弹窗,叠在这个
/// 440px 的面板上面、关掉后再露出一个陈旧的移动端面板,不如干脆换过去。
/// 开设置排到 `defer`:与 `close_dialog` 同一帧里再 `open_dialog`,覆盖物栈
/// 的登记顺序会乱(`open_guarded` / `close_guarded` 各自只认栈顶)。
fn render_launchers_pointer() -> impl IntoElement {
    row_card()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("mobileRelay", "launchersMoved")),
        )
        .child(
            ui::ghost_button("relay-open-settings", t("mobileRelay", "openSettings")).on_click(
                |_: &ClickEvent, window, cx| {
                    close_guarded(kind::MOBILE_RELAY, window, cx);
                    window.defer(cx, |window, cx| {
                        let store = AppStore::global(cx);
                        crate::settings::open_settings(
                            store,
                            Some(crate::settings::SettingsPage::Terminal),
                            window,
                            cx,
                        );
                    });
                },
            ),
        )
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
