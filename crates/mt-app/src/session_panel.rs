//! AI 历史面板(右侧抽屉)。对应 `src/components/SessionList.tsx`。
//!
//! ```text
//! 项目切换 ─→ refresh(force=false)
//!              ├─ background: mt_ai::sessions::get_ai_sessions(宿主,秒出)
//!              └─ background: mt_ai::sessions::get_wsl_ai_sessions(9P + 可能的 VM 冷启动)
//!                     ↓ 各自回主线程 setState,按时间戳降序混排
//! 点一行 ─→ resume 命令写进当前 pane(走 TerminalPane::write,保住 AI 输入检测)
//! 点「查看」→ background: get_ai_session_content → 面板内正文预览
//! ```
//!
//! **两个慢函数必须丢后台**(看板技术债清单明示):`get_ai_session_content` 与
//! `get_wsl_ai_sessions` 原本是 `#[tauri::command(async)]`,靠命令层挪出主线程;
//! 现在是普通同步函数,WSL 冷启动秒级,落在 GPUI 主线程上就是整个窗口卡住。
//!
//! # 与旧版的偏差
//!
//! - 分支树视图(`sessionListView: 'tree'`)与 `scan_session_lineage` 没搬:那是
//!   PR #47 的整套 fork 谱系,数据面与渲染面都独立成篇,留给后续批次。
//! - SSH 远程来源(`ssh_remote_ai_sessions`)没搬:mt-ssh 还没进 crates/。
//! - 右键菜单换成行内按钮(gpui 侧还没有上下文菜单基建),四个动作里保留
//!   「在当前终端恢复」「新标签恢复」「查看」,「复制命令」并进查看页。

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder,
    px,
};
use mt_ai::sessions::{AiSession, AiSessionMessage};

use crate::store::AppStore;
use crate::ui;

/// 一页多少条(与旧版 `PAGE_SIZE` 同值)。
const PAGE_SIZE: usize = 20;

/// 该会话对应的 resume 命令;id 形态异常返回 `None`。
///
/// sessionId 会被原样拼进写进 PTY 的命令行,必须过白名单:字母数字与 `-_`
/// (Claude UUID、Codex rollout id 与 Grok UUIDv7 的实际形态)。两个来源
/// ——持久化布局与会话记录文件内容——都不是可信输入,空格/引号/管道/换行
/// 等一切 shell 元字符在此拦截(逐条对照 `src/utils/aiResume.ts`)。
pub fn build_resume_command(agent: &str, session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id.len() > 128 {
        return None;
    }
    if !session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(match agent {
        "codex" => format!("codex resume {session_id}"),
        "grok" => format!("grok --resume {session_id}"),
        _ => format!("claude --resume {session_id}"),
    })
}

/// 项目是否有 WSL 会话来源:UNC 形态的 WSL 根项目,或显式配置了发行版。
///
/// (`mt_ai` 的 `parse_wsl_unc` 目前是 crate 私有,这里按前缀判一道;
/// 见交付说明的「接线需求」。)
fn has_wsl_source(path: &str, distro: Option<&str>) -> bool {
    if distro.is_some_and(|d| !d.is_empty()) {
        return true;
    }
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    lower.starts_with("\\\\wsl$\\") || lower.starts_with("\\\\wsl.localhost\\")
}

/// ISO 8601 → 「刚刚 / n 分钟前 / n 小时前 / n 天前 / 月-日」。
fn format_time(iso: &str) -> String {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return String::new();
    };
    let now = chrono::Local::now();
    let minutes = (now.timestamp() - ts.timestamp()) / 60;
    if minutes < 1 {
        return "刚刚".into();
    }
    if minutes < 60 {
        return format!("{minutes} 分钟前");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} 小时前");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days} 天前");
    }
    let local = ts.with_timezone(&chrono::Local);
    use chrono::Datelike;
    if local.year() == now.year() {
        format!("{}/{}", local.month(), local.day())
    } else {
        format!("{}/{}/{}", local.year(), local.month(), local.day())
    }
}

/// 会话正文预览的一次加载。
struct Preview {
    title: String,
    loading: bool,
    error: Option<String>,
    messages: Vec<AiSessionMessage>,
    /// 可复制的 resume 命令(拼不出来则为 None)。
    command: Option<String>,
}

pub struct SessionPanel {
    store: Entity<AppStore>,
    /// 上次拉取用的项目路径 —— 项目切换时据此重拉。
    project_path: Option<String>,
    host: Vec<AiSession>,
    wsl: Vec<AiSession>,
    loading: bool,
    wsl_loading: bool,
    display_count: usize,
    /// 请求序号:项目切换后旧请求(尤其是慢的 WSL)返回时不得覆盖新项目的列表。
    request_id: u64,
    /// 抽屉是否展开。旧版 `SessionList` 挂在 `RightDrawer` 里,收起时压根不挂载,
    /// 自然也不会去扫会话 —— 这里是常驻实体,只能自己记一份可见性。
    visible: bool,
    /// 关着的时候项目切过 → 打开时补拉一次。
    stale: bool,
    preview: Option<Preview>,
    _tasks: Vec<Task<()>>,
}

impl SessionPanel {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            // 项目切了才重拉;别的 store 变化(状态灯之类)只重画
            let path = this.store.read(cx).active_project().map(|p| p.path.clone());
            if path != this.project_path {
                if this.visible {
                    this.refresh(false, cx);
                } else {
                    // 收着的时候不去扫:WSL 那一路要冷启动整台 VM,不该由「切了个
                    // 项目」触发(旧版收起时组件根本没挂载)
                    this.stale = true;
                }
            }
            cx.notify();
        })
        .detach();
        Self {
            store,
            project_path: None,
            host: Vec::new(),
            wsl: Vec::new(),
            loading: false,
            wsl_loading: false,
            display_count: PAGE_SIZE,
            request_id: 0,
            visible: false,
            stale: true,
            preview: None,
            _tasks: Vec::new(),
        }
    }

    /// 抽屉开合。第一次展开(或关着的时候项目切过)在这里补拉。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible && self.stale {
            self.refresh(false, cx);
        }
    }

    /// 重拉两个来源。`force` 绕过 `mt_ai` 的会话缓存(手动刷新用)。
    pub fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        let project = self
            .store
            .read(cx)
            .active_project()
            .map(|p| (p.path.clone(), p.wsl_sessions_distro.clone()));
        self.request_id += 1;
        let req = self.request_id;
        self.stale = false;
        self.host.clear();
        self.wsl.clear();
        self.display_count = PAGE_SIZE;
        self.preview = None;
        self._tasks.clear();

        let Some((path, distro)) = project else {
            self.project_path = None;
            self.loading = false;
            self.wsl_loading = false;
            cx.notify();
            return;
        };
        self.project_path = Some(path.clone());
        self.loading = true;

        // 宿主来源:秒出,先显示
        let host_path = path.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_ai::sessions::get_ai_sessions(host_path) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request_id != req {
                    return;
                }
                this.host = result.unwrap_or_default();
                this.loading = false;
                cx.notify();
            });
        }));

        // WSL 来源:并行请求,到达后合并(不阻塞宿主显示)
        if has_wsl_source(&path, distro.as_deref()) {
            self.wsl_loading = true;
            self._tasks.push(cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        mt_ai::sessions::get_wsl_ai_sessions(path, distro, Some(force))
                    })
                    .await;
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.request_id != req {
                        return;
                    }
                    this.wsl = result.unwrap_or_default();
                    this.wsl_loading = false;
                    cx.notify();
                });
            }));
        } else {
            self.wsl_loading = false;
        }
        cx.notify();
    }

    /// 两个来源按时间戳降序混排(与后端排序口径一致:ISO 8601 字符串比较)。
    fn merged(&self) -> Vec<&AiSession> {
        let mut all: Vec<&AiSession> = self.host.iter().chain(self.wsl.iter()).collect();
        if !self.wsl.is_empty() {
            all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        }
        all
    }

    /// 在当前活动 pane 里恢复会话。没有终端时退化成「开一个新的再恢复」。
    fn resume(&mut self, command: String, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let existing = self.store.read(cx).active_pane_id(&project_id);
        self.store.update(cx, |store, cx| {
            let target = if new_tab || existing.is_none() {
                // 不能事后再 resolveActivePane:新终端的焦点还没落下去,
                // 拿到的会是用户原本待着的那个 —— 命令就敲进别人的会话了
                store.new_terminal(&project_id, None, existing.clone(), window, cx)
            } else {
                existing.clone()
            };
            let Some(pane_id) = target else { return };
            store.write_to_pane(&project_id, &pane_id, &format!("{command}\r"), cx);
            store.focus_pane(&project_id, &pane_id, window, cx);
        });
    }

    fn open_preview(&mut self, session: &AiSession, cx: &mut Context<Self>) {
        let Some(project_path) = self.project_path.clone() else {
            return;
        };
        self.preview = Some(Preview {
            title: session.title.clone(),
            loading: true,
            error: None,
            messages: Vec::new(),
            command: build_resume_command(&session.session_type, &session.id),
        });
        let session_type = session.session_type.clone();
        let session_id = session.id.clone();
        let distro = session.wsl_distro.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            // 正文可能几 MB + WSL 9P,雷打不动丢后台
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_ai::sessions::get_ai_session_content(
                        session_type,
                        session_id,
                        project_path,
                        distro,
                    )
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let Some(preview) = this.preview.as_mut() else {
                    return;
                };
                preview.loading = false;
                match result {
                    Ok(messages) => preview.messages = messages,
                    Err(err) => preview.error = Some(err),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_preview(&mut self, preview_title: String, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = self.preview.as_ref() else {
            return div().into_any_element();
        };
        let mut body = div()
            .id("session-preview-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0));

        if preview.loading {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(ui::text_muted())
                    .child("读取会话正文…"),
            );
        }
        if let Some(err) = &preview.error {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(ui::color_error())
                    .child(err.clone()),
            );
        }
        for msg in &preview.messages {
            let is_user = msg.role == "user";
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .bg(if is_user {
                        ui::bg_overlay()
                    } else {
                        ui::bg_base()
                    })
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(ui::text_muted())
                            .child(if is_user { "你" } else { "AI" }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(ui::text_secondary())
                            .child(msg.content.clone()),
                    ),
            );
        }

        let command = preview.command.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        ui::ghost_button("session-preview-back", "‹ 返回").on_click(cx.listener(
                            |this: &mut Self, _, _window, cx| {
                                this.preview = None;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_size(px(12.0))
                            .text_color(ui::text_primary())
                            .child(preview_title),
                    )
                    .when_some(command, |el, command| {
                        el.child(
                            ui::ghost_button("session-copy-cmd", "复制命令").on_click(
                                move |_, _window, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        command.clone(),
                                    ));
                                },
                            ),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }
}

impl Render for SessionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .border_l_1()
            .border_color(ui::border_default());

        if let Some(title) = self.preview.as_ref().map(|p| p.title.clone()) {
            let body = self.render_preview(title, cx);
            return container.child(body);
        }

        let sessions: Vec<AiSession> = self
            .merged()
            .into_iter()
            .take(self.display_count)
            .cloned()
            .collect();
        let total = self.host.len() + self.wsl.len();
        let has_more = self.display_count < total;
        let loading = self.loading;
        let wsl_loading = self.wsl_loading;
        let has_project = self.project_path.is_some();

        let mut list = div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .px(px(6.0))
            .flex()
            .flex_col();

        if loading && sessions.is_empty() {
            list = list.child(
                div()
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(ui::text_muted())
                    .child("加载中…"),
            );
        } else if sessions.is_empty() {
            list = list.child(
                div()
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(ui::text_muted())
                    .child(if has_project {
                        "该项目还没有 AI 会话记录"
                    } else {
                        "先选一个项目"
                    }),
            );
        }

        for session in sessions {
            let key = format!(
                "{}-{}-{}",
                session.session_type,
                session.wsl_distro.as_deref().unwrap_or("host"),
                session.id
            );
            let command = build_resume_command(&session.session_type, &session.id);
            // 会话来自 WSL 时,把命令敲进本机终端是跑不通的 —— 只留查看
            let can_resume_here = command.is_some() && session.wsl_distro.is_none();
            let title = session.title.clone();
            let time = format_time(&session.timestamp);
            let badge = match session.session_type.as_str() {
                "codex" => "CX",
                "grok" => "GK",
                _ => "CL",
            };
            let wsl_badge = session.wsl_distro.clone();
            let session_for_preview = session.clone();
            let cmd_here = command.clone();
            let cmd_tab = command.clone();

            // 标题一行、动作一行:抽屉最窄 240px,标题与三个按钮并排会把标题挤成
            // 一列竖排的字(实测),所以按钮另起一行。
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(6.0))
                    .py(px(5.0))
                    .rounded(px(4.0))
                    .hover(|el| el.bg(ui::bg_overlay()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.0))
                                    .text_color(ui::text_muted())
                                    .child(badge),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(ui::text_secondary())
                                    .child(title.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_size(px(10.0))
                                    .text_color(ui::text_muted())
                                    .child(match wsl_badge {
                                        Some(distro) => format!("{time} · WSL·{distro}"),
                                        None => time,
                                    }),
                            )
                            .child(
                                ui::ghost_button(
                                    SharedString::from(format!("view-{key}")),
                                    "查看",
                                )
                                .flex_none()
                                .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                                    this.open_preview(&session_for_preview, cx);
                                })),
                            )
                            .when(can_resume_here, |el| {
                                el.child(
                                    ui::ghost_button(
                                        SharedString::from(format!("resume-{key}")),
                                        "恢复",
                                    )
                                    .flex_none()
                                    .on_click(cx.listener(
                                        move |this: &mut Self, _, window, cx| {
                                            if let Some(cmd) = cmd_here.clone() {
                                                this.resume(cmd, false, window, cx);
                                            }
                                        },
                                    )),
                                )
                                .child(
                                    ui::ghost_button(
                                        SharedString::from(format!("resume-tab-{key}")),
                                        "新标签",
                                    )
                                    .flex_none()
                                    .on_click(cx.listener(
                                        move |this: &mut Self, _, window, cx| {
                                            if let Some(cmd) = cmd_tab.clone() {
                                                this.resume(cmd, true, window, cx);
                                            }
                                        },
                                    )),
                                )
                            }),
                    ),
            );
        }

        if has_more {
            let remaining = total - self.display_count;
            list = list.child(
                div()
                    .id("session-load-more")
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::accent()))
                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                        this.display_count += PAGE_SIZE;
                        cx.notify();
                    }))
                    .child(format!("加载更多({remaining})")),
            );
        }

        container
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(ui::text_muted())
                                    .child("AI 会话"),
                            )
                            .when(wsl_loading, |el| {
                                el.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(ui::text_muted())
                                        .child("WSL 加载中…"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("session-refresh")
                            .px(px(6.0))
                            .text_size(px(12.0))
                            .text_color(ui::text_muted())
                            .cursor_pointer()
                            .hover(|el| el.text_color(ui::accent()))
                            .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                                this.refresh(true, cx);
                            }))
                            .child("↻"),
                    ),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// resume 命令按 agent 分派,id 过白名单。
    #[test]
    fn resume_命令按_agent_分派() {
        assert_eq!(
            build_resume_command("claude", "abc-123").as_deref(),
            Some("claude --resume abc-123")
        );
        assert_eq!(
            build_resume_command("codex", "rollout_9").as_deref(),
            Some("codex resume rollout_9")
        );
        assert_eq!(
            build_resume_command("grok", "0199-x").as_deref(),
            Some("grok --resume 0199-x")
        );
        // 未知 agent 按 claude 兜底(与旧版一致)
        assert!(build_resume_command("whatever", "id1").is_some());
    }

    /// shell 元字符一律拦下 —— 这条命令是要原样写进 PTY 的。
    #[test]
    fn 非法会话_id_拒绝拼命令() {
        for bad in [
            "a b",
            "a;rm -rf /",
            "a|b",
            "a\nb",
            "a$(x)",
            "a`x`",
            "a\"b",
            "a'b",
            "../../etc",
            "",
        ] {
            assert!(
                build_resume_command("claude", bad).is_none(),
                "应拒绝: {bad:?}"
            );
        }
        assert!(build_resume_command("claude", &"a".repeat(129)).is_none());
        assert!(build_resume_command("claude", &"a".repeat(128)).is_some());
    }

    #[test]
    fn wsl_来源判定() {
        assert!(has_wsl_source("\\\\wsl$\\Ubuntu\\home\\u", None));
        assert!(has_wsl_source("\\\\wsl.localhost\\Debian\\srv", None));
        assert!(has_wsl_source("D:\\Git\\x", Some("Ubuntu")));
        assert!(!has_wsl_source("D:\\Git\\x", None));
        assert!(!has_wsl_source("D:\\Git\\x", Some("")));
    }

    #[test]
    fn 时间戳解析不出来时不显示() {
        assert_eq!(format_time("不是时间"), "");
        assert_eq!(format_time(""), "");
    }
}
