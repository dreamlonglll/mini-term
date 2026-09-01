//! 偏好与配置写入相关的 `AppStore` 方法:AI 历史面板视图、用量面板偏好、主题、
//! shell 列表、通用配置补丁、终端渲染参数、界面字号/字族、AI 感知取数口、
//! 界面语言、pane 重命名、移动端中转。
//!
//! 从 `store.rs` 原样搬来的一串段落,段注释随代码走,逻辑一行未改。

use gpui::{App, Context, Entity, Window};
use mt_config::{AiLauncher, AppConfig, MobileRelayConfig};
use mt_relay::MobileRelayStatusPayload;
use mt_ui::theme_bridge::BackgroundArt;

use crate::ai::AiBridge;
use crate::pane::TerminalPane;
use crate::shell_ops::ShellList;

use super::pure::{find_pane_of_pty, rename_pane_in_states, resolve_scrollback};
use super::{AppStore, UsagePrefs};

impl AppStore {
    // === AI 历史面板视图偏好 ===

    /// 会话列表视图(`"flat"` | `"tree"`)。认不出/没设过 = 平铺
    /// (原版 `SessionList.tsx:242` 的 `?? 'flat'`)。
    pub fn session_list_view(&self) -> &str {
        match self.config.session_list_view.as_deref() {
            Some("tree") => "tree",
            _ => "flat",
        }
    }

    pub fn set_session_list_view(&mut self, view: &str, cx: &mut Context<Self>) {
        if self.session_list_view() == view {
            return;
        }
        self.config.session_list_view = Some(view.to_string());
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 用量面板偏好 ===

    /// 六个偏好**一把写** —— 六个 setter 各自触发一次 500ms 去抖没有意义。
    pub fn set_usage_prefs(&mut self, prefs: UsagePrefs, cx: &mut Context<Self>) {
        self.config.usage_scope = Some(prefs.scope);
        self.config.usage_range = Some(prefs.range);
        self.config.usage_project = prefs.project;
        self.config.usage_auto_refresh = Some(prefs.auto_refresh);
        self.config.usage_custom_from = Some(prefs.custom_from);
        self.config.usage_custom_to = Some(prefs.custom_to);
        self.save_config_soon(cx);
    }

    /// 用户对 pane 键入 = 已在处理待确认事项,清掉 attention 黄灯
    /// (旧版 `clearPaneAttentionByPty`)。
    ///
    /// codex 批准后直到 PostToolUse 没有任何 hook 事件,不清会误挂整个执行期。
    pub fn clear_pane_attention_by_pty(&mut self, pty_id: u32, cx: &mut Context<Self>) {
        let mut changed = false;
        for state in self.project_states.values_mut() {
            if let Some(pane) = state.pane_by_pty_mut(pty_id)
                && pane.attention
            {
                pane.attention = false;
                changed = true;
                break;
            }
        }
        if changed {
            cx.notify();
        }
    }

    // === 主题 ===

    /// 按当前配置装配主题:gpui-component 主题层 + 壳配色 + 终端配色。
    ///
    /// **已存在的终端也热更新** —— 对应旧版
    /// `terminalCache.ts::updateAllTerminalThemes`,不然换主题只有新开的终端跟着变。
    ///
    /// 启动、切亮暗、切皮肤、开关 `terminalFollowTheme` 全走这一条路。
    pub fn apply_theme_from_config(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let applied = crate::theme::apply(&self.config, window, cx);
        if let Some(failed) = &applied.failed_pack {
            // 只清内存不落盘:主题目录可能只是这次读不到(盘没挂载、文件正被替换),
            // 落盘会把用户的选择永久抹掉,下次启动就找不回来了(旧版同一红线)。
            self.config.custom_theme_id = None;
            eprintln!("[store] 主题包 {failed} 本次不可用,已回落内置外观(配置未改盘)");
        }
        crate::ui::set_palette(applied.palette);
        self.background_art = applied.background;
        self.terminal_theme = applied.terminal.clone();

        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            let theme = applied.terminal.clone();
            entity.update(cx, |pane, cx| pane.set_theme(theme, cx));
        }
        cx.notify();
    }

    /// 当前主题的背景图参数(渲染归 mt-ui,这里只是取数口)。
    #[allow(dead_code)] // 消费方是 mt-ui 的背景图渲染,尚未落地
    pub fn background_art(&self) -> Option<&BackgroundArt> {
        self.background_art.as_ref()
    }

    /// 切内置亮/暗/跟随系统(`light` / `dark` / `auto`)。
    ///
    /// **切亮暗 = 退出外置皮肤** —— 皮肤的明暗由作者在 `theme.json` 里定死,
    /// 留着它这一步就没有效果(旧版 themePackManager 的同一条约定)。
    pub fn set_theme_mode(&mut self, mode: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.config.theme = mode.to_string();
        self.config.custom_theme_id = None;
        self.apply_theme_from_config(Some(window), cx);
        self.save_config_soon(cx);
    }

    /// 切外置主题包;`None` = 退出皮肤回内置外观。
    ///
    /// 装不上返回 `false` 且**不落盘**:内存里已经回落内置,配置里那条
    /// `customThemeId` 不该被这次失败改掉。
    pub fn set_theme_pack(
        &mut self,
        theme_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.config.custom_theme_id = theme_id.clone();
        self.apply_theme_from_config(Some(window), cx);
        if self.config.custom_theme_id != theme_id {
            return false;
        }
        self.save_config_soon(cx);
        true
    }

    /// 终端配色跟不跟随主题。关掉 = 终端固定内置暗色(旧版同一行为)。
    pub fn set_terminal_follow_theme(
        &mut self,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.config.terminal_follow_theme == follow {
            return;
        }
        self.config.terminal_follow_theme = follow;
        self.apply_theme_from_config(Some(window), cx);
        self.save_config_soon(cx);
    }

    // === 终端配置(shell 列表)===

    pub fn shell_list(&self) -> ShellList {
        ShellList {
            shells: self.config.available_shells.clone(),
            default_shell: self.config.default_shell.clone(),
        }
    }

    pub fn apply_shell_list(&mut self, list: ShellList, cx: &mut Context<Self>) {
        self.config.available_shells = list.shells;
        self.config.default_shell = list.default_shell;
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 通用配置补丁 ===

    /// 写一份配置补丁并落盘(对应原版 `SettingsModal.tsx:59-70` 的 `useConfigPatch`)。
    ///
    /// 设置页上百个开关全走这一条:改字段 → 500ms 防抖落盘 → `cx.notify()`。
    /// 需要**额外副作用**的那几项(主题 / 字号 / 字族 / 回滚行数 / 停留时长)
    /// 各有自己的 setter,不要拿这个入口去改它们 —— 热更新会漏。
    pub fn patch_config(&mut self, edit: impl FnOnce(&mut AppConfig), cx: &mut Context<Self>) {
        edit(&mut self.config);
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 终端渲染参数(四项热更新)===

    /// 终端字号。**热更新全部已开终端** —— 原版由 `TerminalInstance` 订阅 config
    /// 改 `term.options.fontSize`,这里走 `TerminalView::set_style`(cell 尺寸随之
    /// 变化,下一帧连带 resize grid 与 PTY)。
    pub fn set_terminal_font_size(&mut self, size: f64, cx: &mut Context<Self>) {
        let size = size.clamp(8.0, 32.0);
        if (self.config.terminal_font_size - size).abs() < f64::EPSILON {
            return;
        }
        self.config.terminal_font_size = size;
        self.apply_terminal_style(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 终端字族。空串 = 回落默认(写 `None`,不落空串)。
    ///
    /// 用户自选字体也会**自动补 CJK 回退**,与原版 `resolveTerminalFontFamily`
    /// (terminalCache.ts:53-58)同语义 —— 见 [`terminal_style_from`]。
    pub fn set_terminal_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let next = family
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if self.config.terminal_font_family == next {
            return;
        }
        self.config.terminal_font_family = next;
        self.apply_terminal_style(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 连体字开关。**字族本身得带 `calt` 表**才看得见效果 —— 默认的
    /// Cascadia **Mono** 是去连字版,开了也没东西可连(设置页那行提示说的就是这个)。
    ///
    /// 存量终端连带下发,不然只对新开的终端生效。
    pub fn set_terminal_ligatures(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.terminal_ligatures == enabled {
            return;
        }
        self.config.terminal_ligatures = enabled;
        self.apply_terminal_style(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 回滚行数。**热更新全部已开终端**:调小时 alacritty 的 `update_history`
    /// 当场裁掉多余历史并释放内存(原版 `updateAllTerminalScrollback` 同效果)。
    pub fn set_terminal_scrollback(&mut self, lines: u32, cx: &mut Context<Self>) {
        let lines = resolve_scrollback(lines as f64);
        if self.config.terminal_scrollback == lines {
            return;
        }
        self.config.terminal_scrollback = lines;
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            entity.update(cx, |pane, _| pane.set_scrollback(lines as usize));
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 拖选停留自动复制时长。`0` = 关掉停留语义(退回「松手即复制」)。
    ///
    /// 存量终端要连带下发 —— 不然改了只对新开的终端生效。
    pub fn set_selection_auto_copy_secs(&mut self, secs: f64, cx: &mut Context<Self>) {
        if self.config.selection_auto_copy_secs == Some(secs) {
            return;
        }
        self.config.selection_auto_copy_secs = Some(secs);
        let dwell = self.selection_dwell();
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            entity.update(cx, |pane, cx| pane.set_selection_dwell(dwell, cx));
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 受编排会话并发上限(ADR 0003 / 工单 08)。
    ///
    /// **改完立刻推给控制面**,不搭 [`Self::save_config_soon`] 那班 500ms 防抖的
    /// 车:验收口径是「用户改完立刻在编排者里试」,而裁决读的是控制面里那个
    /// 原子量、不是磁盘。落盘照旧防抖(这是个数字输入框,失焦/回车才提交一次,
    /// 不存在拖动时每帧写盘的问题)。
    ///
    /// 调低**不动已存活的乐手**:上限只在 `start-session` 那一行被读一次
    /// (钉子见 `mt_ai::control` 的 `调低上限不动已存活的乐手`)。
    pub fn set_orchestrator_session_cap(&mut self, cap: u32, cx: &mut Context<Self>) {
        if self.config.orchestrator_session_cap == Some(cap) {
            return;
        }
        self.config.orchestrator_session_cap = Some(cap);
        self.ai
            .set_orchestrator_session_cap(crate::orchestrator::resolve_session_cap(Some(cap)));
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 把当前的终端字号/字族下发给**全部**已开终端。
    fn apply_terminal_style(&mut self, cx: &mut Context<Self>) {
        let style = self.terminal_style();
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            let style = style.clone();
            entity.update(cx, |pane, cx| pane.set_style(style, cx));
        }
    }

    // === 界面字号 / 字族 ===

    /// 把 `uiFontSize` / `uiFontFamily` 装进 [`crate::ui`] 的快照。
    ///
    /// **启动时也要调**(在建任何视图之前),否则首帧按默认 13px 画出来再被刷一遍。
    /// 与 `apply_theme_from_config` 同形:改一次快照,下一帧所有视图跟着变。
    pub fn apply_ui_font(&self) {
        crate::ui::set_ui_font(
            self.config.ui_font_size,
            self.config.ui_font_family.as_deref(),
        );
    }

    /// 界面字号(滑块 10..20)。**即时全局**,等价于原版改 `html` 的 `font-size`。
    pub fn set_ui_font_size(&mut self, size: f64, cx: &mut Context<Self>) {
        if (self.config.ui_font_size - size).abs() < f64::EPSILON {
            return;
        }
        self.config.ui_font_size = size;
        self.apply_ui_font();
        // 字号散在几十个 `render` 里,没有哪个 Entity 能代表「全部文字」——
        // 与切语言同一处理:让所有窗口重画(设置页一辈子也拖不了几次滑块)
        cx.refresh_windows();
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 界面字族。空串 = 回落平台默认(写 `None`,不落空串)。
    pub fn set_ui_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let next = family
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if self.config.ui_font_family == next {
            return;
        }
        self.config.ui_font_family = next;
        self.apply_ui_font();
        cx.refresh_windows();
        self.save_config_soon(cx);
        cx.notify();
    }

    // === AI 感知(hook 页要用)===

    /// AI 桥的一份克隆(hook 服务器开关 / 状态查询)。
    pub fn ai(&self) -> AiBridge {
        self.ai.clone()
    }

    // === 界面语言 ===

    /// 当前界面语言。取自配置,认不出(或没设过)时回落到**进程内实际生效**的那个
    /// —— 也就是启动时 `i18n::install` 按系统语言探测出来的结果,这样语言切换
    /// 段控件的高亮与眼前看到的文案始终一致。
    pub fn locale(&self) -> mt_i18n::Locale {
        self.config
            .locale
            .as_deref()
            .and_then(mt_i18n::Locale::from_code)
            .unwrap_or_else(mt_i18n::locale)
    }

    /// 切界面语言。对应 TS 侧 `useI18nStore.setLang`,只是落点从 localStorage
    /// 换成了 `config.locale`(GPUI 没有 localStorage,配置文件是唯一的持久层)。
    ///
    /// **一定要落盘**:探测出来的语言不写、用户选的语言必写 —— 否则下次启动又被
    /// 系统语言盖回去,选择等于没生效。
    pub fn set_locale(&mut self, locale: mt_i18n::Locale, cx: &mut Context<Self>) {
        let code = locale.code().to_string();
        if self.config.locale.as_deref() == Some(code.as_str()) && mt_i18n::locale() == locale {
            return;
        }
        self.config.locale = Some(code);
        // 进程内切换 + 全窗口重绘(观察者顺带把 gpui-component 的 rust-i18n 也改了)
        crate::i18n::switch(locale, cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    // === pane 重命名 ===

    /// 改 tab 标题。空字符串 = 恢复默认(shell 名)。
    ///
    /// **不落盘** —— `SavedPane` 里没有这个字段,装机版同样只在运行时保留
    /// (`serializeSplitNode` 只写 shellName/cwd/aiSession)。磁盘格式一字不改。
    pub fn rename_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        let title = title.trim();
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(pane) = state.pane_mut(pane_id)
        {
            pane.custom_title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            cx.notify();
        }
    }

    /// 移动端改会话名:按 `pane_id` **全局**定位 —— 移动端只认得 pane,
    /// 不知道它挂在哪个项目下(`src/store.ts:1163-1180`)。
    ///
    /// 空串 = 清除自定义名、回落 shell 名。**不落盘**:`SavedPane` 里没有
    /// `customTitle`,AI 会话本来也活不过重启。
    ///
    /// 与 [`Self::rename_pane`] 并存:那条是 F2 / 右键改名(知道项目、要 trim),
    /// 这条是移动端来的 —— 标题**已经在 mt-relay 里收敛过**
    /// (trim + 去控制字符 + 64 字符限长,`relay.rs:709-716`),
    /// 这里不再叠加任何收敛,否则两处限长会打架。
    pub fn rename_pane_by_id(&mut self, pane_id: &str, title: &str, cx: &mut Context<Self>) {
        if rename_pane_in_states(&mut self.project_states, pane_id, title) {
            cx.notify();
        }
    }

    // === 移动端中转 ===

    /// `pty_id` → `(project_id, pane_id)` 反查。
    ///
    /// 移动端指令只带 PTY 编号,而 [`Self::write_to_pane`] 要「项目 + pane」。
    pub fn pane_of_pty(&self, pty_id: u32) -> Option<(String, String)> {
        find_pane_of_pty(&self.project_states, pty_id)
    }

    /// 这个 pane 的 PTY 起来了吗。
    ///
    /// `spawn_pane` 就算 PTY 起不来也照样返回 `PaneState`(视图里画一行红字),
    /// 而 [`Self::write_to_pane`] 在没有 PTY 时是静默丢弃的 —— 移动端发起会话的
    /// 回执要靠这一条把「终端根本没起来」与「命令已写入」分开。
    pub fn pane_pty_alive(&self, pty_id: u32, cx: &App) -> bool {
        self.terminals
            .get(&pty_id)
            .is_some_and(|entity| entity.read(cx).spawn_error().is_none())
    }

    /// 中转连接状态(`RelayEvents::status_changed` 的落点)。
    pub fn mobile_relay_status(&self) -> Option<&MobileRelayStatusPayload> {
        self.mobile_relay_status.as_ref()
    }

    pub fn set_mobile_relay_status(
        &mut self,
        status: MobileRelayStatusPayload,
        cx: &mut Context<Self>,
    ) {
        if self.mobile_relay_status.as_ref() == Some(&status) {
            return;
        }
        self.mobile_relay_status = Some(status);
        cx.notify();
    }

    /// 移动端中转配置的**读**口径:整块缺失时回落 `Default`(含预置两条启动器),
    /// 与 `mt_config` 的迁移(`config.rs:666`)同口径。
    pub fn mobile_relay(&self) -> MobileRelayConfig {
        self.config.mobile_relay.clone().unwrap_or_default()
    }

    /// 移动端中转配置的**改**口径,对应原版 `withMobileRelayDefaults` 的
    /// `{ relayUrl:'', desktopKey:'', launchers:[], ...current, ...patch }`:
    /// 整块缺失时 `launchers` 取**空列表而不是预置两条** ——
    /// 凭空补预置会跟后端「用户删光是有意结果」的迁移规则打架
    /// (`src/utils/mobileRelayConfig.ts:8-10`)。
    ///
    /// 与 [`Self::mobile_relay`] 的差别只在「整块缺失」这一种情况下可见,
    /// 而 `load()` 的迁移保证了正常路径上这一块必然在场。
    fn mobile_relay_for_patch(&self) -> MobileRelayConfig {
        self.config
            .mobile_relay
            .clone()
            .unwrap_or_else(|| MobileRelayConfig {
                relay_url: String::new(),
                desktop_key: String::new(),
                launchers: Vec::new(),
            })
    }

    /// 写中转地址 + 桌面端密钥,**其余字段(启动器)一个不动**。
    ///
    /// 立即落盘而不是 500ms 防抖(坑 8):原版是 `await saveConfigToDisk` 之后
    /// 才 `apply`,用户点完「保存并连接」立刻关掉应用,地址不该丢。
    pub fn set_mobile_relay_endpoint(&mut self, url: &str, key: &str, cx: &mut Context<Self>) {
        let mut relay = self.mobile_relay_for_patch();
        relay.relay_url = url.to_string();
        relay.desktop_key = key.to_string();
        self.config.mobile_relay = Some(relay);
        self.save_config_now();
        cx.notify();
    }

    /// 写启动器名单,**地址与密钥一个不动**。同样立即落盘。
    pub fn set_launchers(&mut self, launchers: Vec<AiLauncher>, cx: &mut Context<Self>) {
        let mut relay = self.mobile_relay_for_patch();
        relay.launchers = launchers;
        self.config.mobile_relay = Some(relay);
        self.save_config_now();
        cx.notify();
    }
}
