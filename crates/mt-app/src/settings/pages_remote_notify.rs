//! 设置面板的 remote-notify 页:远程通知(issue #87)。
//!
//! - **总开关打开后才出现**「通知渠道」多选与各渠道表单(关着时整块不画,与托盘
//!   子项同口径,见 [`super::tray_children_visible`]);勾选的渠道才展开表单;
//! - 输入框是**草稿态**(失焦 / 回车提交,设置页通例)。三处凭据提交时在 store 里封存
//!   ([`AppStore::set_remote_notify_secret`](crate::store::AppStore::set_remote_notify_secret)),
//!   打开面板时解开回填;解不开就回填空并就地标红,与移动端面板的桌面密钥同一套;
//! - 「测试发送」先把该渠道的草稿**全部提交**再发 —— 点按钮不一定让输入框先失焦;
//!   关面板时同理兜底再提交一次([`SettingsView::commit_remote_drafts`]);
//! - 结果与错误一律**就地显示**,不走 toast(toast 画在设置弹窗遮罩之下,看不见)。

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::RemoteNotifyConfig;

use crate::i18n::{t, tr};
use crate::remote_notify::{
    self, BARK_DEFAULT_SERVER, Channel, PushSecret, TELEGRAM_DEFAULT_API, WEBHOOK_PLACEHOLDERS,
};
use crate::ui;

use super::SettingsView;
use super::widgets::{banner, page_root, section, toggle_row};

/// 一次「测试发送」的进行态 / 结果。
#[derive(Clone)]
enum TestState {
    Sending,
    Ok,
    Failed(String),
}

/// 页上的七个输入框。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteField {
    BarkServer,
    BarkKey,
    TgToken,
    TgChat,
    TgApi,
    HookUrl,
    HookBody,
}

impl RemoteField {
    const ALL: [RemoteField; 7] = [
        Self::BarkServer,
        Self::BarkKey,
        Self::TgToken,
        Self::TgChat,
        Self::TgApi,
        Self::HookUrl,
        Self::HookBody,
    ];

    fn channel(self) -> Channel {
        match self {
            Self::BarkServer | Self::BarkKey => Channel::Bark,
            Self::TgToken | Self::TgChat | Self::TgApi => Channel::Telegram,
            Self::HookUrl | Self::HookBody => Channel::Webhook,
        }
    }

    fn secret(self) -> Option<PushSecret> {
        match self {
            Self::BarkKey => Some(PushSecret::BarkKey),
            Self::TgToken => Some(PushSecret::TelegramToken),
            Self::HookUrl => Some(PushSecret::WebhookUrl),
            _ => None,
        }
    }
}

/// 远程通知页的表单状态(挂在 [`SettingsView`] 上)。
pub(super) struct RemoteNotifyForm {
    bark_server: Entity<InputState>,
    bark_key: Entity<InputState>,
    tg_token: Entity<InputState>,
    tg_chat: Entity<InputState>,
    tg_api: Entity<InputState>,
    hook_url: Entity<InputState>,
    hook_body: Entity<InputState>,
    /// 打开面板时解不开的凭据(换了机器 / `credential.key` 丢了)。框里回填为空,
    /// 框下红字提示重填;用户重新提交成功后摘掉。
    unreadable: HashSet<PushSecret>,
    /// 凭据封存失败的提示(本次没存)。下次提交成功清掉。
    notice: Option<String>,
    tests: HashMap<Channel, TestState>,
    /// 每个渠道的测试任务各持一份:换一次就丢掉上一次(丢 = 取消)。
    jobs: HashMap<Channel, Task<()>>,
}

impl RemoteNotifyForm {
    pub(super) fn new(
        config: &RemoteNotifyConfig,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let mut unreadable = HashSet::new();
        let mut secret =
            |which: PushSecret| match crate::secrets::reveal_secret(which.stored(config)) {
                Ok(plain) => plain,
                Err(err) => {
                    eprintln!("[settings] 远程通知已存凭据无法解密,回填为空: {err}");
                    unreadable.insert(which);
                    String::new()
                }
            };
        let bark_key = secret(PushSecret::BarkKey);
        let tg_token = secret(PushSecret::TelegramToken);
        let hook_url = secret(PushSecret::WebhookUrl);

        let mut field = |value: String, placeholder: &str, masked: bool| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(masked)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        Self {
            bark_server: field(config.bark.server.clone(), BARK_DEFAULT_SERVER, false),
            // 凭据框 masked:token 不该明文常驻在屏幕上(与 SSH 密码 / 中转密钥同)
            bark_key: field(
                bark_key,
                t("settings", "remoteNotify.bark.keyPlaceholder"),
                true,
            ),
            tg_token: field(tg_token, "123456789:AAF…", true),
            tg_chat: field(
                config.telegram.chat_id.clone(),
                t("settings", "remoteNotify.telegram.chatPlaceholder"),
                false,
            ),
            tg_api: field(
                config.telegram.api_base.clone(),
                TELEGRAM_DEFAULT_API,
                false,
            ),
            // Webhook URL 不 masked:要能看清改错的那一位;库里照样是信封
            hook_url: field(hook_url, "https://", false),
            hook_body: field(
                config.webhook.body.clone(),
                t("settings", "remoteNotify.webhook.bodyPlaceholder"),
                false,
            ),
            unreadable,
            notice: None,
            tests: HashMap::new(),
            jobs: HashMap::new(),
        }
    }

    fn input(&self, field: RemoteField) -> &Entity<InputState> {
        match field {
            RemoteField::BarkServer => &self.bark_server,
            RemoteField::BarkKey => &self.bark_key,
            RemoteField::TgToken => &self.tg_token,
            RemoteField::TgChat => &self.tg_chat,
            RemoteField::TgApi => &self.tg_api,
            RemoteField::HookUrl => &self.hook_url,
            RemoteField::HookBody => &self.hook_body,
        }
    }
}

/// 服务器地址框的归一:去首尾空白与尾部斜杠;填的就是官方地址时存空(= 缺省)。
fn normalize_base(draft: &str, default: &str) -> String {
    match draft.trim().trim_end_matches('/') {
        base if base == default => String::new(),
        base => base.to_string(),
    }
}

impl SettingsView {
    // ── 提交 ──

    /// 七个输入框的「失焦 / 回车即提交」订阅(面板打开时一次)。
    pub(super) fn subscribe_remote_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in RemoteField::ALL {
            let input = self.remote.input(field).clone();
            self._subs.push(cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, _input, event: &InputEvent, window, cx| match event {
                    // 凭据框的红字在用户一开始输入时就换回常规提示
                    InputEvent::Change if field.secret().is_some() => cx.notify(),
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        this.commit_remote_field(field, window, cx)
                    }
                    _ => {}
                },
            ));
        }
    }

    fn commit_remote_field(
        &mut self,
        field: RemoteField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (field, value) in self.save_remote_field(field, cx) {
            let input = self.remote.input(field).clone();
            if input.read(cx).value().as_ref() != value.as_str() {
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
        cx.notify();
    }

    /// 归一并写进配置,返回要写回输入框的内容。不碰输入框,所以没有 `Window` 也能调。
    fn save_remote_field(
        &mut self,
        field: RemoteField,
        cx: &mut App,
    ) -> Vec<(RemoteField, String)> {
        let draft = self.remote.input(field).read(cx).value().to_string();
        let Some(secret) = field.secret() else {
            let value = match field {
                RemoteField::BarkServer => normalize_base(&draft, BARK_DEFAULT_SERVER),
                RemoteField::TgApi => normalize_base(&draft, TELEGRAM_DEFAULT_API),
                _ => draft.trim().to_string(),
            };
            let stored = value.clone();
            self.store.update(cx, |store, cx| {
                store.patch_remote_notify(
                    |c| match field {
                        RemoteField::BarkServer => c.bark.server = stored,
                        RemoteField::TgChat => c.telegram.chat_id = stored,
                        RemoteField::TgApi => c.telegram.api_base = stored,
                        RemoteField::HookBody => c.webhook.body = stored,
                        _ => {}
                    },
                    cx,
                )
            });
            return vec![(field, value)];
        };

        let mut writes = Vec::new();
        let mut plain = draft.trim().to_string();
        // Bark App 里复制的是整条推送地址:拆成服务器 + key,服务器框一并改
        if field == RemoteField::BarkKey
            && let Some((server, key)) = remote_notify::split_bark_url(&plain)
        {
            plain = key;
            let server = normalize_base(&server, BARK_DEFAULT_SERVER);
            writes.push((RemoteField::BarkServer, server.clone()));
            self.store.update(cx, |store, cx| {
                store.patch_remote_notify(|c| c.bark.server = server, cx)
            });
        }
        writes.push((field, plain.clone()));
        // 解不开的旧凭据:框是空的,用户只是点进来又点走 —— 别把它当「清空」提交,
        // 红字留着等用户真重填
        if plain.is_empty() && self.remote.unreadable.contains(&secret) {
            return writes;
        }
        let result = self.store.update(cx, |store, cx| {
            store.set_remote_notify_secret(secret, &plain, cx)
        });
        match result {
            Ok(()) => {
                self.remote.unreadable.remove(&secret);
                self.remote.notice = None;
            }
            Err(err) => {
                eprintln!("[settings] 远程通知凭据加密失败,本次未保存: {err}");
                self.remote.notice =
                    Some(tr!("settings", "remoteNotify.err.sealFailed", detail = err));
            }
        }
        writes
    }

    /// 关面板兜底提交全部草稿。顺序沿用 [`RemoteField::ALL`]:这里写不回输入框,key 框里
    /// 贴的整条推送地址要盖过服务器框,服务器框就得先提交。
    pub(super) fn commit_remote_drafts(&mut self, cx: &mut App) {
        for field in RemoteField::ALL {
            if !self.remote_secret_unchanged(field, cx) {
                self.save_remote_field(field, cx);
            }
        }
    }

    /// 凭据框的内容与已存的一致。没改就别过封存:封存失败会把已存的清空,
    /// 不能让「只是关个面板」碰上这条路。
    fn remote_secret_unchanged(&self, field: RemoteField, cx: &App) -> bool {
        let Some(secret) = field.secret() else {
            return false;
        };
        let stored = secret.stored(&self.store.read(cx).config().remote_notify);
        let draft = self.remote.input(field).read(cx).value();
        crate::secrets::reveal_secret(stored).is_ok_and(|plain| plain == draft.trim())
    }

    /// 把一个渠道的草稿全部提交(测试发送之前)。
    fn commit_remote_channel(
        &mut self,
        channel: Channel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for field in RemoteField::ALL {
            if field.channel() == channel {
                self.commit_remote_field(field, window, cx);
            }
        }
    }

    fn toggle_remote_channel(&mut self, channel: Channel, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.patch_remote_notify(
                |c| {
                    let next = !channel.enabled(c);
                    channel.set_enabled(c, next);
                },
                cx,
            )
        });
        cx.notify();
    }

    fn run_remote_test(&mut self, channel: Channel, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.remote.tests.get(&channel), Some(TestState::Sending)) {
            return;
        }
        self.commit_remote_channel(channel, window, cx);
        let (config, message) = {
            let store = self.store.read(cx);
            (
                store.config().remote_notify.clone(),
                remote_notify::test_message(channel, store.mobile_pwa_link(None)),
            )
        };
        self.remote.tests.insert(channel, TestState::Sending);
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { remote_notify::send_one(channel, &config, &message) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let state = match result {
                    Ok(()) => TestState::Ok,
                    Err(err) => TestState::Failed(err),
                };
                this.remote.tests.insert(channel, state);
                cx.notify();
            });
        });
        self.remote.jobs.insert(channel, task);
        cx.notify();
    }

    // ── 渲染 ──

    pub(super) fn render_remote_notify_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (config, pwa) = {
            let store = self.store.read(cx);
            (
                store.config().remote_notify.clone(),
                store.mobile_pwa_link(None),
            )
        };
        let enabled = config.enabled;

        let mut root = page_root().child(
            section("remoteNotify.title")
                .child(toggle_row(
                    "remote-notify-enable",
                    "remoteNotify.enable",
                    "remoteNotify.enableDesc",
                    enabled,
                    false,
                    |this, next, _window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.patch_remote_notify(|c| c.enabled = next, cx)
                        });
                    },
                    cx,
                ))
                .when(!enabled, |el| {
                    el.child(ui::hint(t("settings", "remoteNotify.footer")))
                }),
        );
        if !enabled {
            return root.into_any_element();
        }

        let picked: Vec<Channel> = Channel::ALL
            .into_iter()
            .filter(|c| c.enabled(&config))
            .collect();
        root = root.child(
            section("remoteNotify.channels")
                .child(self.render_channel_picker(&config, cx))
                .when(picked.is_empty(), |el| {
                    el.child(
                        ui::hint(t("settings", "remoteNotify.noChannel"))
                            .text_color(ui::color_warning()),
                    )
                }),
        );
        for channel in picked {
            root = root.child(self.render_channel_form(channel, cx));
        }
        if let Some(notice) = self.remote.notice.clone() {
            root = root.child(banner(notice, ui::color_error()));
        }
        root.child(
            section("remoteNotify.mobileLink")
                .child(
                    ui::settings_card().child(match &pwa {
                        Some(url) => div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(t("settings", "remoteNotify.mobileLinkOn")),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::accent())
                                    .child(url.clone()),
                            )
                            .child(ui::desc_text(t(
                                "settings",
                                "remoteNotify.mobileLinkOnDesc",
                            ))),
                        None => div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(t("settings", "remoteNotify.mobileLinkOff")),
                            )
                            .child(ui::desc_text(t(
                                "settings",
                                "remoteNotify.mobileLinkOffDesc",
                            ))),
                    }),
                )
                .child(ui::hint(t("settings", "remoteNotify.footer"))),
        )
        .into_any_element()
    }

    /// 渠道多选(与 hook 页「注入目标」同一种勾选行)。
    fn render_channel_picker(
        &self,
        config: &RemoteNotifyConfig,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut card = ui::settings_card().p_0().flex().flex_col();
        for channel in Channel::ALL {
            let checked = channel.enabled(config);
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "remote-channel-{}",
                        channel.key()
                    )))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::border_subtle()))
                    .child(ui::checkbox(
                        SharedString::from(format!("remote-check-{}", channel.key())),
                        checked,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(channel.label()),
                            )
                            .child(
                                div()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(t("settings", channel_desc_key(channel))),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_remote_channel(channel, cx)
                    })),
            );
        }
        card
    }

    /// 一个渠道的表单卡片:标题行(名字 + 测试按钮 + 结果)+ 各字段。
    fn render_channel_form(&self, channel: Channel, cx: &mut Context<Self>) -> gpui::Div {
        let test = self.remote.tests.get(&channel).cloned();
        let sending = matches!(test, Some(TestState::Sending));
        let status = match &test {
            None => None,
            Some(TestState::Sending) => Some((
                t("settings", "remoteNotify.testSending").to_string(),
                ui::text_muted(),
            )),
            Some(TestState::Ok) => Some((
                t("settings", "remoteNotify.testOk").to_string(),
                ui::color_success(),
            )),
            Some(TestState::Failed(err)) => Some((
                tr!("settings", "remoteNotify.testFailed", detail = err),
                ui::color_error(),
            )),
        };

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(ui::font_px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(ui::text_primary())
                    .child(channel.label()),
            )
            .child(
                ui::ghost_button(
                    SharedString::from(format!("remote-test-{}", channel.key())),
                    t("settings", "remoteNotify.test"),
                )
                .when(sending, |el| el.opacity(0.5))
                .when(!sending, |el| {
                    el.on_click(cx.listener(move |this, _, window, cx| {
                        this.run_remote_test(channel, window, cx)
                    }))
                }),
            );

        let mut card = ui::settings_card()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(header);
        match channel {
            Channel::Bark => {
                card = card
                    .child(self.text_field(
                        RemoteField::BarkKey,
                        "remoteNotify.bark.key",
                        Some("remoteNotify.bark.keyHint"),
                        cx,
                    ))
                    .child(self.text_field(
                        RemoteField::BarkServer,
                        "remoteNotify.bark.server",
                        Some("remoteNotify.bark.serverHint"),
                        cx,
                    ));
            }
            Channel::Telegram => {
                card = card
                    .child(self.text_field(
                        RemoteField::TgToken,
                        "remoteNotify.telegram.token",
                        Some("remoteNotify.telegram.tokenHint"),
                        cx,
                    ))
                    .child(self.text_field(
                        RemoteField::TgChat,
                        "remoteNotify.telegram.chat",
                        Some("remoteNotify.telegram.chatHint"),
                        cx,
                    ))
                    .child(self.text_field(
                        RemoteField::TgApi,
                        "remoteNotify.telegram.api",
                        Some("remoteNotify.telegram.apiHint"),
                        cx,
                    ));
            }
            Channel::Webhook => {
                let placeholders = WEBHOOK_PLACEHOLDERS.join(" ");
                card = card
                    .child(self.text_field(
                        RemoteField::HookUrl,
                        "remoteNotify.webhook.url",
                        Some("remoteNotify.webhook.urlHint"),
                        cx,
                    ))
                    .child(self.text_field(
                        RemoteField::HookBody,
                        "remoteNotify.webhook.body",
                        None,
                        cx,
                    ))
                    .child(ui::desc_text(tr!(
                        "settings",
                        "remoteNotify.webhook.bodyHint",
                        placeholders = placeholders
                    )));
            }
        }
        card.when_some(status, |el, (text, color)| {
            el.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(color)
                    .child(text),
            )
        })
    }

    /// 上标签、下整宽输入框、再下一行说明(凭据解不开时换成红字)。
    fn text_field(
        &self,
        field: RemoteField,
        label_key: &'static str,
        hint_key: Option<&'static str>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let input = self.remote.input(field);
        // 弹窗每帧重建,读输入框的值是活的:一开始输入红字就换回常规提示
        let unreadable = field
            .secret()
            .is_some_and(|s| self.remote.unreadable.contains(&s))
            && input.read(cx).value().is_empty();
        let hint = if unreadable {
            Some((
                t("settings", "remoteNotify.err.secretUnreadable"),
                ui::color_error(),
            ))
        } else {
            hint_key.map(|key| (t("settings", key), ui::text_muted()))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .child(t("settings", label_key)),
            )
            .child(Input::new(input))
            .when_some(hint, |el, (text, color)| {
                el.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(color)
                        .child(text),
                )
            })
    }
}

fn channel_desc_key(channel: Channel) -> &'static str {
    match channel {
        Channel::Bark => "remoteNotify.bark.desc",
        Channel::Telegram => "remoteNotify.telegram.desc",
        Channel::Webhook => "remoteNotify.webhook.desc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 服务器地址归一_官方地址存空() {
        assert_eq!(
            normalize_base(" https://api.day.app/ ", BARK_DEFAULT_SERVER),
            ""
        );
        assert_eq!(normalize_base("", BARK_DEFAULT_SERVER), "");
        assert_eq!(
            normalize_base("https://bark.example.com//", BARK_DEFAULT_SERVER),
            "https://bark.example.com"
        );
        assert_eq!(
            normalize_base("https://api.telegram.org", TELEGRAM_DEFAULT_API),
            ""
        );
    }

    /// 关面板兜底按 [`RemoteField::ALL`] 提交:key 框贴的整条推送地址要盖过服务器框。
    #[test]
    fn 服务器框排在_key_框之前提交() {
        let at = |field| RemoteField::ALL.iter().position(|f| *f == field);
        assert!(at(RemoteField::BarkServer) < at(RemoteField::BarkKey));
    }

    #[test]
    fn 每个字段归属一个渠道且凭据只有三处() {
        let secrets: Vec<_> = RemoteField::ALL.iter().filter_map(|f| f.secret()).collect();
        assert_eq!(
            secrets,
            vec![
                PushSecret::BarkKey,
                PushSecret::TelegramToken,
                PushSecret::WebhookUrl
            ]
        );
        for channel in Channel::ALL {
            assert!(RemoteField::ALL.iter().any(|f| f.channel() == channel));
        }
    }
}
