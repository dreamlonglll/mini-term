//! 文档的读写状态机与口径(纯逻辑,可测)。
//!
//! [`DocumentSession`] 管一份打开的文档的全部读写状态位:载入代次、磁盘基线的
//! 编辑器投影、脏态、保存、外部改动、远程基线与冲突、连接身份失效。视图
//! ([`super::FileViewer`])只负责发起 IO(读盘 / 写盘 / SFTP / 目录监听)、把结果
//! 喂进来、按这里的结论建编辑器与重画 —— 状态位怎么翻全在这里,单测逐条锁住。
//! 转换规则是从视图的异步闭包里原样抽出来的,一条未改。
//!
//! # 状态转换
//!
//! 「代次」即 `load_generation`:每次载入 / 远程刷新 +1,保存时若有远程刷新在飞
//! 也 +1 作废它;异步结果回来时代次对不上就整条丢弃。
//!
//! | 入口 | 前提 | 结果 |
//! |---|---|---|
//! | [`begin_load`](DocumentSession::begin_load) | 保存中 | 不动(旧写入还没收口,不许重建) |
//! | | 看图 | 代次 +1;清远程刷新中 / 刷新警告 / 远程基线与冲突;`loading = false`、清内容(`error` 不动) |
//! | | 其余 | 同上,但 `loading = true`,清 `error` 与内容;交出代次去读 |
//! | [`settle_load`](DocumentSession::settle_load) | 代次不符 | 丢弃 |
//! | | 相符 | `loading = false`,随后 [`fail_load`](DocumentSession::fail_load) 或落内容 |
//! | [`apply_local_content`](DocumentSession::apply_local_content) | — | 清远程基线与冲突 → 落基线 |
//! | [`apply_remote_content`](DocumentSession::apply_remote_content) | 连接身份失效 | 从没载入过内容才报 `error`(连接已变);不落 |
//! | | 否则 | 换远程基线、清冲突 → 落基线 |
//! | 落基线 | — | 探测行尾、归一后展开 Tab;`saved = disk = 投影`;清脏 / 外部改动 / 保存错误与警告 / 刷新警告;记下内容 |
//! | [`on_edit`](DocumentSession::on_edit) | — | `dirty = 编辑器值 != saved` |
//! | [`should_refresh_on_activation`](DocumentSession::should_refresh_on_activation) | 远程、非图、身份有效、不在载入 / 刷新 / 保存、不脏 | 后台重读一次 |
//! | [`begin_remote_refresh`](DocumentSession::begin_remote_refresh) | — | 代次 +1;刷新中;清刷新警告与 `error` |
//! | [`settle_remote_refresh`](DocumentSession::settle_remote_refresh) | 代次不符 | 丢弃 |
//! | | 相符 | 刷新中 = false(视图随后复核连接身份,失效就停在这) |
//! | [`on_refresh_loaded`](DocumentSession::on_refresh_loaded) | 有编辑器、可编辑、行尾与投影都没变 | 只换基线,清冲突与 `error`,编辑器实体原样留着 |
//! | | 脏(刷新途中开始打字) | 挂远程冲突,草稿保住 |
//! | | 其余 | 按新内容重落基线,上次保存的清理警告留着 |
//! | [`on_refresh_failed`](DocumentSession::on_refresh_failed) | 已有内容或编辑器 | 刷新警告,清 `error` |
//! | | 否则 | 致命:`error`,清刷新警告 |
//! | [`prepare_save`](DocumentSession::prepare_save) | 保存中 / 草稿与 `saved` 相同 | 不存 |
//! | | 远程刷新在飞 | 代次 +1 作废它 |
//! | [`begin_save`](DocumentSession::begin_save) | 连接身份失效 | 不存 |
//! | | 否则 | `saving = true`;清保存错误 / 警告与远程冲突;交出代次与落盘文本(先还原 Tab 再还原行尾) |
//! | [`abort_save_read_only`](DocumentSession::abort_save_read_only) | 远程没有基线 | 撤销保存中,报只读 |
//! | [`abort_save_source_invalid`](DocumentSession::abort_save_source_invalid) | 取不到当前连接 | 撤销保存中,身份失效 |
//! | [`settle_save`](DocumentSession::settle_save) | 代次不符 | 丢弃(`saving` 不动) |
//! | | 相符 | `saving = false` |
//! | [`on_local_save_result`](DocumentSession::on_local_save_result) | 成功 / 失败 | 收尾 / `save_error` |
//! | [`on_remote_save_result`](DocumentSession::on_remote_save_result) | — | 只有写成功才清刷新警告 |
//! | | 写成功 | 收尾(换新基线,带上清理警告) |
//! | | 远端已变 | 挂远程冲突 |
//! | | 失败 | `save_error` |
//! | 收尾 | — | 按写回文本重算 Tab 映射;`saved = disk = 文本`;记保存时刻;新基线(没有就沿用);清冲突;`save_warning = 清理警告`;按最新草稿重算脏;清外部改动 |
//! | [`on_fs_change`](DocumentSession::on_fs_change) | 还没内容 / 保存后 [`ECHO_WINDOW`] 内 | 不理 |
//! | | 草稿与 `saved` 不同,或保存中 | 挂外部改动提示条 |
//! | | 其余 | 视图静默重载 |
//!
//! 看图页签的外部改动不进这里:视图直接走换代去抖
//! ([`super::FileViewer::schedule_image_reload`]),与文档状态无关。
//!
//! [`LineEnding`] / [`normalize_to_lf`] / [`restore_line_ending`] 经 `file_viewer`
//! 再导出,对外路径不变(`tab_expansion` 的单测在用)。

use std::time::{Duration, Instant};

use mt_project::fs::FileContentResult;
use mt_remote::{RemoteFileBaseline, RemoteFileReadResult, RemoteFileSaveResult};

use crate::i18n::t;
use crate::tab_expansion::{TAB_WIDTH, TabExpansion};

/// 文件行尾。读入时探测,写回时还原。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// 探测。**只要出现过一次 `\r\n` 就整份按 CRLF 处理** —— 与原版
    /// `const crlf = value.includes('\r\n')`(`CodeEditor.tsx:246`)一字不差。
    ///
    /// 混合行尾的文件因此会在保存时被收敛成 CRLF。原版在这一点上略有不同:
    /// CodeMirror 设了 `lineSeparator` 之后,孤立的 `\n` 会以控制字符留在行内容里、
    /// `doc.toString()` 原样吐回去(注释里写的「恰好暴露混合行尾」)。GPUI 侧的
    /// 编辑器没有 lineSeparator 这个概念,孤立 `\n` 只能当换行看 —— 于是保存后统一。
    /// 这是**刻意取舍**:混合行尾文件本就是坏味道,统一比留着更符合直觉,
    /// 而「一行都别动」的目标(纯 CRLF 文件保存后仍是纯 CRLF)照样达成。
    pub fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            Self::Crlf
        } else {
            Self::Lf
        }
    }
}

/// 磁盘内容 → 编辑器内容:`\r\n` 折成 `\n`。
///
/// 不归一直接喂进去也能显示(ropey 认 `\r\n` 是一个换行),但**新敲的回车是 `\n`**,
/// 于是同一份文件里两种行尾并存,还原时无从下手。归一之后「编辑器里只有 `\n`」
/// 是不变式,[`restore_line_ending`] 才能无歧义地还原。
pub fn normalize_to_lf(text: &str) -> String {
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

/// 编辑器内容 → 磁盘内容:按探测到的行尾还原。
///
/// 先把可能混进来的 `\r\n` 折掉再统一加 `\r`,是为了幂等 —— 免得对同一份文本
/// 调两次变成 `\r\r\n`(编辑器里理论上不该有 `\r\n`,但这条不值得赌)。
pub fn restore_line_ending(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::Crlf => normalize_to_lf(text).replace('\n', "\r\n"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RemoteRefreshFailurePresentation {
    Fatal,
    Warning,
}

fn remote_refresh_failure_presentation(
    has_loaded_result: bool,
    has_editor: bool,
) -> RemoteRefreshFailurePresentation {
    if has_loaded_result || has_editor {
        RemoteRefreshFailurePresentation::Warning
    } else {
        RemoteRefreshFailurePresentation::Fatal
    }
}

fn refresh_warning_after_remote_save(
    current: Option<String>,
    save_succeeded: bool,
) -> Option<String> {
    if save_succeeded { None } else { current }
}

/// 自己落盘的回声窗口:保存后 2s 内的 `fs-change` 不算「外部修改」
/// (`FileViewerModal.tsx:280`)。
///
/// 已知边界(原版就有,照抄不改):这 2s 内**真正的**外部修改也会被吞掉
/// (保存后立刻被 formatter / pre-commit 改写)。不改成内容比对 ——
/// 那会引入「外部改写结果恰好等于刚保存的内容」的另一类误判。
const ECHO_WINDOW: Duration = Duration::from_millis(2000);

// ─── 远程内容的接缝 ─────────────────────────────────────────────

/// 远程读回来的一份内容。真类型是 [`RemoteFileReadResult`];做成 trait 只为单测
/// 能造假基线 —— 真基线([`RemoteFileBaseline`])的字段出了 `mt_remote` 就不可见,
/// 这是它防「UI 伪造基线」的刻意设计,不为测试破例。
pub(super) trait RemoteContent {
    type Baseline: Clone;
    fn content(&self) -> &FileContentResult;
    fn into_parts(self) -> (FileContentResult, Option<Self::Baseline>);
}

impl RemoteContent for RemoteFileReadResult {
    type Baseline = RemoteFileBaseline;

    fn content(&self) -> &FileContentResult {
        &self.content
    }

    fn into_parts(self) -> (FileContentResult, Option<RemoteFileBaseline>) {
        (self.content, self.baseline)
    }
}

/// 远程保存「写成功 / 远端已变」两种结论,与 [`RemoteFileSaveResult`] 同形
/// (基线类型做成参数的理由同 [`RemoteContent`])。
pub(super) enum RemoteSave<R: RemoteContent> {
    Saved {
        baseline: R::Baseline,
        warning: Option<String>,
    },
    ExternalChange {
        current: R,
    },
}

impl From<RemoteFileSaveResult> for RemoteSave<RemoteFileReadResult> {
    fn from(result: RemoteFileSaveResult) -> Self {
        match result {
            RemoteFileSaveResult::Saved { baseline, warning } => Self::Saved { baseline, warning },
            RemoteFileSaveResult::ExternalChange { current } => Self::ExternalChange { current },
        }
    }
}

// ─── 状态机 ───────────────────────────────────────────────────

/// [`DocumentSession::begin_load`] 的结论:这次载入走哪条路。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LoadStart {
    /// 看图页签:不读盘,图由资源系统自己读
    Image,
    /// 去读盘 / 拉远程,结果回来时拿这个代次对号([`DocumentSession::settle_load`])
    Read { generation: u64 },
}

/// [`DocumentSession::begin_save`] 交出的保存起点。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SaveStart {
    /// 结果回来时拿它对号([`DocumentSession::settle_save`])
    pub(super) generation: u64,
    /// 已还原 Tab 与行尾的落盘文本
    pub(super) on_disk: String,
}

/// [`DocumentSession::on_refresh_loaded`] 的结论。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefreshLoaded {
    /// 远端与编辑器基线一致:只换了基线,编辑器实体(光标 / 撤销栈)原样留着
    Unchanged,
    /// 刷新途中用户开始打字:挂了远程冲突,草稿保住
    Conflict,
    /// 已按新内容落了基线,视图拿这份编辑器文本重建编辑器;`None` = 连接身份
    /// 已失效、没落(视图在调用前已复核过身份,实际走不到)
    Replaced(Option<String>),
}

/// [`DocumentSession::on_fs_change`] 的结论。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FsChange {
    /// 还没内容 / 自己落盘的回声:不理
    Ignore,
    /// 脏或正在保存:已挂外部改动提示条,视图重画即可
    Flagged,
    /// 干净:视图静默重载跟上磁盘
    Reload,
}

/// 一份打开的文档的读写状态。转换规则见模块注释的状态转换表。
///
/// 基线类型参数 `R` 只为单测(理由见 [`RemoteContent`]),视图用的是默认的
/// [`RemoteFileReadResult`]。
pub(super) struct DocumentSession<R: RemoteContent = RemoteFileReadResult> {
    loading: bool,
    remote_refreshing: bool,
    error: Option<String>,
    /// A re-activation refresh failed after usable content was already loaded.
    /// Keep it separate from `error`: the latter selects the full-page fatal
    /// branch, while this warning must leave the editor/draft visible.
    refresh_warning: Option<String>,
    result: Option<FileContentResult>,
    /// 磁盘上最后一次已知内容的**编辑器投影**(已归一成 `\n`、Tab 已展开)。
    /// 载入 / 保存成功时更新。
    saved: String,
    /// 磁盘现内容的投影(Markdown 预览渲染用它,不用 `result.content` ——
    /// 后者是「打开时」的内容,保存后就旧了)。与 [`Self::saved`] 同一口径。
    disk: String,
    /// 文件读进来时的行尾。写回时按它还原(见 `file_viewer` 模块注释)。
    line_ending: LineEnding,
    /// 文件读进来时的 Tab 展开记录。写回时按它还原(见 `file_viewer` 模块注释
    /// 「Tab」一节)。
    tabs: TabExpansion,
    dirty: bool,
    saving: bool,
    save_error: Option<String>,
    save_warning: Option<String>,
    ext_changed: bool,
    last_save_at: Option<Instant>,
    /// 远程可编辑文件的加载/上次保存基线。二进制、超限和失败分支为 `None`。
    remote_baseline: Option<R::Baseline>,
    /// 保存前发现远端已变化。保留后端返回的新内容，让“重新加载”无需第二次网络请求。
    remote_conflict: Option<R>,
    /// 当前配置中的 SSH 连接身份已与打开页签时不同；此页签只允许查看，不允许保存。
    remote_source_invalid: bool,
    load_generation: u64,
}

impl<R: RemoteContent> DocumentSession<R> {
    pub(super) fn new() -> Self {
        Self {
            loading: false,
            remote_refreshing: false,
            error: None,
            refresh_warning: None,
            result: None,
            saved: String::new(),
            disk: String::new(),
            line_ending: LineEnding::Lf,
            tabs: TabExpansion::default(),
            dirty: false,
            saving: false,
            save_error: None,
            save_warning: None,
            ext_changed: false,
            last_save_at: None,
            remote_baseline: None,
            remote_conflict: None,
            remote_source_invalid: false,
            load_generation: 0,
        }
    }

    // ── 读数(渲染与视图判定用) ─────────────────────────────

    pub(super) fn loading(&self) -> bool {
        self.loading
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn refresh_warning(&self) -> Option<&str> {
        self.refresh_warning.as_deref()
    }

    pub(super) fn result(&self) -> Option<&FileContentResult> {
        self.result.as_ref()
    }

    pub(super) fn saved(&self) -> &str {
        &self.saved
    }

    pub(super) fn disk(&self) -> &str {
        &self.disk
    }

    /// 文件以 Tab 缩进(编辑器的 Tab 键按它决定一次缩几格,见 [`TabExpansion`])。
    pub(super) fn indents_with_tabs(&self) -> bool {
        self.tabs.indents_with_tabs()
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn is_saving(&self) -> bool {
        self.saving
    }

    pub(super) fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub(super) fn save_warning(&self) -> Option<&str> {
        self.save_warning.as_deref()
    }

    pub(super) fn ext_changed(&self) -> bool {
        self.ext_changed
    }

    pub(super) fn remote_source_invalid(&self) -> bool {
        self.remote_source_invalid
    }

    pub(super) fn has_remote_conflict(&self) -> bool {
        self.remote_conflict.is_some()
    }

    pub(super) fn remote_baseline(&self) -> Option<&R::Baseline> {
        self.remote_baseline.as_ref()
    }

    /// 当前草稿:编辑器全文(`\n` 行尾、Tab 已展开);没有编辑器(`None`)时就是
    /// 磁盘内容的投影。
    pub(super) fn draft(&self, editor_text: Option<String>) -> String {
        editor_text.unwrap_or_else(|| self.saved.clone())
    }

    /// 编辑器文本 → 落盘形态:先还原 Tab、再还原行尾(与读入时相反的顺序,见
    /// `file_viewer` 模块注释)。只算不写,保存与 Git 注解(`blame --contents`)共用。
    pub(super) fn to_disk_form(&self, text: &str) -> String {
        restore_line_ending(&self.tabs.restore(text), self.line_ending)
    }

    // ── 载入 ───────────────────────────────────────────────

    /// 发起一次(重新)载入。`None` = 保存还在进行中,这次不载。
    pub(super) fn begin_load(&mut self, is_img: bool) -> Option<LoadStart> {
        // 保存任务已经拿到旧基线并可能正在落盘。此时重建编辑器会让迟到的保存
        // 完成跨代修改状态，也会允许用户在旧写入尚未结束时启动第二次保存。
        if self.saving {
            return None;
        }
        self.remote_refreshing = false;
        self.refresh_warning = None;
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        self.remote_conflict = None;
        self.remote_baseline = None;
        if is_img {
            self.loading = false;
            self.result = None;
            return Some(LoadStart::Image);
        }
        self.loading = true;
        self.error = None;
        self.result = None;
        Some(LoadStart::Read { generation })
    }

    /// 读盘 / 拉远程的结果回来了。代次对不上(已被更新的载入 / 刷新 / 保存作废)
    /// 返回 `false`,整条丢弃;对上了就结束载入态。
    pub(super) fn settle_load(&mut self, generation: u64) -> bool {
        if self.load_generation != generation {
            return false;
        }
        self.loading = false;
        true
    }

    pub(super) fn fail_load(&mut self, error: String) {
        self.error = Some(error);
    }

    /// 本地内容到位:落基线,返回给编辑器的文本。
    pub(super) fn apply_local_content(&mut self, res: FileContentResult) -> String {
        self.remote_baseline = None;
        self.remote_conflict = None;
        self.apply_file_content(res)
    }

    /// 远程内容到位(连接身份须已复核过):落基线,返回给编辑器的文本;身份失效
    /// 时不落,返回 `None`。
    pub(super) fn apply_remote_content(&mut self, content: R) -> Option<String> {
        if self.remote_source_invalid {
            if self.result.is_none() {
                self.error = Some(t("fileViewer", "remoteConnectionChanged").to_string());
            }
            return None;
        }
        let (content, baseline) = content.into_parts();
        self.remote_baseline = baseline;
        self.remote_conflict = None;
        Some(self.apply_file_content(content))
    }

    /// 落基线:「编辑基线与内容一起落位」(`FileViewerModal.tsx:224`),返回编辑器
    /// 该装的文本(已归一行尾、Tab 已展开)。
    fn apply_file_content(&mut self, res: FileContentResult) -> String {
        self.line_ending = LineEnding::detect(&res.content);
        // 先归一行尾再展开 Tab(见 `file_viewer` 模块注释「Tab」一节)
        let (text, tabs) = TabExpansion::expand(&normalize_to_lf(&res.content), TAB_WIDTH);
        self.tabs = tabs;
        self.saved = text.clone();
        self.disk = text.clone();
        self.dirty = false;
        self.ext_changed = false;
        self.save_error = None;
        self.save_warning = None;
        self.refresh_warning = None;
        self.result = Some(res);
        text
    }

    // ── 编辑 ───────────────────────────────────────────────

    /// 编辑器内容变了(原版 `onDocChange` → `setDirty(doc !== savedRef)`)。
    pub(super) fn on_edit(&mut self, value: &str) {
        self.dirty = value != self.saved;
    }

    // ── 远程:激活刷新与连接身份 ─────────────────────────────

    /// 页签重新激活时要不要后台重读一次:只有干净、空闲、身份有效的远程文本文档。
    pub(super) fn should_refresh_on_activation(&self, is_remote: bool, is_img: bool) -> bool {
        is_remote
            && !is_img
            && !self.remote_source_invalid
            && !self.loading
            && !self.remote_refreshing
            && !self.saving
            && !self.dirty
    }

    /// 发起激活刷新,返回这次的代次。
    pub(super) fn begin_remote_refresh(&mut self) -> u64 {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.remote_refreshing = true;
        self.refresh_warning = None;
        self.error = None;
        self.load_generation
    }

    /// 刷新结果回来了。代次对不上返回 `false`,整条丢弃。
    pub(super) fn settle_remote_refresh(&mut self, generation: u64) -> bool {
        if self.load_generation != generation {
            return false;
        }
        self.remote_refreshing = false;
        true
    }

    /// 刷新读到了内容(连接身份须已复核过)。`has_editor`:视图手上有编辑器实体。
    pub(super) fn on_refresh_loaded(&mut self, content: R, has_editor: bool) -> RefreshLoaded {
        self.refresh_warning = None;
        let loaded = content.content();
        let editable = has_editor && !loaded.is_binary && !loaded.too_large;
        let unchanged = editable
            && LineEnding::detect(&loaded.content) == self.line_ending
            && normalize_to_lf(&loaded.content) == self.saved;
        if unchanged {
            self.remote_baseline = content.into_parts().1;
            self.remote_conflict = None;
            self.error = None;
            RefreshLoaded::Unchanged
        } else if self.dirty {
            // The user started typing while the refresh was in
            // flight. Preserve the draft and surface the same
            // explicit reload/overwrite decision used by save.
            self.remote_conflict = Some(content);
            RefreshLoaded::Conflict
        } else {
            let save_warning = self.save_warning.clone();
            let text = self.apply_remote_content(content);
            // A successful refresh resolves only the refresh
            // warning. A prior committed-save cleanup warning
            // remains actionable until the next save/reload.
            self.save_warning = save_warning;
            RefreshLoaded::Replaced(text)
        }
    }

    /// 刷新失败。已有可用内容时只挂警告(编辑器 / 草稿照常可见),否则进致命错误页。
    pub(super) fn on_refresh_failed(
        &mut self,
        error: String,
        has_editor: bool,
    ) -> RemoteRefreshFailurePresentation {
        let presentation = remote_refresh_failure_presentation(self.result.is_some(), has_editor);
        match presentation {
            RemoteRefreshFailurePresentation::Warning => {
                self.refresh_warning = Some(error);
                self.error = None;
            }
            RemoteRefreshFailurePresentation::Fatal => {
                self.refresh_warning = None;
                self.error = Some(error);
            }
        }
        presentation
    }

    /// 记下连接身份复核的结论,返回是否变了(变了视图要重画)。
    pub(super) fn set_remote_source_invalid(&mut self, invalid: bool) -> bool {
        if self.remote_source_invalid == invalid {
            return false;
        }
        self.remote_source_invalid = invalid;
        true
    }

    /// 远程冲突横幅的「重新加载」:取走冲突时读回的内容,交给视图重新落基线。
    pub(super) fn take_remote_conflict(&mut self) -> Option<R> {
        self.remote_conflict.take()
    }

    // ── 保存 ───────────────────────────────────────────────

    /// 保存第一步:干净或在保存中时不存(Ctrl+S 是肌肉记忆,静默返回);要存就先
    /// 作废在飞的激活刷新。随后视图复核连接身份,再 [`Self::begin_save`]。
    pub(super) fn prepare_save(&mut self, text: &str) -> bool {
        if self.saving || text == self.saved {
            return false;
        }
        if self.remote_refreshing {
            // Saving performs its own fresh baseline validation. Invalidate the
            // older activation refresh so its late result cannot replace a draft
            // or conflict state owned by this save.
            self.load_generation = self.load_generation.wrapping_add(1);
            self.remote_refreshing = false;
        }
        true
    }

    /// 保存第二步:身份失效不存;否则进保存态,交出代次与落盘文本。
    pub(super) fn begin_save(&mut self, text: &str) -> Option<SaveStart> {
        if self.remote_source_invalid {
            return None;
        }
        self.saving = true;
        self.save_error = None;
        self.save_warning = None;
        self.remote_conflict = None;
        Some(SaveStart {
            generation: self.load_generation,
            on_disk: self.to_disk_form(text),
        })
    }

    /// 远程文档没有可比对的基线(二进制 / 超限 / 读失败后):只读,撤销保存态。
    pub(super) fn abort_save_read_only(&mut self) {
        self.saving = false;
        self.save_error = Some(t("fileViewer", "remoteReadOnly").to_string());
    }

    /// 配置里已取不到这个项目的连接:撤销保存态,标记身份失效。
    pub(super) fn abort_save_source_invalid(&mut self) {
        self.saving = false;
        self.remote_source_invalid = true;
    }

    /// 写盘结果回来了。代次对不上返回 `false`,整条丢弃(保存态不动);对上了
    /// 就结束保存态。
    pub(super) fn settle_save(&mut self, generation: u64) -> bool {
        if self.load_generation != generation {
            return false;
        }
        self.saving = false;
        true
    }

    /// 本地写盘的结论。`editor_text` 只在收尾时取(按最新草稿重算脏态)。
    pub(super) fn on_local_save_result(
        &mut self,
        text: String,
        outcome: Result<(), String>,
        now: Instant,
        editor_text: impl FnOnce() -> Option<String>,
    ) {
        match outcome {
            Ok(()) => self.finish_save(text, None, None, now, editor_text),
            Err(err) => self.save_error = Some(err),
        }
    }

    /// 远程保存的结论(连接身份须已复核过)。
    pub(super) fn on_remote_save_result(
        &mut self,
        text: String,
        outcome: Result<RemoteSave<R>, String>,
        now: Instant,
        editor_text: impl FnOnce() -> Option<String>,
    ) {
        let save_succeeded = matches!(&outcome, Ok(RemoteSave::Saved { .. }));
        self.refresh_warning =
            refresh_warning_after_remote_save(self.refresh_warning.take(), save_succeeded);
        match outcome {
            Ok(RemoteSave::Saved { baseline, warning }) => {
                self.finish_save(text, Some(baseline), warning, now, editor_text);
            }
            Ok(RemoteSave::ExternalChange { current }) => {
                self.remote_conflict = Some(current);
            }
            Err(err) => self.save_error = Some(err),
        }
    }

    fn finish_save(
        &mut self,
        text: String,
        remote_baseline: Option<R::Baseline>,
        warning: Option<String>,
        now: Instant,
        editor_text: impl FnOnce() -> Option<String>,
    ) {
        // 写回的是 `tabs.restore(text)`,此后「没动过的行」以它为准:用写回文本重算
        // 一遍映射。`expand(restore(v)) == v` 是 `tab_expansion` 的不变式,编辑器内容
        // 不用重建,`saved` 直接取 `text`
        self.tabs = TabExpansion::expand(&self.tabs.restore(&text), TAB_WIDTH).1;
        self.saved = text.clone();
        self.disk = text.clone();
        self.last_save_at = Some(now);
        self.remote_baseline = remote_baseline.or_else(|| self.remote_baseline.clone());
        self.remote_conflict = None;
        self.save_warning = warning;
        // 保存期间用户可能又敲了字:按**最新**草稿重新比对。
        self.dirty = self.draft(editor_text()) != text;
        self.ext_changed = false;
    }

    // ── 外部改动 ───────────────────────────────────────────

    /// 监听报「这个文件在磁盘上变了」(逐条对照 `FileViewerModal.tsx:275-283`)。
    /// `editor_text` 只在过了回声窗口之后才取。
    pub(super) fn on_fs_change(
        &mut self,
        now: Instant,
        editor_text: impl FnOnce() -> Option<String>,
    ) -> FsChange {
        if self.result.is_none() {
            return FsChange::Ignore;
        }
        // 自己 write 落盘触发的回声,不算「外部」修改
        if self
            .last_save_at
            .is_some_and(|at| now.saturating_duration_since(at) < ECHO_WINDOW)
        {
            return FsChange::Ignore;
        }
        if self.draft(editor_text()) != self.saved || self.saving {
            // 脏或正在保存:先挂提示条，不能在旧写入尚未收口时重建编辑器。
            self.ext_changed = true;
            FsChange::Flagged
        } else {
            // 干净:静默重载跟上磁盘
            FsChange::Reload
        }
    }
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
