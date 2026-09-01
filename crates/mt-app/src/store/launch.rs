//! 「按启动器起会话」的共享入口。
//!
//! 一次「起会话」的落地动作是固定四步 —— **校验项目 → 解析 shell → 建 pane →
//! 往 shell 里敲启动命令**。这四步此前有两份:桌面端新终端菜单一份
//! (`store::panes` 的 `*_from_launcher`),移动端中转一份
//! (`mobile_relay::MobileRelayBridge::try_start_session`)。两份都要维护
//! 「命令只能来自桌面端配置」「写不进去也保留 pane」这些 ADR 0002 的纪律,
//! 走散一次就是一个边界洞;编排者(ADR 0003)是第三个消费者,再抄一份不划算。
//!
//! 于是四步收进 [`AppStore::launch_ai_session`],把真正不同的东西参数化:
//!
//! | 差异 | 参数 | 桌面端 | 移动端 |
//! |------|------|--------|--------|
//! | 落点与焦点 | [`LaunchPlacement`] | `Tab` / `Panel`,抢焦点 | `Background`,不抢焦点不切项目 |
//! | 内部环境变量 | [`LaunchRequest::env`] | 空 | 空 |
//! | 诞生一次性提示 | [`LaunchRequest::notice`] | 无(人自己点的) | 有(凭证被盗时唯一的审计迹象) |
//! | 回执 | 返回的 [`LaunchOutcome`] | 丢弃 | 映射成中转协议的回执 |
//!
//! # 刻意不做的事
//!
//! - **不认识中转协议的类型**。失败原因是本模块自己的 [`LaunchError`],由调用方
//!   映射到各自的回执(移动端 → `StartSessionFailReason`,编排控制面 → 它自己的
//!   错误集)。共享入口一旦 `use mt_relay_protocol::...`,编排控制面就得跟着
//!   拖上整个中转体系。
//! - **不判「这个项目能不能远程发起」**。SSH 远程项目与 WSL 根项目的置灰是
//!   *发起侧* 的策略(移动端是 `mt_relay::can_start_session`,推给手机的项目
//!   快照里就已经置灰了),不是落地侧的。两类消费者的可达范围本就不同
//!   (ADR 0003 的编排范围是「本项目 + 同分组」),塞进来只会把两套策略搅在一起。
//! - **不代替调用方回执**。移动端那条「外层统一回执、内层随便 `?` 早退」的结构
//!   (坑 5)留在 `mobile_relay` 自己手里。
//!
//! # 环境变量这条缝
//!
//! [`LaunchRequest::env`] 走的是 [`mt_pty::PtySpawn::env`] —— 应用注入**内部
//! 协议变量**的既有通道(`MINITERM_` 是保留前缀,项目级 env 那条 `user_env`
//! 会被它挡掉)。现在两个调用点都传空;它是给编排者的令牌注入(工单 02/03)
//! 预留的,`MINITERM_PTY_ID` / `MINITERM_HOOK_PORT` 顶不掉,判据见
//! `store::pure` 的 `merge_internal_env`。

use gpui::{Context, Window};

use crate::notify::ToastKind;

use super::AppStore;

/// 新会话落在哪、要不要抢焦点 —— 两条路径唯一的结构性差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchPlacement {
    /// 桌面端人工触发:落进锚点 pane 所在叶子的 tab 栏,激活并抢焦点。
    /// `anchor_pane_id = None` 时锚点是当前焦点 pane。
    Tab { anchor_pane_id: Option<String> },
    /// 桌面端人工触发:落成一个新的项目级面板,激活并抢焦点。
    Panel,
    /// 远程发起(移动端 / 编排者):挂进活动面板最左侧叶子的 tab 栏末尾,
    /// **不激活、不抢焦点、不切项目**(ADR 0002 的出生礼仪)。
    Background,
}

/// 诞生一次性提示。
///
/// ADR 0002:远程发起时桌面端弹一次通知,凭证被盗时这是唯一的审计迹象。
/// 只在**启动命令确实写进了活着的 PTY** 时弹 —— 与失败回执互斥,时序与
/// 拆分前的移动端路径一字不差。桌面端人工触发传 `None`(人自己点的,不必回告)。
pub struct LaunchNotice {
    pub kind: ToastKind,
    /// 正文。项目名由 toast 层的标题行展示,这里只补出身与启动器名。
    pub message: String,
}

/// 一次「按启动器起会话」的请求。
///
/// ⚠️ `command` 只在桌面端进程内流转 —— 它来自桌面端配置的具名启动器,
/// 「命令只能来自桌面端配置」是 ADR 0002 的防线本身。别把它写进日志、
/// 错误消息或任何回执。
pub struct LaunchRequest<'a> {
    pub project_id: &'a str,
    /// 启动器展示名。落成 pane 的自定义标题:回到电脑前一眼看出这个标签是什么。
    pub launcher_name: &'a str,
    /// 启动器绑定的 shell 名;`None` = 用默认 shell。
    pub shell_name: Option<&'a str>,
    /// 要敲进 shell 的启动命令(**不含**回车,由入口补)。
    pub command: &'a str,
    pub placement: LaunchPlacement,
    /// 额外注入 PTY 的**应用内部**环境变量(`MINITERM_` 保留前缀)。
    /// 现有两个调用点都传空,见模块注释「环境变量这条缝」。
    pub env: Vec<(String, String)>,
    pub notice: Option<LaunchNotice>,
}

/// 起会话失败的原因。**刻意不复用中转协议的 `StartSessionFailReason`**,
/// 理由见模块注释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchError {
    /// 目标项目已不存在。发起侧校验过一遍,但校验到执行之间用户可能刚好把
    /// 项目移除了 —— 这一档必须单列。
    ProjectNotFound,
    /// 终端没建成:一个 shell 都没配 / 项目在起 PTY 期间被移除 / 布局挂不上。
    SpawnFailed,
}

/// 起会话的结局。**pane 已经建成了** —— 命令写没写进去、PTY 起没起来另说,
/// 两者都为假时 pane 也**保留不杀**(ADR 0002:分不清「起不来」和「起得慢」,
/// 杀掉的破坏性大于留着)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub pane_id: String,
    /// 启动命令写进 PTY 了吗。
    pub command_written: bool,
    /// PTY 真起来了吗。
    ///
    /// `spawn_pane` 起不到 PTY 也照样返回 pane(视图里画一行红字),而
    /// `write_to_pane` 没有 PTY 时是**静默丢弃**的 —— 少了这一问,shell 路径
    /// 失效时调用方会拿到成功回执然后干等超时。
    pub pty_alive: bool,
}

impl LaunchOutcome {
    /// 启动命令确实交到了一根活着的 PTY 手上。远程回执的成功判据。
    pub fn command_delivered(&self) -> bool {
        self.command_written && self.pty_alive
    }
}

impl AppStore {
    /// 按 AI 启动器起一个会话:校验 → 建 pane → 写启动命令 → 诞生提示。
    ///
    /// 桌面端新终端菜单、移动端中转、编排控制面共用这一条。各自的差异经
    /// [`LaunchRequest`] 参数化,详见模块注释。
    ///
    /// 返回 [`LaunchOutcome`] 而不是 `Result<String, _>`:pane 建成之后
    /// 「命令没写进去」不是失败,是**调用方自己决定怎么回执**的事 ——
    /// 桌面端根本不在乎,移动端要把它折成 `SpawnFailed`。
    pub fn launch_ai_session(
        &mut self,
        req: LaunchRequest<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<LaunchOutcome, LaunchError> {
        let LaunchRequest {
            project_id,
            launcher_name,
            shell_name,
            command,
            placement,
            env,
            notice,
        } = req;

        // 1. 项目还在吗。发起侧已经校验过一遍,但从校验到执行之间用户可能
        //    刚好把项目移除了,所以 ProjectNotFound 这一档必须保留。
        if self.project(project_id).is_none() {
            return Err(LaunchError::ProjectNotFound);
        }
        // 2. shell:启动器绑定的 → default_shell → 列表首项。绑定的 shell 被
        //    删掉时退回默认 —— 总比不开好,用户在桌面能看到实情。
        // 3. 一个 shell 都没配:开不出终端,也没有能写进布局的 shellName。
        let shell = self
            .resolve_shell(shell_name)
            .ok_or(LaunchError::SpawnFailed)?;

        // 4. 建 PTY + 建 pane + 按落点挂进布局树。
        let pane_id = match placement {
            LaunchPlacement::Tab { anchor_pane_id } => {
                let pane_id = self
                    .new_terminal_with_env(project_id, Some(shell), anchor_pane_id, None, &env, window, cx)
                    .ok_or(LaunchError::SpawnFailed)?;
                // 标题走 `rename_pane`(会 trim)—— 拆分前桌面端那两条就是它,
                // 与 `Background` 那档的 `custom_title`(建 pane 时原样带上)
                // 差一个 trim。**别为了对称把两边并成一条**:那是行为变更。
                self.rename_pane(project_id, &pane_id, launcher_name, cx);
                pane_id
            }
            LaunchPlacement::Panel => {
                let pane_id = self
                    .new_panel_with_env(project_id, Some(shell), &env, window, cx)
                    .ok_or(LaunchError::SpawnFailed)?;
                self.rename_pane(project_id, &pane_id, launcher_name, cx);
                pane_id
            }
            LaunchPlacement::Background => self
                .append_pane_background(
                    project_id,
                    shell,
                    Some(launcher_name.to_string()),
                    &env,
                    window,
                    cx,
                )
                .ok_or(LaunchError::SpawnFailed)?,
        };

        // 5. 写启动命令 + 回车。
        //
        //    ⚠️ 必须走 `write_to_pane` 而不是裸 PTY 写:AI 会话身份靠**输入
        //    检测**建立,只有「往 shell 里敲进启动命令并回车」这条路能让 pane
        //    进入 AI 会话状态。把 AI CLI 当成 PTY 根程序 spawn
        //    (`shell -c "claude"`)会绕开检测,拿不到状态徽章与对话镜像 ——
        //    这是 ADR 0002 定下的纪律,别改。
        //
        //    PTY 内核缓冲 stdin,shell 就绪前写入不丢。写不进去时**保留 pane**:
        //    用户回头能看到它卡在哪。
        let command_written = self.write_to_pane(project_id, &pane_id, &format!("{command}\r"), cx);
        let pty_alive = self
            .project_state(project_id)
            .and_then(|s| s.pane(&pane_id))
            .and_then(|p| p.pty_id)
            .is_some_and(|pty_id| self.pane_pty_alive(pty_id, cx));
        let outcome = LaunchOutcome {
            pane_id,
            command_written,
            pty_alive,
        };

        // 6. 诞生一次性提示。走自建 toast 层,**不去重** —— 连开两个会话该看到
        //    两条(原版 `mobileStartSession.ts:122-127` 就是裸 `pushNotification`)。
        //    项目名由标题行展示,正文只补启动器名。
        if let Some(notice) = notice
            && outcome.command_delivered()
        {
            let project_name = self
                .project(project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            crate::toast::push_message(
                notice.kind,
                project_id.to_string(),
                project_name,
                notice.message,
                cx,
            );
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(written: bool, alive: bool) -> LaunchOutcome {
        LaunchOutcome {
            pane_id: "pane-1".into(),
            command_written: written,
            pty_alive: alive,
        }
    }

    /// 远程回执的成功判据是**两个都真**。
    ///
    /// 少问一句 `pty_alive`,shell 路径失效时手机会拿到成功回执然后干等 15s
    /// 超时(`write_to_pane` 没有 PTY 时静默丢弃,返回值只说明「找到了那个
    /// 终端实体」)—— 这条坑不许回去。
    #[test]
    fn 命令送达要求写入成功且_pty_活着() {
        assert!(outcome(true, true).command_delivered());
        assert!(!outcome(false, true).command_delivered());
        assert!(!outcome(true, false).command_delivered());
        assert!(!outcome(false, false).command_delivered());
    }
}
