//! 文案层接线:把 [`mt_i18n`] 装进 GPUI 壳。
//!
//! # 三件事
//!
//! 1. **启动装配**([`install`]):`config.locale` 优先,没有就按系统语言探测
//!    ([`system_locale`]),定下来之后立刻挂观察者;
//! 2. **桥接 gpui-component**([`bridge_component_locale`]):组件库内部走
//!    rust-i18n(`locales/ui.yml`),日期选择器的星期名、输入框的右键菜单都取
//!    自那份字典。它的语言键是 **BCP 47**(`zh-CN` / `en`),所以桥过去的是
//!    [`Locale::bcp47`] 而不是 `code()` —— 传 `"zh"` 的话组件库会当成未知语言
//!    回落英文,于是自研文案是中文、组件库文案是英文,半页中半页英;
//! 3. **切换与重绘**([`switch`]):写全局语言 → 观察者顺带把 rust-i18n 也改了
//!    → `cx.refresh_windows()` 让每个窗口重新渲染。
//!
//! # 为什么重绘不在观察者里做
//!
//! [`mt_i18n::add_locale_observer`] 只收 `fn(Locale)` 裸函数指针(见它的注释:
//! 静态全局里放 `Box<dyn Fn>` 有销毁顺序问题),而 GPUI 的重绘需要 `&mut App`,
//! 拿不到也不能安全地全局持有。于是分工:**进程级副作用**(rust-i18n)放观察者,
//! 保证任何调用方切语言都生效;**重绘**放 [`switch`] —— UI 上切语言只有那一条路。
//!
//! # 文案 key 从哪来
//!
//! 一律照抄 TS 侧 `src/i18n/locales/*.ts` 里同一句文案的 `ns.key`,
//! **不在 Rust 侧新造 key**:字典是 `tools/gen_from_ts.mjs` 从 TS 生成的,
//! 手加进 `dict.rs` 的条目下次重新生成就没了。
//!
//! GPUI 专属的文案(原版界面上压根没有的串)照样走这条路:先在 TS 侧
//! `locales/<ns>.ts` 里 zh/en 双语加词条,再 `node crates/mt-i18n/tools/gen_from_ts.mjs`
//! 重生成字典,并把 `mt-i18n/tests/consistency.rs` 里的条目总数对账常量改成
//! 新数目 —— 这是文档记载的正常再生成流程,不是「改断言迁就实现」。

use gpui::App;

pub use mt_i18n::{Locale, t, tr};

/// 启动装配。`cfg_locale` 是 `config.locale` 里那个字符串(可能是 `None`、
/// 也可能被手改成了认不出的值,两种情况都退到系统语言)。
///
/// 返回最终生效的语言,调用方据此决定要不要回写配置(首启探测出来的结果
/// **不落盘**:与 TS 侧 `detectInitialLang()` 一致,用户没选过就一直跟随系统)。
pub fn install(cfg_locale: Option<&str>) -> Locale {
    let locale = cfg_locale
        .and_then(Locale::from_code)
        .unwrap_or_else(system_locale);
    mt_i18n::set_locale(locale);
    // set_locale 只在**变化时**通知观察者,而这一次多半没变(默认就是 Zh),
    // 所以先手动桥一次,再挂观察者管后续切换。
    bridge_component_locale(locale);
    mt_i18n::add_locale_observer(bridge_component_locale);
    locale
}

/// 把语言同步给 gpui-component 内部的 rust-i18n 字典。
///
/// 注册进 [`mt_i18n::add_locale_observer`],所以任何一处 `set_locale` 都会带上它。
fn bridge_component_locale(locale: Locale) {
    // gpui-component 的 locales/ui.yml 用 BCP 47 键(en / zh-CN / zh-HK / it),
    // 传 "zh" 匹配不上会回落到 fallback = "en"。
    gpui_component::set_locale(locale.bcp47());
}

/// 切换界面语言并让全部窗口重画。
///
/// 只改进程内状态;写回 `config.locale`(防抖落盘)是 [`crate::store::AppStore`]
/// 的事,见 `AppStore::set_locale`。
pub fn switch(locale: Locale, cx: &mut App) {
    if !mt_i18n::set_locale(locale) {
        return;
    }
    // 文案散在几十个 `render` 里,没有哪个 Entity 能代表"全部文案"。
    // 语言切换一辈子点不了几次,直接让所有窗口重新渲染最省心也最不会漏。
    cx.refresh_windows();
}

/// 系统语言 → [`Locale`]。规则同 TS 侧 `detectInitialLang()`:`zh*` → 中文,其余英文。
///
/// Windows 走 Win32 `GetUserDefaultLocaleName`(`LANG` / `LC_ALL` 在 Windows 上
/// 通常压根不存在,读环境变量等于永远拿默认值);其余平台走环境变量。
pub fn system_locale() -> Locale {
    #[cfg(windows)]
    {
        if let Some(tag) = win32_user_locale_name() {
            return Locale::from_system_tag(&tag);
        }
    }
    mt_i18n::detect_from_env()
}

/// 把系统语言交给编码识别当地区提示(`mt_project::encoding::set_region_from_language`):
/// 简体中文用户的短 GBK 文本才不会被猜成 EUC-KR / Big5。跟的是**系统**语言而不是界面
/// 语言 —— 文件是在这台机器上写的,与界面挑了哪种语言无关。启动时调一次。
pub fn install_encoding_region() {
    #[cfg(windows)]
    let tag = win32_user_locale_name();
    #[cfg(not(windows))]
    let tag = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()));
    if let Some(tag) = tag {
        mt_project::encoding::set_region_from_language(&tag);
    }
}

/// `GetUserDefaultLocaleName` 的安全包装,返回形如 `"zh-CN"` / `"en-US"` 的标签。
#[cfg(windows)]
fn win32_user_locale_name() -> Option<String> {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;

    /// `LOCALE_NAME_MAX_LENGTH`(winnls.h)。这个常量在 windows crate 里没有随
    /// `Win32_Globalization` 一起导出,而它是**写死在 SDK 头文件里**的固定值
    /// (自 Vista 起从未变过,MSDN `GetUserDefaultLocaleName` 明确要求缓冲区
    /// 按它开),照抄字面量即可 —— 不为一个常量去动 windows crate 的版本/feature。
    const LOCALE_NAME_MAX_LENGTH: usize = 85;

    let mut buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    // 返回值是**含结尾 NUL 的字符数**;0 = 失败。
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    if len <= 1 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len as usize - 1)]))
}

/// 终端查找条的全部文案(`terminalSearch` 命名空间,与旧版同 key),按**当前**语言取一份。
///
/// mt-ui 不依赖 mt-i18n(`dict.rs` 是生成物、改得勤,依赖它就意味着每次重生成都把
/// mt-ui 整个重编),文案由这里注入:`TerminalSearchBar::new` 收的是这个函数本身,
/// 查找条每帧调一次,切语言照旧立刻生效;输入框占位符只在创建时取一次 —— 两个时机
/// 都与注入前(mt-ui 里的 `SearchBarLabels::from_i18n`)一致。
pub fn terminal_search_labels() -> mt_ui::SearchBarLabels {
    let label = |key: &'static str| gpui::SharedString::new_static(t("terminalSearch", key));
    mt_ui::SearchBarLabels {
        title: label("title"),
        placeholder: label("placeholder"),
        no_results: label("noResults"),
        case_sensitive: label("caseSensitive"),
        whole_word: label("wholeWord"),
        regex: label("regex"),
        previous: label("previous"),
        next: label("next"),
        close: label("close"),
    }
}

/// 无法通过源码字面量扫描直接提取的动态 / 包装器文案 key(`ns.key` 全路径,按字典序)。
///
/// 其余所有直接写成 `t("ns", "key")` / `tr!("ns", "key", ...)` 的调用点由测试
/// [`tests::用到的每个_key_两种语言都在`] 自动遍历 `crates/mt-app/src/**/*.rs` 扫描校验,
/// 新增字面量文案调用时不再需要手工登记。
///
/// 只有经由辅助函数、枚举映射或局部包装器(如 `settings` 页的局部封装、`label("...")`、
/// `store::TitleBarLight::i18n_key`、`gitBranch` / `gitLog` / `gitCommit.segments` /
/// `sshModal` 动态键表)间接构造的 key 需在此登记。
#[cfg(test)]
const DYNAMIC_KEYS: &[&str] = &[
    "app.titleBar.close",
    "app.titleBar.maximize",
    "app.titleBar.minimize",
    "app.titleBar.restore",
    "app.titleBar.status.attention",
    "app.titleBar.status.error",
    "app.titleBar.status.idle",
    "app.titleBar.status.working",
    "envVars.error.duplicateKey",
    "envVars.error.emptyKey",
    "envVars.error.invalidKey",
    "envVars.error.invalidValue",
    "envVars.error.protectedPrefix",
    "envVars.error.reservedWslenv",
    "fileTree.dialog.deleteFailedMessage",
    "fileTree.dialog.deleteFailedTitle",
    "fileTree.dialog.renameFailedMessage",
    "fileTree.dialog.renameFailedTitle",
    "fileTree.upload.chooseFilesTitle",
    "fileTree.upload.chooseFolderTitle",
    "gitBranch.bannerCherryPick",
    "gitBranch.bannerMerge",
    "gitBranch.bannerRebase",
    "gitBranch.bannerRevert",
    "gitBranch.kindCherryPick",
    "gitBranch.kindMerge",
    "gitBranch.kindRebase",
    "gitBranch.kindRevert",
    "gitBranch.menuDelete",
    "gitBranch.menuDeleteRemote",
    "gitBranch.modeFfOnly",
    "gitBranch.modeFfOnlyItem",
    "gitBranch.modeMerge",
    "gitBranch.modeMergeItem",
    "gitBranch.modeRebase",
    "gitBranch.modeRebaseItem",
    "gitBranch.nameBadFormat",
    "gitBranch.nameEmpty",
    "gitBranch.nameExists",
    "gitBranch.nameIllegalChar",
    "gitBranch.nameLeadingDash",
    "gitBranch.nameReserved",
    "gitBranch.nameUnchanged",
    "gitBranch.nameWhitespace",
    "gitBranch.opAbort",
    "gitBranch.opCheckout",
    "gitBranch.opContinue",
    "gitBranch.opCreate",
    "gitBranch.opDelete",
    "gitBranch.opFetch",
    "gitBranch.opPull",
    "gitBranch.opRename",
    "gitBranch.pushInBackground",
    "gitChanges.stageAll",
    "gitChanges.unstageAll",
    "gitCommit.deleteSelected",
    "gitCommit.discardSelected",
    "gitCommit.segments.blockedBinary",
    "gitCommit.segments.blockedDeleteModify",
    "gitCommit.segments.blockedLossy",
    "gitCommit.segments.blockedNoMarkers",
    "gitCommit.segments.blockedTooLarge",
    "gitCommit.segments.blockedUnreadable",
    "gitCommit.segments.ours",
    "gitCommit.segments.oursRebase",
    "gitCommit.segments.pickOurs",
    "gitCommit.segments.pickOursRebase",
    "gitCommit.segments.pickOursThenTheirs",
    "gitCommit.segments.pickOursThenTheirsRebase",
    "gitCommit.segments.pickTheirs",
    "gitCommit.segments.pickTheirsRebase",
    "gitCommit.segments.pickTheirsThenOurs",
    "gitCommit.segments.pickTheirsThenOursRebase",
    "gitCommit.segments.theirs",
    "gitCommit.segments.theirsRebase",
    "gitCommit.segments.writeAndMark",
    "gitCommit.segments.writeOnly",
    "gitCommit.stageSelected",
    "gitCommit.takeOurs",
    "gitCommit.takeOursDeleted",
    "gitCommit.takeOursRebase",
    "gitCommit.takeOursRebaseDeleted",
    "gitCommit.takeTheirs",
    "gitCommit.takeTheirsDeleted",
    "gitCommit.takeTheirsRebase",
    "gitCommit.takeTheirsRebaseDeleted",
    "gitCommit.unstageSelected",
    "gitGutter.hideAnnotations",
    "gitGutter.showAnnotations",
    "gitHistory.remoteNotSupported",
    "gitHistoryContent.copyCommitHash",
    "gitLog.action.checkout",
    "gitLog.action.createBranch",
    "gitLog.action.createTag",
    "gitLog.action.deleteTag",
    "gitLog.action.pushTag",
    "gitLog.action.reset",
    "gitLog.action.reword",
    "gitLog.action.undo",
    "gitLog.detail.moreFiles",
    "gitLog.reason.alreadyCheckedOut",
    "gitLog.reason.busy",
    "gitLog.reason.inHistory",
    "gitLog.reason.merge",
    "gitLog.reason.notHead",
    "gitLog.reason.notInHistory",
    "gitLog.reason.repoBusy",
    "gitLog.reason.root",
    "gitLog.reset.hard",
    "gitLog.reset.keep",
    "gitLog.reset.mixed",
    "gitLog.reset.soft",
    "gitLog.toast.branchCreated",
    "gitLog.toast.branchCreatedCheckedOut",
    "gitLog.toast.picked",
    "gitLog.toast.reverted",
    "gitStash.busy.apply",
    "gitStash.busy.clear",
    "gitStash.busy.drop",
    "gitStash.busy.pop",
    "gitStash.checkout.done",
    "gitStash.checkout.failedTitle",
    "gitStash.checkout.restored",
    "gitStash.result.applied",
    "gitStash.result.popped",
    "gitStash.save.failedTitle",
    "paneGroup.maximize",
    "paneGroup.maximizePane",
    "paneGroup.restore",
    "paneGroup.restorePane",
    "projectList.dragHint.duplicate",
    "projectList.dragHint.forbidden",
    "projectList.dragHint.valid",
    "remoteProject.picker.home",
    "remoteProject.picker.root",
    "remoteProject.picker.up",
    "sessionList.remoteBadgeTitle",
    "settings.aiHook.enableHook",
    "settings.aiHook.enableHookDesc",
    "settings.aiNotification.method",
    "settings.aiNotification.popup",
    "settings.aiNotification.popupDesc",
    "settings.aiNotification.sound",
    "settings.aiNotification.soundDesc",
    "settings.aiNotification.taskbarFlash",
    "settings.aiNotification.taskbarFlashDesc",
    "settings.appearance.language",
    "settings.appearance.terminalFollowTheme",
    "settings.appearance.terminalFollowThemeDesc",
    "settings.appearance.theme",
    "settings.appearance.themeAuto",
    "settings.appearance.themeDark",
    "settings.appearance.themeLight",
    "settings.clipboard.autoCopyDwellDesc",
    "settings.clipboard.autoCopyDwellTitle",
    "settings.clipboard.charThreshold",
    "settings.clipboard.charThresholdDesc",
    "settings.clipboard.copyPaste",
    "settings.clipboard.lineThreshold",
    "settings.clipboard.lineThresholdDesc",
    "settings.clipboard.longPaste",
    "settings.clipboard.longPasteAiOnlyDesc",
    "settings.clipboard.longPasteAiOnlyTitle",
    "settings.clipboard.longPasteDesc",
    "settings.clipboard.longPasteTitle",
    "settings.clipboard.remotePaste",
    "settings.clipboard.smartCopyPasteDesc",
    "settings.clipboard.smartCopyPasteTitle",
    "settings.editor.externalEditor",
    "settings.envVars.section",
    "settings.font.font",
    "settings.font.fontSize",
    "settings.font.ligatures",
    "settings.menu.about",
    "settings.menu.aiHook",
    "settings.menu.aiNotification",
    "settings.menu.appearance",
    "settings.menu.clipboard",
    "settings.menu.envVars",
    "settings.menu.font",
    "settings.menu.general",
    "settings.menu.groupAi",
    "settings.menu.groupAppearance",
    "settings.menu.groupSystem",
    "settings.menu.groupTerminal",
    "settings.menu.remoteNotify",
    "settings.menu.shell",
    "settings.menu.shortcuts",
    "settings.remoteNotify.bark.desc",
    "settings.remoteNotify.bark.key",
    "settings.remoteNotify.bark.keyHint",
    "settings.remoteNotify.bark.server",
    "settings.remoteNotify.bark.serverHint",
    "settings.remoteNotify.channels",
    "settings.remoteNotify.enable",
    "settings.remoteNotify.enableDesc",
    "settings.remoteNotify.mobileLink",
    "settings.remoteNotify.telegram.api",
    "settings.remoteNotify.telegram.apiHint",
    "settings.remoteNotify.telegram.chat",
    "settings.remoteNotify.telegram.chatHint",
    "settings.remoteNotify.telegram.desc",
    "settings.remoteNotify.telegram.token",
    "settings.remoteNotify.telegram.tokenHint",
    "settings.remoteNotify.title",
    "settings.remoteNotify.webhook.body",
    "settings.remoteNotify.webhook.desc",
    "settings.remoteNotify.webhook.url",
    "settings.remoteNotify.webhook.urlHint",
    "settings.shortcuts.aiTaskMarks",
    "settings.shortcuts.clipboard",
    "settings.shortcuts.closePane",
    "settings.shortcuts.closeTab",
    "settings.shortcuts.commandLibrary",
    "settings.shortcuts.copyDesc",
    "settings.shortcuts.copySelected",
    "settings.shortcuts.focusDown",
    "settings.shortcuts.focusLeft",
    "settings.shortcuts.focusRight",
    "settings.shortcuts.focusUp",
    "settings.shortcuts.global",
    "settings.shortcuts.invalidNeedsModifier",
    "settings.shortcuts.invalidReserved",
    "settings.shortcuts.invalidSystemKey",
    "settings.shortcuts.invalidTypesChar",
    "settings.shortcuts.jumpAttention",
    "settings.shortcuts.jumpNextAi",
    "settings.shortcuts.jumpPrevAi",
    "settings.shortcuts.lineStartEnd",
    "settings.shortcuts.moveTabLeft",
    "settings.shortcuts.moveTabRight",
    "settings.shortcuts.navigation",
    "settings.shortcuts.newTerminal",
    "settings.shortcuts.nextPane",
    "settings.shortcuts.openSettings",
    "settings.shortcuts.pasteToTerminal",
    "settings.shortcuts.prevPane",
    "settings.shortcuts.quickOpen",
    "settings.shortcuts.readOnlyDigits",
    "settings.shortcuts.readOnlyTerminal",
    "settings.shortcuts.renamePane",
    "settings.shortcuts.reopenClosed",
    "settings.shortcuts.resizeDown",
    "settings.shortcuts.resizeLeft",
    "settings.shortcuts.resizeRight",
    "settings.shortcuts.resizeUp",
    "settings.shortcuts.scrollBottom",
    "settings.shortcuts.scrollPageDown",
    "settings.shortcuts.scrollPageUp",
    "settings.shortcuts.scrollTop",
    "settings.shortcuts.scrollbackZoom",
    "settings.shortcuts.selectPaneN",
    "settings.shortcuts.splitDown",
    "settings.shortcuts.splitRight",
    "settings.shortcuts.switchProject",
    "settings.shortcuts.tabsSplits",
    "settings.shortcuts.terminalOps",
    "settings.shortcuts.terminalSearch",
    "settings.shortcuts.toggleGlobalSearch",
    "settings.shortcuts.toggleMaximize",
    "settings.shortcuts.toggleSessions",
    "settings.shortcuts.toggleSidebar",
    "settings.shortcuts.toggleUsage",
    "settings.shortcuts.zoomIn",
    "settings.shortcuts.zoomOut",
    "settings.shortcuts.zoomReset",
    "settings.system.aiAutoResumeDesc",
    "settings.system.aiAutoResumeTitle",
    "settings.system.downloadDirectoryGroup",
    "settings.system.fpsBackgroundDesc",
    "settings.system.fpsBackgroundTitle",
    "settings.system.fpsForegroundDesc",
    "settings.system.fpsForegroundTitle",
    "settings.system.performanceGroup",
    "settings.system.startupGroup",
    "settings.system.trayClickFocusDesc",
    "settings.system.trayClickFocusTitle",
    "settings.system.trayGroup",
    "settings.system.trayMaxDesc",
    "settings.system.trayMaxTitle",
    "settings.system.trayStatusDesc",
    "settings.system.trayStatusTitle",
    "settings.terminal.availableTerminals",
    "settings.terminal.behavior",
    "settings.terminal.confirmCloseDesc",
    "settings.terminal.confirmCloseTitle",
    "settings.terminal.scrollback",
    "settings.terminal.scrollbackDesc",
    "settings.terminal.tabTitleFollowsShellDesc",
    "settings.terminal.tabTitleFollowsShellTitle",
    "sshModal.hostRequired",
    "sshModal.nameRequired",
    "sshModal.portInvalid",
    "sshModal.test.failedAuth",
    "sshModal.test.failedConfig",
    "sshModal.test.failedCooldown",
    "sshModal.test.failedHostKey",
    "sshModal.test.failedNetwork",
    "sshModal.test.failedOther",
    "sshModal.test.failedTimeout",
    "sshModal.userRequired",
    "terminalSearch.caseSensitive",
    "terminalSearch.close",
    "terminalSearch.next",
    "terminalSearch.noResults",
    "terminalSearch.placeholder",
    "terminalSearch.previous",
    "terminalSearch.regex",
    "terminalSearch.title",
    "terminalSearch.wholeWord",
    "usageStats.tokens.written",
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    /// 递归扫描 `crates/mt-app/src/**/*.rs`,提取所有字面量形式的
    /// `t("ns", "key")` 与 `tr!("ns", "key", ...)` 调用(含跨行排版)。
    fn scan_literal_i18n_keys(dir: &Path, out: &mut BTreeSet<String>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("读取源码目录失败 {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("读取目录项失败").path();
            if path.is_dir() {
                scan_literal_i18n_keys(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                let src = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("读取源码失败 {}: {e}", path.display()));
                extract_keys_from_source(&src, out);
            }
        }
    }

    fn extract_keys_from_source(src: &str, out: &mut BTreeSet<String>) {
        let mut stripped = String::with_capacity(src.len());
        for line in src.lines() {
            stripped.push_str(strip_line_comment(line));
            stripped.push('\n');
        }
        let bytes = stripped.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            if bytes[i] == b'"' {
                i = skip_string(bytes, i);
                continue;
            }
            let call_open = if bytes[i] == b't'
                && !is_ident_before(bytes, i)
                && i + 1 < len
                && bytes[i + 1] == b'('
            {
                Some(i + 2)
            } else if i + 3 < len && &bytes[i..i + 4] == b"tr!(" && !is_ident_before(bytes, i) {
                Some(i + 4)
            } else {
                None
            };
            let Some(mut p) = call_open else {
                i += 1;
                continue;
            };
            p = skip_ws(bytes, p);
            let Some((ns, next)) = parse_quoted_token(bytes, p, is_ns_char) else {
                i += 1;
                continue;
            };
            p = skip_ws(bytes, next);
            if p >= len || bytes[p] != b',' {
                i += 1;
                continue;
            }
            p = skip_ws(bytes, p + 1);
            let Some((key, after_key)) = parse_quoted_token(bytes, p, is_key_char) else {
                i += 1;
                continue;
            };
            out.insert(format!("{ns}.{key}"));
            i = after_key;
        }
    }

    fn strip_line_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut i = 0;
        while i < bytes.len() {
            if in_str {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if bytes[i] == b'"' {
                in_str = true;
                i += 1;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                return &line[..i];
            }
            i += 1;
        }
        line
    }

    fn is_ident_before(bytes: &[u8], pos: usize) -> bool {
        pos > 0 && (bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_')
    }

    fn is_ns_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn is_key_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
    }

    fn skip_ws(bytes: &[u8], mut p: usize) -> usize {
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        p
    }

    fn skip_string(bytes: &[u8], start: usize) -> usize {
        let mut p = start + 1;
        while p < bytes.len() {
            if bytes[p] == b'\\' {
                p += 2;
                continue;
            }
            if bytes[p] == b'"' {
                return p + 1;
            }
            p += 1;
        }
        p
    }

    fn parse_quoted_token(
        bytes: &[u8],
        start: usize,
        valid: fn(u8) -> bool,
    ) -> Option<(&str, usize)> {
        if start >= bytes.len() || bytes[start] != b'"' {
            return None;
        }
        let mut p = start + 1;
        let begin = p;
        while p < bytes.len() && bytes[p] != b'"' {
            if !valid(bytes[p]) {
                return None;
            }
            p += 1;
        }
        if p == begin || p >= bytes.len() {
            return None;
        }
        let text = std::str::from_utf8(&bytes[begin..p]).ok()?;
        Some((text, p + 1))
    }

    /// 每个用到的 key(源码自动扫描的字面量调用 + [`DYNAMIC_KEYS`])在 **zh 与 en 两侧都必须有条目**。
    ///
    /// 少了任何一边都是界面上的中英混排(英文缺条目会静默回落中文),
    /// 而这条在真机上要点开对应面板才看得见,必须在测试里挡下来。
    #[test]
    fn 用到的每个_key_两种语言都在() {
        let mut keys = BTreeSet::new();
        scan_literal_i18n_keys(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut keys,
        );
        assert!(
            keys.len() > 1000,
            "源码扫描到的 i18n key 过少({}),扫描器可能失效",
            keys.len()
        );
        for path in DYNAMIC_KEYS {
            keys.insert((*path).to_string());
        }
        let mut missing = Vec::new();
        for path in &keys {
            let (ns, key) = path
                .split_once('.')
                .unwrap_or_else(|| panic!("key 必须带命名空间:{path}"));
            for locale in Locale::ALL {
                if mt_i18n::lookup(locale, ns, key).is_none() {
                    missing.push(format!("{path}({locale})"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "字典缺条目({} 项):\n{}",
            missing.len(),
            missing.join("\n")
        );
    }

    /// 动态 key 清单要按字典序去重排好 —— 方便新增与比对。
    #[test]
    fn key_清单有序且不重复() {
        let mut sorted = DYNAMIC_KEYS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.as_slice(),
            DYNAMIC_KEYS,
            "DYNAMIC_KEYS 必须字典序且无重复"
        );
    }

    /// 配置里那个字符串说了算;认不出(或没有)才退到系统探测。
    /// 系统语言是环境相关的,这里只钉"不 panic 且落在两个合法值之一"。
    #[test]
    fn 配置取值优先于系统探测() {
        assert_eq!(Locale::from_code("en"), Some(Locale::En));
        assert_eq!(Locale::from_code("zh"), Some(Locale::Zh));
        assert_eq!(Locale::from_code("fr"), None, "认不出就该退到系统探测");
        assert!(Locale::ALL.contains(&system_locale()));
    }

    /// 桥给 gpui-component 的必须是 BCP 47,不是 `code()`——
    /// 它的 ui.yml 键是 `zh-CN`,传 `zh` 会静默回落英文。
    #[test]
    fn 桥接用的是_bcp47_不是语言码() {
        assert_eq!(Locale::Zh.bcp47(), "zh-CN");
        assert_eq!(Locale::En.bcp47(), "en");
        assert_ne!(Locale::Zh.bcp47(), Locale::Zh.code());
    }

    /// 查找条文案 key 与旧版 `src/i18n/locales/terminalSearch.ts` 逐条对齐 ——
    /// 打错一个 key 不会崩,只会在界面上显示成 key 本身,肉眼很难第一时间发现。
    /// (随文案注入自 mt-ui 的 `search_bar` 挪来,那边已不依赖 mt-i18n。)
    ///
    /// 这里**不动全局语言**(它是进程级的,并行测试会互相踩),用
    /// `t_in` 指定语言来验两侧;[`terminal_search_labels`] 走全局这条,逐字段
    /// 对照当前语言的 `t()`,钉住「哪个字段取哪个 key」的接线。
    #[test]
    fn 查找条文案_key_与旧版字典逐条对上() {
        use mt_i18n::t_in;
        let zh = |key: &'static str| t_in(Locale::Zh, "terminalSearch", key);
        let en = |key: &'static str| t_in(Locale::En, "terminalSearch", key);

        assert_eq!(zh("title"), "在终端中查找");
        assert_eq!(zh("placeholder"), "查找…");
        assert_eq!(zh("noResults"), "无结果");
        assert_eq!(zh("caseSensitive"), "区分大小写");
        assert_eq!(zh("wholeWord"), "全词匹配");
        assert_eq!(zh("regex"), "正则表达式");
        assert_eq!(zh("previous"), "上一个 (Shift+Enter)");
        assert_eq!(zh("next"), "下一个 (Enter)");
        assert_eq!(zh("close"), "关闭 (Esc)");

        assert_eq!(en("placeholder"), "Find…");
        assert_eq!(en("noResults"), "No results");
        assert_eq!(en("caseSensitive"), "Match case");
        assert_eq!(en("close"), "Close (Esc)");

        // 打错 key 在 debug 下会直接 panic(mt-i18n 的静态断言),
        // 所以上面这一堆同时也是「key 都存在」的证明。
        let labels = terminal_search_labels();
        let now = |key: &'static str| t("terminalSearch", key);
        assert_eq!(labels.title.as_ref(), now("title"));
        assert_eq!(labels.placeholder.as_ref(), now("placeholder"));
        assert_eq!(labels.no_results.as_ref(), now("noResults"));
        assert_eq!(labels.case_sensitive.as_ref(), now("caseSensitive"));
        assert_eq!(labels.whole_word.as_ref(), now("wholeWord"));
        assert_eq!(labels.regex.as_ref(), now("regex"));
        assert_eq!(labels.previous.as_ref(), now("previous"));
        assert_eq!(labels.next.as_ref(), now("next"));
        assert_eq!(labels.close.as_ref(), now("close"));
    }
}
