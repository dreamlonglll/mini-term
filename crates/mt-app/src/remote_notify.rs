//! 远程通知(issue #87):AI 任务完成 / 停下来等你处理时,经 Bark / Telegram /
//! 通用 Webhook 推到手机。
//!
//! # 链路
//!
//! ```text
//! notify::DoneTracker::apply ──AlertPlan.remote──▶ AppStore::apply_ai_event
//!   (判据与本地提醒同源)                             拼好 RemoteAlert(配置快照 + 文案 + 手机端链接)
//!        ──▶ Workspace::deliver_alert ──background──▶ deliver(逐渠道 POST)
//! ```
//!
//! - **判据不在这里**:完成的下降沿 / 待确认的上升沿、重复 Stop 去重、降级路径的回合闩
//!   全在 `notify.rs`,远程与本地吃同一份。这里只管「推给谁、怎么推」;
//! - **凭据发送那一刻才解开**(配置里是 `mt-secret` 信封,见 [`crate::secrets`]);
//!   解不开的渠道报「请重新填写」,绝不把信封当 token 送出去;
//! - **内容只有项目名 / AI 名 / 终端名 / 待处理原因**,不含对话正文 —— 消息要经过
//!   第三方服务器(Bark 走 APNs,Telegram 走它自己的服务器);
//! - **配了移动端中转就附手机端链接** `{PWA}/#pane=<pane id>`:点开通知直接进这个会话的
//!   对话镜像(PWA 侧见 `mobile/src/relay.ts` 的 `#pane=` 深链);
//! - **失败只记日志**,不弹 toast —— 人不在电脑前,弹了也没人看。设置页的「测试发送」
//!   把结果就地显示;
//! - **错误串里不许出现凭据**:reqwest 的错误自带完整 URL(Telegram 的 token 就在路径里、
//!   Webhook 的 key 常在查询串里),一律 `without_url()`。

use std::sync::OnceLock;
use std::time::Duration;

use mt_config::RemoteNotifyConfig;
use serde_json::{Value, json};

use crate::i18n::{t, tr};
use crate::notify::ToastKind;

/// 官方 Bark 服务器(`bark.server` 留空时用)。
pub const BARK_DEFAULT_SERVER: &str = "https://api.day.app";
/// 官方 Telegram Bot API(`telegram.apiBase` 留空时用)。
pub const TELEGRAM_DEFAULT_API: &str = "https://api.telegram.org";
/// 单次请求的总超时。`reqwest::blocking` 默认无限等,而一条推送卡住不该拖住后面的渠道。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 错误信息里引用响应体的上限(字符)。
const ERROR_BODY_CHARS: usize = 200;

/// 推送渠道。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Bark,
    Telegram,
    Webhook,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Bark, Channel::Telegram, Channel::Webhook];

    /// 展示名(各家的品牌名,不走 i18n)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Bark => "Bark",
            Self::Telegram => "Telegram",
            Self::Webhook => "Webhook",
        }
    }

    /// 稳定 id(元素 id 用)。
    pub fn key(self) -> &'static str {
        match self {
            Self::Bark => "bark",
            Self::Telegram => "telegram",
            Self::Webhook => "webhook",
        }
    }

    pub fn enabled(self, config: &RemoteNotifyConfig) -> bool {
        match self {
            Self::Bark => config.bark.enabled,
            Self::Telegram => config.telegram.enabled,
            Self::Webhook => config.webhook.enabled,
        }
    }

    pub fn set_enabled(self, config: &mut RemoteNotifyConfig, enabled: bool) {
        match self {
            Self::Bark => config.bark.enabled = enabled,
            Self::Telegram => config.telegram.enabled = enabled,
            Self::Webhook => config.webhook.enabled = enabled,
        }
    }
}

/// 三处凭据(配置里是信封,写入走 [`crate::store::AppStore::set_remote_notify_secret`])。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PushSecret {
    BarkKey,
    TelegramToken,
    WebhookUrl,
}

impl PushSecret {
    pub fn slot(self, config: &mut RemoteNotifyConfig) -> &mut String {
        match self {
            Self::BarkKey => &mut config.bark.device_key,
            Self::TelegramToken => &mut config.telegram.bot_token,
            Self::WebhookUrl => &mut config.webhook.url,
        }
    }

    pub fn stored(self, config: &RemoteNotifyConfig) -> &str {
        match self {
            Self::BarkKey => &config.bark.device_key,
            Self::TelegramToken => &config.telegram.bot_token,
            Self::WebhookUrl => &config.webhook.url,
        }
    }
}

/// 一条要推出去的消息(各渠道共用)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMessage {
    pub title: String,
    pub body: String,
    /// `"completion"` / `"attention"` / `"test"`(Webhook 模板的 `{{event}}`)。
    pub event: &'static str,
    pub project: String,
    pub agent: String,
    pub pane: String,
    /// 手机端链接;没配移动端中转时 `None`。
    pub url: Option<String>,
}

/// 一次 AI 提醒要推的东西:配置快照 + 消息。由 store 拼好,交给持有 executor 的
/// `Workspace::deliver_alert` 丢后台发。
pub struct RemoteAlert {
    pub config: RemoteNotifyConfig,
    pub message: RemoteMessage,
}

// ─── 文案 ─────────────────────────────────────────────────────

/// agent 标识 → 展示名(`claude` → `Claude Code`)。认不出原样用,没有就叫「AI」。
fn agent_label(agent: Option<&str>) -> String {
    let raw = agent.map(str::trim).filter(|a| !a.is_empty());
    match raw {
        Some(raw) => mt_ai::AgentKind::parse(raw)
            .map(|kind| kind.spec().label.to_string())
            .unwrap_or_else(|| raw.to_string()),
        None => "AI".to_string(),
    }
}

/// 待确认的原因(按 hook 事件名,判据表与 `mt_ai::is_attention_cause` 同集)。
fn attention_reason(cause: Option<&str>) -> &'static str {
    match cause {
        Some("PermissionRequest") => t("settings", "remoteNotify.msg.reasonPermission"),
        Some("Elicitation") => t("settings", "remoteNotify.msg.reasonElicitation"),
        Some("StopFailure") => t("settings", "remoteNotify.msg.reasonStopFailure"),
        _ => t("settings", "remoteNotify.msg.reasonGeneric"),
    }
}

/// 拼一条 AI 提醒。`kind` 只会是 Completion / Attention(见 `AlertPlan::remote`)。
pub fn compose(
    kind: ToastKind,
    cause: Option<&str>,
    project: &str,
    agent: Option<&str>,
    pane: &str,
    url: Option<String>,
) -> RemoteMessage {
    let agent = agent_label(agent);
    let terminal = tr!("settings", "remoteNotify.msg.terminal", pane = pane);
    let (event, title, body) = if kind == ToastKind::Attention {
        (
            "attention",
            tr!(
                "settings",
                "remoteNotify.msg.attentionTitle",
                project = project,
                agent = agent
            ),
            format!("{} · {terminal}", attention_reason(cause)),
        )
    } else {
        (
            "completion",
            tr!(
                "settings",
                "remoteNotify.msg.completionTitle",
                project = project,
                agent = agent
            ),
            terminal,
        )
    };
    RemoteMessage {
        title,
        body,
        event,
        project: project.to_string(),
        agent,
        pane: pane.to_string(),
        url,
    }
}

/// 设置页「测试发送」的那一条。
pub fn test_message(channel: Channel, url: Option<String>) -> RemoteMessage {
    RemoteMessage {
        title: t("settings", "remoteNotify.msg.testTitle").to_string(),
        body: tr!(
            "settings",
            "remoteNotify.msg.testBody",
            channel = channel.label()
        ),
        event: "test",
        project: "Mini-Term".to_string(),
        agent: "Mini-Term".to_string(),
        pane: String::new(),
        url,
    }
}

// ─── 请求构造(纯函数,可测) ─────────────────────────────────

/// 一次 POST(正文一律 JSON)。
#[derive(Debug, PartialEq, Eq)]
pub struct JsonPost {
    pub url: String,
    pub body: String,
}

/// 服务器地址的归一:去首尾空白与尾部斜杠,空 = 用缺省值。
fn base_or(input: &str, default: &str) -> String {
    match input.trim().trim_end_matches('/') {
        "" => default.to_string(),
        base => base.to_string(),
    }
}

/// 用户把 Bark App 里复制的整条推送地址(`https://api.day.app/<key>/推送内容`)粘进了
/// key 框:拆成 `(服务器, key)`。不是 http(s) 地址返回 `None`。
///
/// key 取路径的**第一段** —— Bark App 给出的地址都是这个形状;自建服务器挂在子路径下
/// 的(`https://x.com/bark/<key>`)拆不准,那种情况请分开填服务器与 key。
pub fn split_bark_url(input: &str) -> Option<(String, String)> {
    let input = input.trim();
    let rest = input
        .strip_prefix("https://")
        .map(|r| ("https://", r))
        .or_else(|| input.strip_prefix("http://").map(|r| ("http://", r)))?;
    let (scheme, rest) = rest;
    let (host, path) = rest.split_once('/')?;
    // 查询串 / 片段不属于 key(`/KEY?sound=x`)
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let key = path.split('/').find(|seg| !seg.is_empty())?;
    if host.is_empty() {
        return None;
    }
    Some((format!("{scheme}{host}"), key.to_string()))
}

/// Bark:`POST {server}/push`,JSON 带 `device_key`(bark-server v2 接口)。待确认走
/// `timeSensitive`(专注模式下也弹),完成走 `active`;有手机端链接就放进 `url`,
/// 点通知直接打开。
pub fn bark_request(
    server: &str,
    device_key: &str,
    msg: &RemoteMessage,
) -> Result<JsonPost, String> {
    let key = device_key.trim();
    if key.is_empty() {
        return Err(t("settings", "remoteNotify.err.barkKeyMissing").to_string());
    }
    let mut body = json!({
        "device_key": key,
        "title": msg.title,
        "body": msg.body,
        "group": "Mini-Term",
        "level": if msg.event == "attention" { "timeSensitive" } else { "active" },
    });
    if let Some(url) = &msg.url {
        body["url"] = Value::String(url.clone());
    }
    Ok(JsonPost {
        url: format!("{}/push", base_or(server, BARK_DEFAULT_SERVER)),
        body: body.to_string(),
    })
}

/// Telegram:`POST {api}/bot{token}/sendMessage`。纯文本(不开 parse_mode —— 项目名里的
/// `_` `*` 在 Markdown 模式下会让整条消息被拒);手机端链接放末行,Telegram 自动识别成
/// 可点的链接。不用 inline 按钮:局域网地址 / 裸 IP 的按钮 URL 会被 Bot API 拒掉,
/// 连带整条消息发不出去。
pub fn telegram_request(
    api_base: &str,
    bot_token: &str,
    chat_id: &str,
    msg: &RemoteMessage,
) -> Result<JsonPost, String> {
    let token = bot_token.trim();
    if token.is_empty() {
        return Err(t("settings", "remoteNotify.err.tgTokenMissing").to_string());
    }
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return Err(t("settings", "remoteNotify.err.tgChatMissing").to_string());
    }
    let mut text = format!("{}\n{}", msg.title, msg.body);
    if let Some(url) = &msg.url {
        text.push('\n');
        text.push_str(url);
    }
    let body = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    Ok(JsonPost {
        url: format!(
            "{}/bot{token}/sendMessage",
            base_or(api_base, TELEGRAM_DEFAULT_API)
        ),
        body: body.to_string(),
    })
}

/// Webhook 模板认的占位符(设置页提示与 [`render_webhook_body`] 共用这一张表)。
pub const WEBHOOK_PLACEHOLDERS: [&str; 7] = [
    "{{title}}",
    "{{body}}",
    "{{event}}",
    "{{project}}",
    "{{agent}}",
    "{{pane}}",
    "{{url}}",
];

/// JSON 字符串**内容**的转义(不带两侧引号)—— 占位符写在模板的引号里面。
fn json_escape(value: &str) -> String {
    let quoted = Value::String(value.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// 模板为空 → 默认 JSON;否则逐个替换占位符(值按 JSON 字符串转义),替换完必须是
/// 合法 JSON —— 模板写坏了宁可报错,也不往别人的接口里塞半截文本。
pub fn render_webhook_body(template: &str, msg: &RemoteMessage) -> Result<String, String> {
    if template.trim().is_empty() {
        return Ok(json!({
            "title": msg.title,
            "body": msg.body,
            "event": msg.event,
            "project": msg.project,
            "agent": msg.agent,
            "pane": msg.pane,
            "url": msg.url,
            "source": "mini-term",
        })
        .to_string());
    }
    let url = msg.url.clone().unwrap_or_default();
    let values = [
        msg.title.as_str(),
        msg.body.as_str(),
        msg.event,
        msg.project.as_str(),
        msg.agent.as_str(),
        msg.pane.as_str(),
        url.as_str(),
    ];
    let mut out = template.to_string();
    for (placeholder, value) in WEBHOOK_PLACEHOLDERS.iter().zip(values) {
        out = out.replace(placeholder, &json_escape(value));
    }
    serde_json::from_str::<Value>(&out)
        .map_err(|e| tr!("settings", "remoteNotify.err.webhookTemplate", detail = e))?;
    Ok(out)
}

pub fn webhook_request(url: &str, template: &str, msg: &RemoteMessage) -> Result<JsonPost, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(t("settings", "remoteNotify.err.webhookUrlMissing").to_string());
    }
    Ok(JsonPost {
        url: url.to_string(),
        body: render_webhook_body(template, msg)?,
    })
}

// ─── 响应判定(纯函数,可测) ─────────────────────────────────

/// 截一段响应体放进错误信息(按字符截,不切坏多字节字符)。
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    let mut out: String = trimmed.chars().take(ERROR_BODY_CHARS).collect();
    if trimmed.chars().count() > ERROR_BODY_CHARS {
        out.push('…');
    }
    out
}

/// 非 2xx 的统一说法。
fn http_error(status: u16, body: &str) -> String {
    match snippet(body) {
        s if s.is_empty() => format!("HTTP {status}"),
        s => format!("HTTP {status}: {s}"),
    }
}

/// 响应体里取一个字符串字段(取不到就空)。
fn field<'a>(json: &'a Value, key: &str) -> &'a str {
    json.get(key).and_then(Value::as_str).unwrap_or("")
}

/// 各家的成败判据。
///
/// - Bark:`{"code":200,"message":"success"}`,`code` 不是 200 即失败;
/// - Telegram:`{"ok":false,"description":"Bad Request: chat not found"}`(出错时 HTTP 也是
///   4xx,但 description 比状态码有用,先看它);
/// - Webhook:2xx 即成功,另认两种「HTTP 200 但业务失败」的常见形状 —— `errcode` 非 0
///   (企业微信 / 钉钉机器人)、`code` 既不是 0 也不是 200(飞书 / Server酱 / Bark 自身)。
///   不是 JSON 的响应(Discord 的 204、ntfy 以外的纯文本)只看状态码。
pub fn judge_response(channel: Channel, status: u16, body: &str) -> Result<(), String> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    match channel {
        Channel::Telegram => {
            if let Some(json) = &json
                && json.get("ok").and_then(Value::as_bool) == Some(false)
            {
                return Err(match field(json, "description") {
                    "" => http_error(status, body),
                    desc => desc.to_string(),
                });
            }
        }
        Channel::Bark => {
            if let Some(json) = &json
                && let Some(code) = json.get("code").and_then(Value::as_i64)
                && code != 200
            {
                return Err(match field(json, "message") {
                    "" => format!("code {code}"),
                    message => format!("code {code}: {message}"),
                });
            }
        }
        Channel::Webhook => {
            if let Some(json) = &json {
                if let Some(code) = json.get("errcode").and_then(Value::as_i64)
                    && code != 0
                {
                    return Err(format!("errcode {code}: {}", field(json, "errmsg")));
                }
                if let Some(code) = json.get("code").and_then(Value::as_i64)
                    && code != 0
                    && code != 200
                {
                    let message = match field(json, "msg") {
                        "" => field(json, "message"),
                        msg => msg,
                    };
                    return Err(format!("code {code}: {message}"));
                }
            }
        }
    }
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(http_error(status, body))
    }
}

// ─── 发送(阻塞,一律丢后台) ─────────────────────────────────

fn client() -> Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    // 默认会读系统代理(Windows 注册表 / HTTPS_PROXY):国内连 Telegram 靠它
    let built = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.without_url().to_string())?;
    Ok(CLIENT.get_or_init(|| built))
}

/// 一次 POST,返回 `(状态码, 响应体)`。**阻塞**。
fn post(req: &JsonPost) -> Result<(u16, String), String> {
    let resp = client()?
        .post(&req.url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(req.body.clone())
        .send()
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status().as_u16();
    // 响应体读不出来不算失败:成败主要看状态码
    let body = resp.text().unwrap_or_default();
    Ok((status, body))
}

/// 已存凭据 → 明文。解不开给可展示的原因(「请重新填写」),不把信封交出去。
fn reveal(stored: &str) -> Result<String, String> {
    crate::secrets::reveal_secret(stored)
        .map_err(|_| t("settings", "remoteNotify.err.secretUnreadable").to_string())
}

/// 按某个渠道的配置构造请求(凭据此时才解开)。
fn build_request(
    channel: Channel,
    config: &RemoteNotifyConfig,
    msg: &RemoteMessage,
) -> Result<JsonPost, String> {
    match channel {
        Channel::Bark => {
            let key = reveal(&config.bark.device_key)?;
            bark_request(&config.bark.server, &key, msg)
        }
        Channel::Telegram => {
            let token = reveal(&config.telegram.bot_token)?;
            telegram_request(
                &config.telegram.api_base,
                &token,
                &config.telegram.chat_id,
                msg,
            )
        }
        Channel::Webhook => {
            let url = reveal(&config.webhook.url)?;
            webhook_request(&url, &config.webhook.body, msg)
        }
    }
}

/// 经一个渠道发一条(不看它勾没勾 —— 测试发送也走这里)。**阻塞**。
pub fn send_one(
    channel: Channel,
    config: &RemoteNotifyConfig,
    msg: &RemoteMessage,
) -> Result<(), String> {
    let req = build_request(channel, config, msg)?;
    let (status, body) = post(&req)?;
    judge_response(channel, status, &body)
}

/// 一次 AI 提醒:逐个勾选的渠道发,失败只记日志(不含凭据)。**阻塞**,调用方丢后台。
pub fn deliver(alert: RemoteAlert) {
    if !alert.config.is_active() {
        return;
    }
    for channel in Channel::ALL {
        if !channel.enabled(&alert.config) {
            continue;
        }
        if let Err(err) = send_one(channel, &alert.config, &alert.message) {
            eprintln!("[remote-notify] {} 推送失败: {err}", channel.label());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(event: &'static str, url: Option<&str>) -> RemoteMessage {
        RemoteMessage {
            title: "proj · Claude Code 任务完成".into(),
            body: "终端「pwsh」".into(),
            event,
            project: "proj".into(),
            agent: "Claude Code".into(),
            pane: "pwsh".into(),
            url: url.map(str::to_string),
        }
    }

    fn body_of(req: &JsonPost) -> Value {
        serde_json::from_str(&req.body).unwrap()
    }

    #[test]
    fn bark_缺省服务器与等级和链接() {
        let req = bark_request("", " k3y ", &msg("completion", None)).unwrap();
        assert_eq!(req.url, "https://api.day.app/push");
        let body = body_of(&req);
        assert_eq!(body["device_key"], "k3y");
        assert_eq!(body["level"], "active");
        assert!(body.get("url").is_none(), "没配中转就不带链接");

        let req = bark_request(
            "https://bark.example.com/",
            "k3y",
            &msg("attention", Some("https://relay.example.com/#pane=pane-1")),
        )
        .unwrap();
        assert_eq!(req.url, "https://bark.example.com/push", "尾斜杠去掉");
        let body = body_of(&req);
        assert_eq!(body["level"], "timeSensitive", "待确认在专注模式下也要弹");
        assert_eq!(body["url"], "https://relay.example.com/#pane=pane-1");
        assert_eq!(body["group"], "Mini-Term");

        assert!(bark_request("", "  ", &msg("completion", None)).is_err());
    }

    #[test]
    fn bark_整条推送地址拆成服务器与_key() {
        assert_eq!(
            split_bark_url("https://api.day.app/AbC123/这里改成你自己的推送内容"),
            Some(("https://api.day.app".into(), "AbC123".into()))
        );
        assert_eq!(
            split_bark_url(" http://192.168.1.5:8080/AbC123/ "),
            Some(("http://192.168.1.5:8080".into(), "AbC123".into()))
        );
        assert_eq!(split_bark_url("AbC123"), None, "裸 key 不拆");
        assert_eq!(split_bark_url("https://api.day.app"), None, "没有 key 段");
        assert_eq!(split_bark_url("https://api.day.app/"), None);
        assert_eq!(
            split_bark_url("https://api.day.app/AbC123?sound=bell"),
            Some(("https://api.day.app".into(), "AbC123".into())),
            "查询串不属于 key"
        );
        assert_eq!(
            split_bark_url("https://api.day.app/AbC123#x"),
            Some(("https://api.day.app".into(), "AbC123".into()))
        );
    }

    #[test]
    fn telegram_纯文本且链接放末行() {
        let req = telegram_request(
            "",
            "123:abc",
            " 42 ",
            &msg("completion", Some("https://relay.example.com/#pane=p")),
        )
        .unwrap();
        assert_eq!(req.url, "https://api.telegram.org/bot123:abc/sendMessage");
        let body = body_of(&req);
        assert_eq!(body["chat_id"], "42");
        assert!(
            body.get("parse_mode").is_none(),
            "不开 Markdown,项目名里的 _ 不会让消息被拒"
        );
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with("proj · Claude Code 任务完成\n终端「pwsh」"));
        assert!(text.ends_with("\nhttps://relay.example.com/#pane=p"));

        let req =
            telegram_request("https://tg.example.com/", "t", "1", &msg("test", None)).unwrap();
        assert_eq!(req.url, "https://tg.example.com/bott/sendMessage");
        assert!(telegram_request("", "", "1", &msg("test", None)).is_err());
        assert!(telegram_request("", "t", " ", &msg("test", None)).is_err());
    }

    #[test]
    fn webhook_默认_json_带全部字段() {
        let body = render_webhook_body("  ", &msg("attention", None)).unwrap();
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["event"], "attention");
        assert_eq!(json["project"], "proj");
        assert_eq!(json["source"], "mini-term");
        assert!(json["url"].is_null());
    }

    /// 占位符的值按 JSON 字符串转义:项目名里的引号 / 换行不会把模板撑坏。
    #[test]
    fn webhook_模板替换并转义() {
        let mut m = msg("completion", Some("https://r.example.com/#pane=p"));
        m.project = "a\"b\nc".into();
        let template = r#"{"msgtype":"text","text":{"content":"{{title}}\n{{project}} {{url}}"}}"#;
        let body = render_webhook_body(template, &m).unwrap();
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json["text"]["content"],
            "proj · Claude Code 任务完成\na\"b\nc https://r.example.com/#pane=p"
        );
        // 没有链接时 {{url}} 替换成空串
        let body = render_webhook_body(r#"{"u":"{{url}}"}"#, &msg("test", None)).unwrap();
        assert_eq!(body, r#"{"u":""}"#);
    }

    #[test]
    fn webhook_模板写坏了报错而不是照发() {
        assert!(render_webhook_body("{\"title\": {{title}}}", &msg("test", None)).is_err());
        assert!(webhook_request(" ", "", &msg("test", None)).is_err());
        let req = webhook_request(" https://h.example.com/x ", "", &msg("test", None)).unwrap();
        assert_eq!(req.url, "https://h.example.com/x");
    }

    #[test]
    fn 响应判定按各家形状() {
        use Channel::*;
        assert!(judge_response(Bark, 200, r#"{"code":200,"message":"success"}"#).is_ok());
        assert_eq!(
            judge_response(
                Bark,
                400,
                r#"{"code":400,"message":"failed to get device token"}"#
            ),
            Err("code 400: failed to get device token".into())
        );
        assert!(judge_response(Telegram, 200, r#"{"ok":true,"result":{}}"#).is_ok());
        assert_eq!(
            judge_response(
                Telegram,
                400,
                r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#
            ),
            Err("Bad Request: chat not found".into())
        );
        // 企业微信:HTTP 200 但 errcode 非 0
        assert_eq!(
            judge_response(
                Webhook,
                200,
                r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#
            ),
            Err("errcode 93000: invalid webhook url".into())
        );
        assert!(judge_response(Webhook, 200, r#"{"errcode":0,"errmsg":"ok"}"#).is_ok());
        // 飞书:code 非 0
        assert!(judge_response(Webhook, 200, r#"{"code":19001,"msg":"param invalid"}"#).is_err());
        assert!(judge_response(Webhook, 200, r#"{"code":0,"msg":"success"}"#).is_ok());
        // Discord 204 无正文 / 纯文本
        assert!(judge_response(Webhook, 204, "").is_ok());
        assert_eq!(judge_response(Webhook, 500, ""), Err("HTTP 500".into()));
        assert_eq!(
            judge_response(Webhook, 404, "not found"),
            Err("HTTP 404: not found".into())
        );
    }

    #[test]
    fn 错误里引用的响应体按字符截断() {
        let long = "错".repeat(ERROR_BODY_CHARS + 10);
        let err = http_error(502, &long);
        assert!(err.ends_with('…'));
        assert_eq!(err.chars().filter(|c| *c == '错').count(), ERROR_BODY_CHARS);
    }

    #[test]
    fn agent_展示名() {
        assert_eq!(agent_label(Some("claude-code")), "Claude Code");
        assert_eq!(agent_label(Some("omp")), "oh-my-pi");
        assert_eq!(agent_label(Some("my-agent")), "my-agent", "认不出原样用");
        assert_eq!(agent_label(None), "AI");
        assert_eq!(agent_label(Some("  ")), "AI");
    }

    #[test]
    fn 待确认消息带原因与终端名() {
        let m = compose(
            ToastKind::Attention,
            Some("PermissionRequest"),
            "proj",
            Some("codex"),
            "pwsh",
            None,
        );
        assert_eq!(m.event, "attention");
        assert!(m.title.contains("proj") && m.title.contains("Codex"));
        assert!(m.body.contains(attention_reason(Some("PermissionRequest"))));
        assert!(m.body.contains("pwsh"));

        let m = compose(
            ToastKind::Completion,
            Some("Stop"),
            "proj",
            None,
            "zsh",
            None,
        );
        assert_eq!(m.event, "completion");
        assert!(m.title.contains("AI"));
    }

    /// 总开关关着 / 一个渠道都没勾:什么都不发(也就不会碰网络)。
    #[test]
    fn 未启用时不发() {
        let mut config = RemoteNotifyConfig::default();
        config.bark.enabled = true;
        deliver(RemoteAlert {
            config,
            message: msg("test", None),
        });
    }
}
