//! 字典一致性体检 —— 守住四条不变量，任何一条破了都是「界面上会看见」的 bug。
//!
//! 1. zh / en 的 key 集合完全一致（缺条目 = 界面中英混排）
//! 2. 同一 key 的 `{占位符}` 集合两语言一致（不一致 = 某语言下参数显示不出来）
//! 3. 没有空文案（空串 = 界面上一块空白，比显示 key 还难查）
//! 4. 条目总数与生成器当时数出来的对账常量相等（防后续增删漏改）
//!
//! 另外还验证生成器承诺的数据布局（key 有序、命名空间有序），
//! 因为 `t()` 的二分查找**依赖这个不变量**，一旦排序丢了会静默查不到。

use std::collections::{BTreeMap, BTreeSet};

use mt_i18n::{
    Locale, Namespace, dict, interpolate, lookup, namespace, namespaces, t_args_in, t_in, t_path,
};

/// TS 侧 `crates/mt-i18n/locales/*.ts` 数出来的对账数字（生成器 2026-08-29 跑出）：
/// 32 个命名空间文件（`locales/index.ts` 里 32 条 import 一一对上），
/// 每种语言 840 条叶子文案。改字典后重跑生成器，这里的数字随 dict.rs 一起更新。
///
/// 727 → 735：M 批(mt-app 消费批)补齐 GPUI 侧 8 条缺 key 文案
/// （`paneGroup.shellExited` / `settings.terminal.fontSizeNewOnly` /
/// `projectList.{pathPlaceholder,pathHint,chooseDirDialogTitle}` /
/// `usageStats.{byTool,byShell,pricingLocalHint}`）。
///
/// 735 → 741：R 批(设置面板 10 分页)补 6 条 GPUI 专属文案 ——
/// 快捷键页那三条 GPUI 独有动作的描述
/// （`settings.shortcuts.{toggleSessions,toggleUsage,jumpAttention}`）、
/// 两条「底层暂未实现」说明（`settings.appearance.skinUnavailable` /
/// `settings.font.ligaturesUnavailable`），以及自定义提示音只认 .wav 的提示
/// （`settings.aiNotification.wavOnly`）。
///
/// 741 → 743：pane 拖拽批（对齐原版 v0.14.0 / PR #49）补最大化按钮的两条 tooltip
/// （`paneGroup.{maximizePane,restorePane}`），文案与原版 `paneGroup.ts` 一字不差。
///
/// 743 → 744：剪贴板图片粘贴补回（GPUI 迁移期整块缺失）时新增
/// `terminal.pasteImageNoRemote` —— 远程 pane 断链时图片既没有原文可退、
/// `Alt+V` 也对远端无效，只能提示，故要一条专属文案。
/// 754 → 758：diff 弹窗补「上一处/下一处改动」跳转按钮的 tooltip
/// （`diffModal.{prevChange,nextChange}` 与 `commitDiff.{prevChange,nextChange}`，
/// 两个弹窗各用各的命名空间，与既有的 `sideBySide`/`inline` 同一口径）。
///
/// 758 → 760：HTML 预览补回（原先只有源码态）时新增
/// `fileViewer.{openInBrowser,htmlPreviewNote}` —— 内嵌的是富文本简版渲染
/// （无 CSS / 无 JS），得有一句说明与一条去浏览器看真效果的出口。
///
/// 760 → 761：AI 任务标记补「还没定位」态时新增 `markerList.pendingAnchor` ——
/// AI 忙时追加的那句是排进队列的，属于它的消息还没上屏就没有锚点可定，
/// 条目先挂着（灰的、点不动），得有一句说明它为什么跳不了。
///
/// 761 → 762：最大化改成「其余组折成标题条码在底部」时新增
/// `paneGroup.collapsedHint` —— 折叠条整条都是热区，得有一句说明点它会发生什么
/// （原先其余组整个不画，压根没有这个交互）。
///
/// 762 → 767：项目级终端面板（一个项目多个独立终端工作面 + 右缘图标竖条）
/// 新增 `app.activityBar.terminals`（边条开关的 tooltip）与
/// `terminalArea.{panelN,newPanel,renamePanel,closePanel}`（序号名/新建/右键两项）。
///
/// 767 → 769：设置页新增「启用动画」总开关（终端区换场动画），落在终端页
/// 行为组：`settings.terminal.{animationsTitle,animationsDesc}`。
///
/// 769 → 771：会话正文预览改用可选中的富文本渲染后，选区跨不了单条消息，
/// 补两条兜底动作的文案：`sessionViewer.{copyMessage,copyAll}`。
///
/// 771 → 776：项目类型从 12 种扩到 51 种后，「手动指定类型」的菜单改成按类别的
/// 二级子菜单，补五个分组标题：`projectList.menu.kindCategory.*`。
///
/// 776 → 771：外观页的「皮肤」单选段整段移除（GPUI 侧从来没有内置皮肤色表，
/// 那一栏只有「无」能点、blueprint / fluent2 长期置灰），随之删掉
/// `settings.appearance.{skin,skinNone,skinBlueprint,skinDesc,skinUnavailable}`。
/// 皮肤只剩默认（主题段）与外置（`settings.themes.*`）两档。
///
/// 771 → 824：远程文件管理补齐复制/粘贴、上传/下载、冲突选择、操作状态，
/// 新增远程目录选择器、递归粘贴保护，并在系统设置加入下载目录偏好。
/// 824 → 825：冲突弹窗列出具体名称，长列表补 `fileTree.conflict.remaining`。
/// 825 → 831：文件工作区新增终端页签名及 7 条远程查看/保存文案；同时删除旧的
/// 2 条“不支持远程预览”，净增 6 条。
/// 831 → 835：关窗确认补未保存文件、“未保存文件 + 运行中 AI”两种风险提示，
/// 并为长列表增加剩余项计数。
/// 835 → 836：项目/worktree 移除遇到未保存页签时补明确的阻止提示。
/// 836 → 839：PR #56 审核整改补远程图片点击加载、远程搜索不支持与下载上下文
/// 失效三条可见反馈。
/// 839 → 840：远程文档刷新失败但保留已加载内容时补非阻断警告。
/// 840 → 842：新建终端菜单接入 AI 启动器段（分组标题 + 「管理启动器…」入口）。
const EXPECTED_NAMESPACES: usize = 32;
const EXPECTED_ENTRIES_PER_LANG: usize = 842;

/// TS 侧 `locales/index.ts` 收编的全部命名空间，手抄一份放这里做交叉验证 ——
/// 只信生成器的话，「某个 ns 文件整体没被读到」这种错会一起漏过去。
const TS_NAMESPACES: &[&str] = &[
    "app",
    "commitDiff",
    "diffModal",
    "envVars",
    "externalLink",
    "fileTree",
    "fileViewer",
    "gitChanges",
    "gitHistory",
    "gitHistoryContent",
    "markerList",
    "mobileRelay",
    "paneGroup",
    "panels",
    "projectList",
    "projectSwitcher",
    "prompt",
    "remoteProject",
    "search",
    "sessionList",
    "sessionViewer",
    "settings",
    "sshAssoc",
    "sshModal",
    "terminal",
    "terminalArea",
    "terminalSearch",
    "time",
    "toast",
    "updateChecker",
    "usageStats",
    "worktree",
];

fn keys(ns: &Namespace, locale: Locale) -> BTreeSet<&'static str> {
    ns.entries(locale).iter().map(|(k, _)| *k).collect()
}

/// 取 `{name}` 占位符集合，正则等价于 TS 侧 store.ts 的 `/\{(\w+)\}/g`
fn placeholders(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(rel) = s[i + 1..].find('}')
        {
            let name = &s[i + 1..i + 1 + rel];
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                out.insert(name.to_string());
                i += rel + 2;
                continue;
            }
        }
        i += s[i..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
    out
}

// ---------------------------------------------------------------------------
// 1. 规模对账
// ---------------------------------------------------------------------------

#[test]
fn namespace_and_entry_counts_match_ts_side() {
    assert_eq!(
        namespaces().len(),
        EXPECTED_NAMESPACES,
        "命名空间数量与 TS 侧对不上：改了字典就重跑 tools/gen_from_ts.mjs 并更新本常量"
    );
    assert_eq!(dict::NAMESPACE_COUNT, EXPECTED_NAMESPACES);
    assert_eq!(dict::ZH_ENTRY_COUNT, EXPECTED_ENTRIES_PER_LANG);
    assert_eq!(dict::EN_ENTRY_COUNT, EXPECTED_ENTRIES_PER_LANG);

    let zh: usize = namespaces().iter().map(|n| n.zh.len()).sum();
    let en: usize = namespaces().iter().map(|n| n.en.len()).sum();
    assert_eq!(zh, EXPECTED_ENTRIES_PER_LANG, "中文条目总数漂移");
    assert_eq!(en, EXPECTED_ENTRIES_PER_LANG, "英文条目总数漂移");
}

#[test]
fn namespace_names_match_ts_index() {
    let actual: BTreeSet<&str> = namespaces().iter().map(|n| n.name).collect();
    let expected: BTreeSet<&str> = TS_NAMESPACES.iter().copied().collect();
    let missing: Vec<_> = expected.difference(&actual).collect();
    let extra: Vec<_> = actual.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "命名空间与 TS 侧 locales/index.ts 对不上 —— 缺失 {missing:?}，多出 {extra:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. 两语言一致性
// ---------------------------------------------------------------------------

#[test]
fn zh_and_en_have_identical_key_sets() {
    let mut gaps: Vec<String> = Vec::new();
    for ns in namespaces() {
        let zh = keys(ns, Locale::Zh);
        let en = keys(ns, Locale::En);
        for k in zh.difference(&en) {
            gaps.push(format!("{}.{k}  en 缺失", ns.name));
        }
        for k in en.difference(&zh) {
            gaps.push(format!("{}.{k}  zh 缺失", ns.name));
        }
    }
    assert!(
        gaps.is_empty(),
        "两语言 key 集合不一致（{} 处）：\n{}",
        gaps.len(),
        gaps.join("\n")
    );
}

#[test]
fn placeholders_agree_across_languages() {
    let mut bad: Vec<String> = Vec::new();
    for ns in namespaces() {
        let en: BTreeMap<&str, &str> = ns.en.iter().copied().collect();
        for (k, zh_text) in ns.zh {
            let Some(en_text) = en.get(k) else { continue };
            let a = placeholders(zh_text);
            let b = placeholders(en_text);
            if a != b {
                bad.push(format!("{}.{k}  zh={a:?} en={b:?}", ns.name));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "占位符两语言不一致（{} 处）：\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn no_empty_messages() {
    let mut empty: Vec<String> = Vec::new();
    for ns in namespaces() {
        for locale in Locale::ALL {
            for (k, v) in ns.entries(locale) {
                if v.trim().is_empty() {
                    empty.push(format!("{} {}.{k}", locale.code(), ns.name));
                }
            }
        }
    }
    assert!(empty.is_empty(), "存在空文案：\n{}", empty.join("\n"));
}

/// 占位符名必须是 `\w+`，否则 [`mt_i18n::interpolate`] 认不出来会原样显示。
/// 顺带确认字典里没有 `{}`、`{ name }` 这类写不进插值的形态。
#[test]
fn every_brace_in_dictionary_is_a_valid_placeholder() {
    let mut bad: Vec<String> = Vec::new();
    for ns in namespaces() {
        for locale in Locale::ALL {
            for (k, v) in ns.entries(locale) {
                let braces = v.matches('{').count();
                if braces != placeholders(v).len() && braces > 0 {
                    // 允许同名占位符重复出现，只有「花括号数 > 去重后占位符数」
                    // 且确实存在解析不出来的花括号时才算问题
                    let mut recovered = 0usize;
                    for name in placeholders(v) {
                        recovered += v.matches(&format!("{{{name}}}")).count();
                    }
                    if recovered != braces {
                        bad.push(format!("{} {}.{k}: {v:?}", locale.code(), ns.name));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "字典里有解析不出的花括号：\n{}",
        bad.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 3. 数据布局（t() 的二分查找依赖它）
// ---------------------------------------------------------------------------

#[test]
fn tables_are_sorted_for_binary_search() {
    assert!(
        namespaces().windows(2).all(|w| w[0].name < w[1].name),
        "命名空间未按 name 升序，namespace() 的二分会查不到"
    );
    for ns in namespaces() {
        for locale in Locale::ALL {
            let table = ns.entries(locale);
            assert!(
                table.windows(2).all(|w| w[0].0 < w[1].0),
                "{} / {} 的 key 未升序或有重复，Namespace::get 的二分会查不到",
                ns.name,
                locale.code()
            );
        }
    }
}

/// 逐条走一遍公共 API，确认每个 key 在两种语言下都真能查出来 ——
/// 排序断言是间接证明，这个是直接证明。
#[test]
fn every_key_is_reachable_through_public_api() {
    for ns in namespaces() {
        for locale in Locale::ALL {
            for (k, v) in ns.entries(locale) {
                assert_eq!(
                    lookup(locale, ns.name, k),
                    Some(*v),
                    "{}.{k} 查不到",
                    ns.name
                );
                assert_eq!(t_in(locale, ns.name, k), *v);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. 抽样验收：几条有代表性的真实文案
// ---------------------------------------------------------------------------

#[test]
fn sampled_messages_survived_the_conversion() {
    // 普通条目
    assert_eq!(t_in(Locale::Zh, "app", "menu.settings"), "设置");
    assert_eq!(t_in(Locale::En, "app", "menu.settings"), "Settings");
    // 三层嵌套 key
    assert_eq!(
        t_in(Locale::En, "app", "titleBar.status.idle"),
        "No AI sessions running"
    );
    // 含换行与多占位符的长文案（TS 里写的是 \n 转义，转换后必须仍是真换行）
    let zh = t_in(Locale::Zh, "app", "closeConfirm.messageWithSessions");
    assert!(zh.contains('\n'), "换行没保住：{zh:?}");
    assert!(zh.contains("{count}") && zh.contains("{names}"));
    // 含反引号与半角引号的条目（Rust 字面量转义正确性）
    assert!(t_in(Locale::Zh, "envVars", "error.reservedWslenv").starts_with('`'));
    assert!(t_in(Locale::Zh, "fileTree", "dialog.deleteConfirmFile").contains('"'));
}

#[test]
fn interpolation_end_to_end() {
    assert_eq!(
        t_args_in(Locale::Zh, "time", "minutesAgo", &[("n", "5")]),
        "5 分钟前"
    );
    assert_eq!(
        t_args_in(Locale::En, "time", "minutesAgo", &[("n", "5")]),
        "5 min ago"
    );
    let s = t_args_in(
        Locale::Zh,
        "app",
        "closeConfirm.messageWithSessions",
        &[("count", "2"), ("names", "claude\ncodex")],
    );
    assert!(s.contains("还有 2 个 AI 会话"));
    assert!(s.contains("claude\ncodex"));
    assert!(!s.contains('{'), "占位符没换干净：{s:?}");
}

#[test]
fn path_form_matches_two_arg_form() {
    // t_path 走全局语言，这里显式对齐：只断言两种写法结果相同即可，
    // 不假设当前全局语言是什么（避免与其它并行用例互踩）。
    assert_eq!(
        t_path("app.menu.settings"),
        t_in(mt_i18n::locale(), "app", "menu.settings")
    );
    assert_eq!(namespace("app").map(|n| n.name), Some("app"));
    assert!(namespace("nope").is_none());
}

/// 回落链：英文缺条目回落中文（release 语义），未知 key 原样返回。
/// 这两条在 debug 下会触发 `debug_assert`，所以只在 release 测试里跑。
#[test]
#[cfg(not(debug_assertions))]
fn fallback_chain_in_release() {
    assert_eq!(t_in(Locale::En, "app", "no_such_key"), "no_such_key");
    assert_eq!(t_in(Locale::Zh, "no_such_ns", "k"), "k");
}

/// 插值不改动不含占位符的文案，逐条验证（防止插值实现把某些字符吃掉）
#[test]
fn interpolate_is_identity_without_args() {
    for ns in namespaces() {
        for locale in Locale::ALL {
            for (_, v) in ns.entries(locale) {
                assert_eq!(&interpolate(v, &[]), v);
                // 给一个不匹配任何占位符的参数，文案也必须原样输出
                assert_eq!(&interpolate(v, &[("__nope__", "x")]), v);
            }
        }
    }
}
