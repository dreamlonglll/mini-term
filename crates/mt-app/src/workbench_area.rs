//! 主内容工作区：常驻终端页 + 项目级页签（文件页签与 diff 页签）。
//!
//! 页签是纯运行时状态，不进入 `SplitNode`、PTY 映射或布局数据库。切到页签页只是
//! 不渲染 [`TerminalArea`]，终端实体及其全部后台会话仍由 `AppStore` 保活。
//!
//! # 两种页签
//!
//! - **文件页签**([`FileViewer`]):文件树 / 搜索 / 快速打开 / Markdown 链接打开的文件,
//!   身份是 [`DocumentKey`](项目 + 后端连接 + 路径);
//! - **diff 页签**([`DiffTab`]):单文件 diff / 提交 diff / 合并变更 / 单文件历史 / 磁盘对比,
//!   身份是 [`DiffKey`](项目 + diff 目标,口径见 `git_diff::tab` 的模块注释)。只读、没有
//!   「未保存」,关闭不确认;单文件 diff 里换文件时身份跟着变(见
//!   [`WorkbenchArea::retarget_diff_tab`])。diff 页签一律归打开那一刻的活动项目。
//!
//! 两种页签共用页签条:单击切换、× 与中键关闭、右键「关闭 / 关闭其他 / 关闭右边 / 关闭左边」,
//! 终端页右键「关闭所有」;切项目时各项目各留各的页签,移除项目时随项目一起回收。
//! 「最近打开」与未保存检查只认文件页签。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, Global, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Window, div, point, prelude::FluentBuilder,
    px,
};
use gpui_component::WindowExt as _;
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::file_viewer::{DocumentSource, FileViewer, LineTarget};
use crate::git_diff::{DiffKey, DiffTab, DiffTabEvent};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_alert};
use crate::store::{AppStore, StoreEvent};
use crate::terminal_area::TerminalArea;
use crate::{git_ui, ui};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DocumentBackendKey {
    Local,
    Remote {
        connection_id: String,
        connection_fingerprint: u64,
    },
}

/// 一个打开文件的稳定身份。项目、后端连接身份和路径共同参与去重。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DocumentKey {
    project_id: String,
    backend: DocumentBackendKey,
    normalized_path: String,
}

impl DocumentKey {
    fn from_source(source: &DocumentSource) -> Self {
        let (backend, normalized_path) = match source {
            DocumentSource::Local { path, .. } => (
                DocumentBackendKey::Local,
                normalize_local_document_path(path),
            ),
            DocumentSource::Remote {
                connection, path, ..
            } => (
                DocumentBackendKey::Remote {
                    connection_id: connection.id.clone(),
                    connection_fingerprint: mt_remote::connection_fingerprint(connection),
                },
                normalize_remote_document_path(path),
            ),
        };
        Self {
            project_id: source.project_id().to_string(),
            backend,
            normalized_path,
        }
    }
}

fn normalize_local_document_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        normalized.replace('\\', "/").to_lowercase()
    } else {
        normalized
    }
}

fn normalize_remote_document_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 页签的身份:文件页签按文档身份,diff 页签按 diff 目标。同一项目里同一身份只开一个页签,
/// 再次打开就切过去。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TabKey {
    Document(DocumentKey),
    Diff(DiffKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkbenchPage {
    Terminal,
    Tab(TabKey),
}

/// 页签里装的东西。
#[derive(Clone)]
enum TabContent {
    File(Entity<FileViewer>),
    Diff(Entity<DiffTab>),
}

impl TabContent {
    /// 有没有未保存的修改(只有文件页签会有)。
    fn is_dirty(&self, cx: &App) -> bool {
        match self {
            TabContent::File(document) => document.read(cx).is_dirty(),
            TabContent::Diff(_) => false,
        }
    }

    fn view(&self) -> AnyElement {
        match self {
            TabContent::File(document) => document.clone().into_any_element(),
            TabContent::Diff(diff) => diff.clone().into_any_element(),
        }
    }

    /// 切到这一页之后(延到这一轮更新收口)交焦点、按需刷新 —— 文件页签见
    /// [`FileViewer::on_activated`],diff 页签见 [`DiffTab::on_activated`]。
    fn activate_later(&self, window: &mut Window, cx: &mut App) {
        match self.clone() {
            TabContent::File(document) => window.defer(cx, move |window, cx| {
                document.update(cx, |document, cx| document.on_activated(window, cx));
            }),
            TabContent::Diff(diff) => window.defer(cx, move |window, cx| {
                diff.update(cx, |diff, cx| diff.on_activated(window, cx));
            }),
        }
    }
}

struct DocumentTab {
    key: TabKey,
    title: String,
    /// 悬停提示:diff 页签写完整路径 / 提交信息(标题装不下的那部分);文件页签没有。
    tip: Option<String>,
    content: TabContent,
    /// 本地文件的原样路径(`key` 里那份在 Windows 上是小写化的,不能拿来打开)。远程文件与
    /// diff 页签为 `None`。只给「最近打开」([`ProjectDocuments::recent`])记账用。
    local_path: Option<PathBuf>,
}

/// 渲染一帧要的页签快照(不在渲染途中借着 `projects` 建元素)。
struct TabSnapshot {
    key: TabKey,
    title: String,
    tip: Option<String>,
    content: TabContent,
}

type ActiveWorkbenchSnapshot = (String, WorkbenchPage, Vec<TabSnapshot>);

/// 「最近打开」每个项目最多记几条。快速打开的空查询把它们列在最前,一屏放得下的三四倍就够了。
const MAX_RECENT_FILES: usize = 50;

struct ProjectDocuments {
    tabs: Vec<DocumentTab>,
    active: WorkbenchPage,
    /// 最近在文件页里打开 / 切到过的**本地**文件,新的在前、去重。给快速打开
    /// (`quick_open`)排序用:空查询列在最前,有查询时同分靠前。
    ///
    /// 只放内存:关掉页签不删(关掉的文件仍是「最近打开过」),项目被移除时随整个
    /// `ProjectDocuments` 一起回收,重启清空。
    recent: Vec<PathBuf>,
}

impl Default for ProjectDocuments {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: WorkbenchPage::Terminal,
            recent: Vec::new(),
        }
    }
}

impl ProjectDocuments {
    fn index_of(&self, key: &TabKey) -> Option<usize> {
        self.tabs.iter().position(|tab| &tab.key == key)
    }

    /// 某个 diff 页签实体在第几个(按实体认,不按身份:换文件时身份会变)。
    fn index_of_diff(&self, diff: &Entity<DiffTab>) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            matches!(&tab.content, TabContent::Diff(entity) if entity.entity_id() == diff.entity_id())
        })
    }

    /// 把一个本地文件记到「最近打开」最前面。
    fn note_recent(&mut self, path: Option<&Path>) {
        if let Some(path) = path {
            push_recent(&mut self.recent, path, MAX_RECENT_FILES);
        }
    }

    fn close(&mut self, key: &TabKey) -> Option<TabContent> {
        let index = self.index_of(key)?;
        let removed = self.tabs.remove(index).content;
        if self.active == WorkbenchPage::Tab(key.clone()) {
            let remaining = self
                .tabs
                .iter()
                .map(|tab| tab.key.clone())
                .collect::<Vec<_>>();
            self.active = next_document_after_close(&remaining, index)
                .map(WorkbenchPage::Tab)
                .unwrap_or(WorkbenchPage::Terminal);
        }
        Some(removed)
    }
}

/// diff 页签换了身份(单文件 diff 里换了文件)之后页签区该怎么收拾:返回要顺手关掉的
/// **同身份旧页签**的下标(没有就 `None`),以及换完之后的活动页。
///
/// 换到的目标恰好已有另一个页签时,**换文件的这个页签接管目标、旧的那个关掉**:用户正在
/// 这个页签里按 Alt+←/→ 往下看,它手上的文件序列(连同更改列表的回调)才是要接着用的;
/// 旧页签多半是早先点开、内容已经过时的同一份 diff。这样页签条上不会出现两个一模一样的
/// 页签,也不会把用户从正在看的页签里拽走。活动页原本指着换文件的这个页签的,跟着新身份走。
fn retarget_plan(
    keys: &[TabKey],
    active: &WorkbenchPage,
    index: usize,
    new_key: &TabKey,
) -> (Option<usize>, WorkbenchPage) {
    let Some(old_key) = keys.get(index) else {
        return (None, active.clone());
    };
    let duplicate = keys
        .iter()
        .enumerate()
        .find(|(i, key)| *i != index && *key == new_key)
        .map(|(i, _)| i);
    let active = match active {
        WorkbenchPage::Tab(key) if key == old_key => WorkbenchPage::Tab(new_key.clone()),
        _ => active.clone(),
    };
    (duplicate, active)
}

fn next_document_after_close(remaining: &[TabKey], removed_index: usize) -> Option<TabKey> {
    remaining
        .get(removed_index)
        .or_else(|| {
            removed_index
                .checked_sub(1)
                .and_then(|index| remaining.get(index))
        })
        .cloned()
}

/// 页签右键菜单里的批量关闭范围，锚点是被右键的那一页。
///
/// 三档都**只覆盖页签区里的页签**(文件页签与 diff 页签一视同仁)：常驻的终端页不在
/// `tabs` 里，也就永远不会被「关闭其他」带走 —— 它是关不掉的（见 [`WorkbenchPage`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseScope {
    Others,
    ToRight,
    ToLeft,
}

/// 按范围挑出要关的页签（不含锚点自己）。顺序与页签条一致，便于逐个 `close`。
fn documents_in_scope(keys: &[TabKey], anchor_index: usize, scope: CloseScope) -> Vec<TabKey> {
    keys.iter()
        .enumerate()
        .filter(|(index, _)| match scope {
            CloseScope::Others => *index != anchor_index,
            CloseScope::ToRight => *index > anchor_index,
            CloseScope::ToLeft => *index < anchor_index,
        })
        .map(|(_, key)| key.clone())
        .collect()
}

struct GlobalWorkbench(Entity<WorkbenchArea>);
impl Global for GlobalWorkbench {}

/// 安装统一文件打开入口。文件树和全局搜索都只调用这里。
pub fn install(area: Entity<WorkbenchArea>, cx: &mut App) {
    cx.set_global(GlobalWorkbench(area));
}

fn global(cx: &App) -> Option<Entity<WorkbenchArea>> {
    cx.try_global::<GlobalWorkbench>()
        .map(|global| global.0.clone())
}

fn project_documents_are_dirty(project: &ProjectDocuments, cx: &App) -> bool {
    project.tabs.iter().any(|tab| tab.content.is_dirty(cx))
}

/// 项目移除、worktree 清理等生命周期操作的统一防丢失闸。
pub fn project_has_dirty_documents(project_id: &str, cx: &App) -> bool {
    global(cx).is_some_and(|area| {
        area.read(cx)
            .projects
            .get(project_id)
            .is_some_and(|project| project_documents_are_dirty(project, cx))
    })
}

/// 关窗确认使用的未保存文档列表。项目名与页签名一起展示，避免不同项目中的
/// 同名文件让用户无法判断哪些草稿会被丢弃。
pub fn dirty_document_names(cx: &App) -> Vec<String> {
    let Some(area) = global(cx) else {
        return Vec::new();
    };
    let area = area.read(cx);
    let store = area.store.read(cx);
    let mut names = Vec::new();
    for (project_id, documents) in &area.projects {
        let project_name = store
            .project(project_id)
            .map(|project| project.name.as_str())
            .unwrap_or(project_id);
        for tab in &documents.tabs {
            if tab.content.is_dirty(cx) {
                names.push(format!("{project_name}: {}", tab.title));
            }
        }
    }
    names.sort();
    names
}

/// 按当前项目快照打开文件。远程项目会同时快照连接身份，断链时明确报错。
pub fn open_active_file(
    store: Entity<AppStore>,
    path: PathBuf,
    highlight: Option<LineTarget>,
    window: &mut Window,
    cx: &mut App,
) {
    let snapshot = {
        let store = store.read(cx);
        let Some(project) = store.active_project() else {
            return;
        };
        (
            project.clone(),
            store.is_remote_project(&project.id),
            store.remote_connection_of(&project.id),
        )
    };
    let (project, remote, connection) = snapshot;
    let source = if remote {
        let Some(connection) = connection else {
            show_alert(
                t("terminalArea", "remoteConnectFailedTitle"),
                t("fileTree", "remote.broken"),
                window,
                cx,
            );
            return;
        };
        DocumentSource::Remote {
            project_id: project.id.clone(),
            connection: Box::new(connection),
            project_root: project.path.clone(),
            path,
        }
    } else {
        DocumentSource::Local {
            project_id: project.id.clone(),
            project_root: PathBuf::from(&project.path),
            path,
        }
    };

    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| {
        area.open_document(source, highlight, window, cx)
    });
}

/// 文件页内 Markdown 链接指向的本地文件:来源身份(项目 / 连接)沿用当前页签的,
/// 已开的页签就切过去,没开的新开一个。
pub fn open_document_source(source: DocumentSource, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| area.open_document(source, None, window, cx));
}

/// 文件页内部的 Ctrl/Cmd+W 入口。延迟执行前先快照来源身份，避免用户在
/// `window.defer` 落地前切换页签后误关新的活动页。
pub fn close_document_source(source: DocumentSource, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    let project_id = source.project_id().to_string();
    let key = TabKey::Document(DocumentKey::from_source(&source));
    area.update(cx, |area, cx| {
        area.request_close_document(project_id, key, window, cx)
    });
}

/// 某个文档的身份(diff 页签的「磁盘对比」按它认是哪个文档)。
pub fn document_key(source: &DocumentSource) -> DocumentKey {
    DocumentKey::from_source(source)
}

// ─── diff 页签 ───────────────────────────────────────────────
//
// diff 页签的打开流程在 `git_diff::tab`(先按身份找、找到就复用,否则建一个),这里只管
// 「放进页签区 / 切过去 / 关掉 / 问现在是不是它」。

/// 身份为 `key` 的 diff 页签(没开就是 `None`)。
pub fn find_diff_tab(key: &DiffKey, cx: &App) -> Option<Entity<DiffTab>> {
    let area = global(cx)?;
    let area = area.read(cx);
    let project = area.projects.get(key.project_id())?;
    let index = project.index_of(&TabKey::Diff(key.clone()))?;
    match &project.tabs[index].content {
        TabContent::Diff(diff) => Some(diff.clone()),
        TabContent::File(_) => None,
    }
}

/// 把 diff 页签放进它所属项目的页签区(已经在就不再放一次)并切过去、交焦点。
///
/// 调用方要保证此刻没有借着任何实体(`git_diff::tab` 的打开流程都延到 `window.defer`
/// 里才调这里):这里会回读页签实体的身份与标题。
pub fn show_diff_tab(diff: Entity<DiffTab>, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| area.show_diff_tab(diff, window, cx));
}

/// 这个 diff 页签是不是用户眼前那一页(活动项目的活动页)。后台页签的迟到结果只更新
/// 自己,不抢焦点;仓库变了也只有眼前那一页当场重取(其余等切回来再取)。
pub fn is_diff_tab_active(key: &DiffKey, cx: &App) -> bool {
    let Some(area) = global(cx) else {
        return false;
    };
    let area = area.read(cx);
    area.store.read(cx).active_project_id.as_deref() == Some(key.project_id())
        && area
            .projects
            .get(key.project_id())
            .is_some_and(|project| project.active == WorkbenchPage::Tab(TabKey::Diff(key.clone())))
}

/// 全部打开着的 diff 页签(各项目的都算)。仓库变了时逐个问一遍要不要重取。
pub fn diff_tabs(cx: &App) -> Vec<Entity<DiffTab>> {
    let Some(area) = global(cx) else {
        return Vec::new();
    };
    area.read(cx)
        .projects
        .values()
        .flat_map(|project| project.tabs.iter())
        .filter_map(|tab| match &tab.content {
            TabContent::Diff(diff) => Some(diff.clone()),
            TabContent::File(_) => None,
        })
        .collect()
}

/// diff 页签里的 Ctrl/Cmd+W。diff 页签没有未保存状态,直接关。
pub fn close_diff_tab(key: DiffKey, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    let project_id = key.project_id().to_string();
    area.update(cx, |area, cx| {
        area.request_close_document(project_id, TabKey::Diff(key), window, cx)
    });
}

/// Unified entry for existing navigation surfaces that explicitly jump to a
/// terminal pane (toast, session list, title bar, tray). Activating a hidden
/// pane without switching the workbench page would leave focus in an invisible
/// PTY while the document page remains on screen.
pub fn activate_terminal_page(window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| area.activate_terminal(window, cx));
}

/// 文档异步读盘完成时用来判断是否可以接管焦点。后台页签或其它项目的迟到结果
/// 只能更新自身内容，不能把键盘焦点从当前终端/文档页抢走。
pub fn is_document_active(source: &DocumentSource, cx: &App) -> bool {
    let Some(area) = global(cx) else {
        return false;
    };
    let key = DocumentKey::from_source(source);
    let area = area.read(cx);
    let Some(active_project_id) = area.store.read(cx).active_project_id.clone() else {
        return false;
    };
    active_project_id == key.project_id
        && area
            .projects
            .get(&active_project_id)
            .is_some_and(|project| project.active == WorkbenchPage::Tab(TabKey::Document(key)))
}

/// 该项目「最近打开」的本地文件,新的在前(见 [`ProjectDocuments::recent`])。
/// 快速打开(`quick_open`)用它排序。
pub fn recent_files(project_id: &str, cx: &App) -> Vec<PathBuf> {
    global(cx)
        .and_then(|area| {
            area.read(cx)
                .projects
                .get(project_id)
                .map(|project| project.recent.clone())
        })
        .unwrap_or_default()
}

/// 该项目当前停在哪个本地文件页上;停在终端页、或活动页签是远程文件时为 `None`。
///
/// 快速打开用它做两件事:空查询时把它排除出「最近打开」(第一条就是上一个文件,
/// 打开面板直接回车即可切回),以及只输 `:行号` 时在它里面跳行。
pub fn active_document_path(project_id: &str, cx: &App) -> Option<PathBuf> {
    let area = global(cx)?;
    let area = area.read(cx);
    let project = area.projects.get(project_id)?;
    let WorkbenchPage::Tab(key) = &project.active else {
        return None;
    };
    let index = project.index_of(key)?;
    project.tabs[index].local_path.clone()
}

/// 把 `path` 放到 `recent` 最前面(已在表里就挪上来),只留前 `cap` 条。
///
/// 比较按 [`normalize_local_document_path`] 的口径:Windows 上大小写不同的两种写法是同一个
/// 文件,页签去重也是这么认的。
fn push_recent(recent: &mut Vec<PathBuf>, path: &Path, cap: usize) {
    let normalized = normalize_local_document_path(path);
    recent.retain(|existing| normalize_local_document_path(existing) != normalized);
    recent.insert(0, path.to_path_buf());
    recent.truncate(cap);
}

/// Restore focus after a modal overlay closes without capturing the document
/// that happened to be active before the close. The active project and page
/// are resolved at handoff time, and `FileViewer::on_activated` performs the
/// final identity/overlay checks again in the deferred callback.
pub fn reactivate_active_document(expected_project_id: &str, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| {
        area.reactivate_active_document(expected_project_id, window, cx)
    });
}

/// 文件页签宿主。
pub struct WorkbenchArea {
    store: Entity<AppStore>,
    terminal_area: Entity<TerminalArea>,
    projects: HashMap<String, ProjectDocuments>,
    last_rendered_project: Option<String>,
    last_rendered_page: Option<WorkbenchPage>,
    /// 页签条的横向滚动位置。**全项目共用一份**：切项目时页签整条重建，
    /// 上一个项目的偏移留着也无意义，切完那一帧的 `scroll_to_item` 会把活动页
    /// 拉回视野。只跟**文档页签**那段滚动区走，常驻终端页在滚动区之外。
    tab_scroll: ScrollHandle,
    /// 鼠标是否落在文档页签那段滚动区里。横向滚动条只在「溢出 + 悬停」时现身，
    /// 判据见 [`Self::render`]。
    tabs_hovered: bool,
    /// 正在拖页签滚动条：按下那一刻的 (鼠标 x, 滚动偏移 x)。换算见
    /// [`tab_drag_offset`]；拖动期间即使鼠标滑出页签条也保持滚动条可见。
    tab_drag: Option<(gpui::Pixels, gpui::Pixels)>,
}

impl WorkbenchArea {
    pub fn new(
        store: Entity<AppStore>,
        terminal_area: Entity<TerminalArea>,
        cx: &mut Context<Self>,
    ) -> Self {
        // render 读活动项目等 store 数据,任何变化都照旧重画
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        // 页签的回收与远程来源校验只看「项目还在不在 / 路径 / SSH 连接」
        // (见 `StoreEvent::touches_document_sources`)。此前挂在 notify 上,AI 工作时
        // OSC 标题约 4Hz/pane 就把每个文档页签的校验全跑一遍。
        cx.subscribe(&store, |this, store, event: &StoreEvent, cx| {
            if !event.touches_document_sources() {
                return;
            }
            let project_ids = store
                .read(cx)
                .projects()
                .iter()
                .map(|project| project.id.clone())
                .collect::<std::collections::HashSet<_>>();
            // 正常移除入口会在 AppStore 层拒绝丢弃脏页签；这里再留一道兜底，
            // 防止配置被其它路径直接改写时观察者静默销毁内存草稿。
            this.projects.retain(|project_id, project| {
                project_ids.contains(project_id) || project_documents_are_dirty(project, cx)
            });
            for project in this.projects.values() {
                for tab in &project.tabs {
                    if let TabContent::File(document) = &tab.content {
                        document.update(cx, |document, cx| document.validate_remote_source(cx));
                    }
                }
            }
            cx.notify();
        })
        .detach();
        Self {
            store,
            terminal_area,
            projects: HashMap::new(),
            last_rendered_project: None,
            last_rendered_page: None,
            tab_scroll: ScrollHandle::new(),
            tabs_hovered: false,
            tab_drag: None,
        }
    }

    /// 拖页签滚动条时把鼠标位移换成新的滚动偏移。
    ///
    /// 挂在**工作区根容器**上而不是 thumb 自己:gpui 的 `on_mouse_move` 只在
    /// 元素 hover 时触发,挂在那条 4px 的 thumb 上,鼠标一垂直抖出去拖动就断了。
    fn on_tab_scroll_drag(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_x, anchor_offset)) = self.tab_drag else {
            return;
        };
        // 在页签条外松开的左键收不到 mouse_up,靠这一步收尾
        if event.pressed_button != Some(MouseButton::Left) {
            self.tab_drag = None;
            cx.notify();
            return;
        }
        let offset = tab_drag_offset(
            f32::from(self.tab_scroll.bounds().size.width),
            f32::from(self.tab_scroll.max_offset().x),
            f32::from(anchor_offset),
            f32::from(event.position.x - anchor_x),
        );
        self.tab_scroll
            .set_offset(point(px(offset), self.tab_scroll.offset().y));
        cx.notify();
    }

    pub fn is_terminal_active(&self, cx: &App) -> bool {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return true;
        };
        self.is_terminal_page_for_project(&project_id, cx)
    }

    fn is_terminal_page_for_project(&self, project_id: &str, cx: &App) -> bool {
        if self.store.read(cx).active_project_id.as_deref() != Some(project_id) {
            return false;
        }
        self.projects
            .get(project_id)
            .is_none_or(|project| project.active == WorkbenchPage::Terminal)
    }

    fn open_document(
        &mut self,
        source: DocumentSource,
        highlight: Option<LineTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_id = source.project_id().to_string();
        let key = TabKey::Document(DocumentKey::from_source(&source));
        let local_path = match &source {
            DocumentSource::Local { path, .. } => Some(path.clone()),
            DocumentSource::Remote { .. } => None,
        };
        let project = self.projects.entry(project_id).or_default();
        project.note_recent(local_path.as_deref());
        if let Some(index) = project.index_of(&key) {
            project.active = WorkbenchPage::Tab(key);
            if let TabContent::File(document) = project.tabs[index].content.clone() {
                window.defer(cx, move |window, cx| {
                    document.update(cx, |document, cx| {
                        document.reveal_line(highlight, window, cx);
                        document.on_activated(window, cx);
                    });
                });
            }
            cx.notify();
            return;
        }

        let title = source.file_name();
        let document = cx.new(|cx| FileViewer::new_document(source, highlight, window, cx));
        cx.observe(&document, |_this, _document, cx| cx.notify())
            .detach();
        let content = TabContent::File(document);
        project.tabs.push(DocumentTab {
            key: key.clone(),
            title,
            tip: None,
            content: content.clone(),
            local_path,
        });
        project.active = WorkbenchPage::Tab(key);
        content.activate_later(window, cx);
        cx.notify();
    }

    /// 把 diff 页签放进它所属项目的页签区并切过去(见模块函数 [`show_diff_tab`])。
    ///
    /// 同一个实体已经在页签区里 → 只是切过去;同身份的另一个页签已经开着(两次打开在后台
    /// 取数时撞上了)→ 切到开着的那个,新建的这个丢掉。新放进来的页签订阅它的
    /// [`DiffTabEvent`](换文件时身份 / 标题跟着变)。
    fn show_diff_tab(
        &mut self,
        diff: Entity<DiffTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, title, tip) = {
            let tab = diff.read(cx);
            (
                tab.key().clone(),
                tab.title().to_string(),
                tab.tip().to_string(),
            )
        };
        let project_id = key.project_id().to_string();
        let key = TabKey::Diff(key);
        let project = self.projects.entry(project_id.clone()).or_default();
        if project.index_of_diff(&diff).is_none() && project.index_of(&key).is_none() {
            cx.subscribe(&diff, Self::on_diff_tab_event).detach();
            project.tabs.push(DocumentTab {
                key: key.clone(),
                title,
                tip: Some(tip),
                content: TabContent::Diff(diff),
                local_path: None,
            });
        }
        self.activate_document(&project_id, &key, window, cx);
    }

    fn on_diff_tab_event(
        &mut self,
        diff: Entity<DiffTab>,
        event: &DiffTabEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            DiffTabEvent::Retarget => self.retarget_diff_tab(&diff, cx),
        }
    }

    /// diff 页签换了身份 / 标题(单文件 diff 里 Alt+←/→ 换了文件,或块操作后换到了组里的
    /// 下一个):页签区跟着改,同身份的旧页签按 [`retarget_plan`] 关掉。页签已经被关掉
    /// (后台任务晚到)就什么都不做。
    fn retarget_diff_tab(&mut self, diff: &Entity<DiffTab>, cx: &mut Context<Self>) {
        let (key, title, tip) = {
            let tab = diff.read(cx);
            (
                tab.key().clone(),
                tab.title().to_string(),
                tab.tip().to_string(),
            )
        };
        let new_key = TabKey::Diff(key);
        for (project_id, project) in self.projects.iter_mut() {
            let Some(index) = project.index_of_diff(diff) else {
                continue;
            };
            let keys = project
                .tabs
                .iter()
                .map(|tab| tab.key.clone())
                .collect::<Vec<_>>();
            let (duplicate, active) = retarget_plan(&keys, &project.active, index, &new_key);
            let old_key = std::mem::replace(&mut project.tabs[index].key, new_key.clone());
            project.tabs[index].title = title;
            project.tabs[index].tip = Some(tip);
            if let Some(duplicate) = duplicate {
                project.tabs.remove(duplicate);
            }
            // 还是同一页,别让 render 把它当成「切过来了」再交一次焦点、再刷一次
            if self.last_rendered_project.as_deref() == Some(project_id.as_str())
                && self.last_rendered_page == Some(WorkbenchPage::Tab(old_key))
            {
                self.last_rendered_page = Some(active.clone());
            }
            project.active = active;
            cx.notify();
            return;
        }
    }

    pub fn activate_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        self.projects.entry(project_id.clone()).or_default().active = WorkbenchPage::Terminal;
        let pane_id = self.store.read(cx).active_pane_id(&project_id);
        if let Some(pane_id) = pane_id {
            self.store.update(cx, |store, cx| {
                store.focus_pane(&project_id, &pane_id, window, cx)
            });
        }
        cx.notify();
    }

    fn activate_document(
        &mut self,
        project_id: &str,
        key: &TabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_id) else {
            return;
        };
        let Some(index) = project.index_of(key) else {
            return;
        };
        let content = project.tabs[index].content.clone();
        // 点页签切过去也算「最近打开」(与 VS Code 的快速打开同口径;diff 页签不记)
        let local_path = project.tabs[index].local_path.clone();
        project.note_recent(local_path.as_deref());
        project.active = WorkbenchPage::Tab(key.clone());
        content.activate_later(window, cx);
        cx.notify();
    }

    fn reactivate_active_document(
        &mut self,
        expected_project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.store.read(cx).active_project_id.as_deref() != Some(expected_project_id) {
            return;
        }
        let Some(WorkbenchPage::Tab(key)) = self
            .projects
            .get(expected_project_id)
            .map(|project| project.active.clone())
        else {
            return;
        };
        self.activate_document(expected_project_id, &key, window, cx);
    }

    /// Ctrl/Cmd+F 落到文件页:交给当前文档的编辑器查找。diff 页签没有查找,什么都不做。
    pub fn search_active_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let Some(WorkbenchPage::Tab(key)) = self
            .projects
            .get(&project_id)
            .map(|project| project.active.clone())
        else {
            return;
        };
        let Some(TabContent::File(document)) = self.projects.get(&project_id).and_then(|project| {
            project
                .index_of(&key)
                .map(|index| project.tabs[index].content.clone())
        }) else {
            return;
        };
        document.update(cx, |document, cx| document.open_search(window, cx));
    }

    fn request_close_document(
        &mut self,
        project_id: String,
        key: TabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self
            .projects
            .get(&project_id)
            .and_then(|project| project.index_of(&key).map(|index| &project.tabs[index]))
            .is_some_and(|tab| tab.content.is_dirty(cx));
        if !dirty {
            self.close_document(&project_id, &key, window, cx);
            return;
        }

        let this = cx.entity();
        Confirm::new(
            t("fileViewer", "unsavedTitle"),
            t("fileViewer", "unsavedMessage"),
        )
        .open(
            move |window, cx| {
                let this = this.clone();
                let project_id = project_id.clone();
                let key = key.clone();
                window.defer(cx, move |window, cx| {
                    this.update(cx, |area, cx| {
                        area.close_document(&project_id, &key, window, cx)
                    });
                });
            },
            window,
            cx,
        );
    }

    fn close_document(
        &mut self,
        project_id: &str,
        key: &TabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_documents(project_id, std::slice::from_ref(key), None, window, cx);
    }

    /// 一次关掉一批页签。`fallback` 是右键的那一页（批量关闭时必然留下），
    /// 活动页被这一批带走时直接落到它上面 —— 逐个 `close` 的左右邻居收敛
    /// 在批量语境下会把活动页停在一个马上又要被关掉的页签上。
    ///
    /// 「关闭所有」没有锚点、传 `None`：页签全部关完，`close` 的收敛终点就是终端页。
    fn close_documents(
        &mut self,
        project_id: &str,
        keys: &[TabKey],
        fallback: Option<&TabKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_id) else {
            return;
        };
        let mut closed_active = false;
        for key in keys {
            closed_active |= project.active == WorkbenchPage::Tab(key.clone());
            let _ = project.close(key);
        }
        if closed_active
            && let Some(anchor) = fallback
            && project.index_of(anchor).is_some()
        {
            project.active = WorkbenchPage::Tab(anchor.clone());
        }
        // 批量关闭后剩下的页签会左移，滚动偏移原地不动就会停在一段空白上；
        // 把留下的锚点重新拉进视野（活动页没被动过时也一样成立）。
        // 滚动区里**只有页签区的页签**，序号即 tabs 下标（终端页固定在滚动区外）
        if let Some(anchor) = fallback
            && let Some(index) = project.index_of(anchor)
        {
            self.tab_scroll.scroll_to_item(index);
        }
        let project_is_visible =
            self.store.read(cx).active_project_id.as_deref() == Some(project_id);
        if !closed_active || !project_is_visible {
            cx.notify();
            return;
        }
        match project.active.clone() {
            WorkbenchPage::Terminal => self.activate_terminal(window, cx),
            WorkbenchPage::Tab(next) => self.activate_document(project_id, &next, window, cx),
        }
        cx.notify();
    }

    /// 右键菜单的三档批量关闭要关掉哪些页签。菜单同时用它判断该不该置灰。
    fn scope_targets(&self, project_id: &str, anchor: &TabKey, scope: CloseScope) -> Vec<TabKey> {
        let Some(project) = self.projects.get(project_id) else {
            return Vec::new();
        };
        let Some(anchor_index) = project.index_of(anchor) else {
            return Vec::new();
        };
        let keys = project
            .tabs
            .iter()
            .map(|tab| tab.key.clone())
            .collect::<Vec<_>>();
        documents_in_scope(&keys, anchor_index, scope)
    }

    /// 页签右键的三档批量关闭。
    fn request_close_scope(
        &mut self,
        project_id: String,
        anchor: TabKey,
        scope: CloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.scope_targets(&project_id, &anchor, scope);
        self.request_close_batch(project_id, targets, Some(anchor), window, cx);
    }

    /// 终端页右键的「关闭所有」：关掉该项目全部页签(文件与 diff)。
    fn request_close_all(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self
            .projects
            .get(&project_id)
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| tab.key.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.request_close_batch(project_id, targets, None, window, cx);
    }

    /// 批量关闭入口。脏页签只弹**一次**确认（列出文件名），取消则一个都不关。
    fn request_close_batch(
        &mut self,
        project_id: String,
        targets: Vec<TabKey>,
        fallback: Option<TabKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            return;
        }
        let dirty = self
            .projects
            .get(&project_id)
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .filter(|tab| targets.contains(&tab.key) && tab.content.is_dirty(cx))
                    .map(|tab| tab.title.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if dirty.is_empty() {
            self.close_documents(&project_id, &targets, fallback.as_ref(), window, cx);
            return;
        }

        let this = cx.entity();
        Confirm::new(
            t("fileViewer", "unsavedTitle"),
            tr!("fileViewer", "unsavedBatchMessage", count = dirty.len()),
        )
        .detail(dirty_preview(&dirty))
        .open(
            move |window, cx| {
                let this = this.clone();
                let project_id = project_id.clone();
                let fallback = fallback.clone();
                let targets = targets.clone();
                window.defer(cx, move |window, cx| {
                    this.update(cx, |area, cx| {
                        area.close_documents(&project_id, &targets, fallback.as_ref(), window, cx)
                    });
                });
            },
            window,
            cx,
        );
    }

    /// 常驻终端页的右键菜单。终端页自己关不掉，这里只管清空页签区；
    /// 页签条只在有页签时才画，所以这一项不会没得可关。
    fn terminal_tab_menu(&self, project_id: &str, cx: &Context<Self>) -> Vec<MenuEntry> {
        let area = cx.entity();
        let project_id = project_id.to_string();
        vec![menu::item(
            t("fileViewer", "closeAll"),
            move |window, cx| {
                let project_id = project_id.clone();
                area.update(cx, |area, cx| {
                    area.request_close_all(project_id, window, cx)
                });
            },
        )]
    }

    /// 页签区页签(文件 / diff)的右键菜单。
    ///
    /// 终端页有自己那份（[`Self::terminal_tab_menu`]），也不出现在这里任何一档的
    /// 关闭范围里 —— 它是常驻页，关不掉（[`CloseScope`]）。
    fn document_tab_menu(
        &self,
        project_id: &str,
        key: &TabKey,
        cx: &Context<Self>,
    ) -> Vec<MenuEntry> {
        let area = cx.entity();
        let close = {
            let (area, project_id, key) = (area.clone(), project_id.to_string(), key.clone());
            MenuItem::new(t("fileViewer", "closeTab"))
                // 键位见 file_viewer/mod.rs 与 git_diff/tab.rs 的 on_key_down(Ctrl/Cmd+W)
                .shortcut(menu::hotkey_label(true, false, false, "W"))
                .on_click(move |window, cx| {
                    let (project_id, key) = (project_id.clone(), key.clone());
                    area.update(cx, |area, cx| {
                        area.request_close_document(project_id, key, window, cx)
                    });
                })
        };
        let mut entries = vec![close.into(), menu::separator()];
        for (scope, label) in [
            (CloseScope::Others, t("fileViewer", "closeOthers")),
            (CloseScope::ToRight, t("fileViewer", "closeToRight")),
            (CloseScope::ToLeft, t("fileViewer", "closeToLeft")),
        ] {
            // 没得可关就置灰(而不是抽掉那一项):菜单条目固定,肌肉记忆才稳
            let empty = self.scope_targets(project_id, key, scope).is_empty();
            let (area, project_id, key) = (area.clone(), project_id.to_string(), key.clone());
            entries.push(
                MenuItem::new(label)
                    .disabled(empty)
                    .on_click(move |window, cx| {
                        let (project_id, key) = (project_id.clone(), key.clone());
                        area.update(cx, |area, cx| {
                            area.request_close_scope(project_id, key, scope, window, cx)
                        });
                    })
                    .into(),
            );
        }
        entries
    }

    fn active_snapshot(&self, cx: &App) -> Option<ActiveWorkbenchSnapshot> {
        let project_id = self.store.read(cx).active_project_id.clone()?;
        let project = self.projects.get(&project_id);
        let active = project
            .map(|project| project.active.clone())
            .unwrap_or(WorkbenchPage::Terminal);
        let tabs = project
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| TabSnapshot {
                        key: tab.key.clone(),
                        title: tab.title.clone(),
                        tip: tab.tip.clone(),
                        content: tab.content.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some((project_id, active, tabs))
    }
}

impl Render for WorkbenchArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((project_id, active, tabs)) = self.active_snapshot(cx) else {
            self.last_rendered_project = None;
            self.last_rendered_page = None;
            return div().size_full().child(self.terminal_area.clone());
        };

        let page_changed = self.last_rendered_project.as_deref() != Some(project_id.as_str())
            || self.last_rendered_page.as_ref() != Some(&active);
        if page_changed {
            self.last_rendered_project = Some(project_id.clone());
            self.last_rendered_page = Some(active.clone());
            match &active {
                WorkbenchPage::Terminal => {
                    let area = cx.entity();
                    let store = self.store.clone();
                    let project_id = project_id.clone();
                    window.defer(cx, move |window, cx| {
                        if !area.read(cx).is_terminal_page_for_project(&project_id, cx)
                            || window.has_active_dialog(cx)
                            || !crate::overlay::allows(crate::overlay::Yield::ToOverlay)
                        {
                            return;
                        }
                        let pane_id = store.read(cx).active_pane_id(&project_id);
                        if let Some(pane_id) = pane_id {
                            store.update(cx, |store, cx| {
                                store.focus_pane(&project_id, &pane_id, window, cx)
                            });
                        }
                    });
                }
                WorkbenchPage::Tab(key) => {
                    if let Some(tab) = tabs.iter().find(|tab| &tab.key == key) {
                        tab.content.activate_later(window, cx);
                    }
                }
            }
            // 页签多到溢出时，刚激活的那一页可能整个在视野之外(尤其是新打开的
            // 文件排在最右)。滚动区里只有页签区的页签，序号即 tabs 下标；切回终端页
            // 不必动偏移 —— 那一颗常驻在滚动区左侧、任何偏移下都在视野里。
            if let WorkbenchPage::Tab(key) = &active
                && let Some(index) = tabs.iter().position(|tab| &tab.key == key)
            {
                self.tab_scroll.scroll_to_item(index);
            }
        }

        // 尚未打开页签时保持原终端区的尺寸与结构，一旦有页签才出现工作区页签条。
        if tabs.is_empty() {
            return div().size_full().child(self.terminal_area.clone());
        }

        let terminal_active = active == WorkbenchPage::Terminal;
        let terminal_menu_project = project_id.clone();
        // 常驻终端页**钉在最左**,不进滚动区 —— 它是回终端的唯一入口,被文件 / diff 页签
        // 挤出视野后要先横向滚回去才点得到。
        let terminal_tab = div()
            .id("workbench-tab-terminal")
            .h_full()
            .flex_none()
            .min_w(px(110.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            // 13 而不是 12:这是「页」的一层,要压得住下面那条终端 tab 栏
            .text_size(ui::font_px(TAB_FONT))
            .when(terminal_active, |el| {
                el.bg(ui::bg_terminal())
                    .text_color(ui::text_primary())
                    .border_t_2()
                    .border_color(ui::accent())
            })
            .when(!terminal_active, |el| {
                el.text_color(ui::text_muted())
                    .border_t_2()
                    .border_color(gpui::Hsla {
                        a: 0.0,
                        ..ui::accent()
                    })
            })
            .child(t("terminalArea", "terminal"))
            .on_click(cx.listener(|this, _event, window, cx| this.activate_terminal(window, cx)))
            // 右键菜单:关闭所有页签(文件与 diff)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = this.terminal_tab_menu(&terminal_menu_project, cx);
                    menu::show(event.position, entries, window, cx);
                }),
            );

        // 页签横向滚动区(与终端 tab 栏同一套):页签**不压缩**,溢出即可横向滚。
        //
        // 垂直滚轮不必自己映射 —— gpui 只在 `overflow.x == Scroll && overflow.y
        // != Scroll` 且 `restrict_scroll_to_axis == false`(默认)时把 `delta.y`
        // 记到 x 上(gpui-0.2.2 `elements/div.rs`)。`track_scroll` 是为了能主动
        // 把活动页拉进视野。
        let mut doc_tabs = div()
            .id("workbench-tabs")
            .size_full()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            // 悬停即出滚动条(下面那道 `tabs_hovered` 判据)。只记一个 bool、
            // 不回读实体,不碰 cx.listener 内禁止再 update 自身那条铁律。
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if this.tabs_hovered != *hovered {
                    this.tabs_hovered = *hovered;
                    cx.notify();
                }
            }));

        for tab in &tabs {
            let TabSnapshot {
                key,
                title,
                tip,
                content,
            } = tab;
            let selected = active == WorkbenchPage::Tab(key.clone());
            let dirty = content.is_dirty(cx);
            // 文件页签按文件类型上色的图标;diff 页签一律 accent 色的 Git 图标(diff / 提交 /
            // 合并 / 历史四种形状),一眼分得开
            let icon: AnyElement = match key {
                TabKey::Document(_) => FileIcon::new(title, false, false)
                    .size(px(14.0))
                    .into_any_element(),
                TabKey::Diff(diff) => {
                    git_ui::icon(diff.icon(), 14.0, ui::accent()).into_any_element()
                }
            };
            let tab_key = key.clone();
            let close_key = key.clone();
            let middle_key = key.clone();
            let menu_key = key.clone();
            let click_project = project_id.clone();
            let close_project = project_id.clone();
            let middle_project = project_id.clone();
            let menu_project = project_id.clone();
            doc_tabs = doc_tabs.child(
                div()
                    .id(SharedString::from(format!(
                        "workbench-tab-{:016x}",
                        stable_hash(key)
                    )))
                    .h_full()
                    .flex_none()
                    .min_w(px(120.0))
                    .max_w(px(220.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(TAB_FONT))
                    .when(selected, |el| {
                        // 与文档页容器同色:背景图皮肤下随内容区一起半透明,
                        // 不透明的 bg_base 会在半透明页签条上凸成一块实色
                        el.bg(ui::bg_document())
                            .text_color(ui::text_primary())
                            .border_t_2()
                            .border_color(ui::accent())
                    })
                    .when(!selected, |el| {
                        el.text_color(ui::text_muted())
                            .border_t_2()
                            .border_color(gpui::Hsla {
                                a: 0.0,
                                ..ui::accent()
                            })
                    })
                    .child(icon)
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .child(title.clone()),
                    )
                    // diff 页签悬停给完整路径 / 提交信息(标题只放得下文件名与短哈希)
                    .when_some(tip.clone(), |el, tip| el.tip(tip))
                    .when(dirty, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!(
                                    "workbench-tab-dirty-{:016x}",
                                    stable_hash(key)
                                )))
                                .w(px(6.0))
                                .h(px(6.0))
                                .flex_none()
                                .rounded_full()
                                .bg(ui::accent())
                                .tip(t("fileViewer", "unsaved")),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "workbench-tab-close-{:016x}",
                                stable_hash(key)
                            )))
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .text_color(ui::text_muted())
                            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                            .child("×")
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                this.request_close_document(
                                    close_project.clone(),
                                    close_key.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.activate_document(&click_project, &tab_key, window, cx)
                    }))
                    // 中键关闭(浏览器 / IDE 的惯例,松开时关;与 × 同一条路,脏页签照样确认)
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(move |this, _event: &MouseUpEvent, window, cx| {
                            cx.stop_propagation();
                            this.request_close_document(
                                middle_project.clone(),
                                middle_key.clone(),
                                window,
                                cx,
                            );
                        }),
                    )
                    // 页签右键菜单:关闭 / 关闭其他 / 关闭右边 / 关闭左边。
                    // 常驻的终端页有自己那份(terminal_tab_menu) —— 它关不掉
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let entries = this.document_tab_menu(&menu_project, &menu_key, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    ),
            );
        }

        // 横向滚动条:悬停(或正在拖)且**真溢出**时才画。几何全部来自上一帧量的
        // `bounds`/`max_offset`(首帧为零 → 不画,下一帧补上)。
        //
        // 自绘而不用 `gpui_component::scroll::Scrollbar`:那颗的 thumb 粗细与内衬
        // 是 crate 内部常量(8px thumb + 上下各 4px = 16px 高的一整条带,还自带
        // 轨道底色),压在 34px 高的页签条上占掉半层。这里只要一根细 thumb。
        let dragging = self.tab_drag.is_some();
        let thumb = (self.tabs_hovered || dragging)
            .then(|| {
                tab_thumb_geometry(
                    f32::from(self.tab_scroll.bounds().size.width),
                    f32::from(self.tab_scroll.max_offset().x),
                    f32::from(self.tab_scroll.offset().x),
                )
            })
            .flatten();
        let tab_bar = div()
            .h(px(TAB_BAR_H))
            .flex_none()
            .flex()
            .items_center()
            .bg(ui::bg_elevated())
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(terminal_tab)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child(doc_tabs)
                    .children(thumb.map(|(width, left)| {
                        // 外层是拖拽热区(10px),可见的那根只有 4px —— 直接给
                        // thumb 4px 高会难以按中。热区透明,不挡页签的点击:
                        // 它只贴在底缘,页签的文字与关闭按钮都在其上方。
                        div()
                            .id("workbench-tabs-thumb")
                            .absolute()
                            .bottom_0()
                            .left(px(left))
                            .w(px(width))
                            .h(px(TAB_THUMB_HIT_HEIGHT))
                            .flex()
                            .items_end()
                            .pb(px(2.0))
                            .cursor_pointer()
                            .child(
                                div()
                                    .w_full()
                                    .h(px(TAB_THUMB_HEIGHT))
                                    .rounded(px(TAB_THUMB_HEIGHT / 2.0))
                                    .bg(ui::with_alpha(
                                        ui::text_muted(),
                                        if dragging {
                                            TAB_THUMB_ALPHA_DRAG
                                        } else {
                                            TAB_THUMB_ALPHA
                                        },
                                    )),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.tab_drag =
                                        Some((event.position.x, this.tab_scroll.offset().x));
                                    cx.notify();
                                }),
                            )
                    })),
            );

        let body = match &active {
            WorkbenchPage::Terminal => self.terminal_area.clone().into_any_element(),
            WorkbenchPage::Tab(key) => tabs
                .iter()
                .find(|tab| &tab.key == key)
                .map(|tab| tab.content.view())
                .unwrap_or_else(|| self.terminal_area.clone().into_any_element()),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            // 页签滚动条的拖拽在**根容器**上收尾:thumb 只有几像素高,鼠标一抖
            // 就滑出去了,监听挂在它身上会拖两下断一次(理由见 on_tab_scroll_drag)
            .on_mouse_move(cx.listener(Self::on_tab_scroll_drag))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    if this.tab_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(tab_bar)
            .child(div().flex_1().min_h(px(0.0)).overflow_hidden().child(body))
    }
}

/// 「页签横向溢出了没有」的判据阈值。取 0.5px 而不是 0：taffy 的宽度是浮点，
/// 内容刚好填满时 `max_offset` 常残一丝亚像素，按 0 判会让滚动条无谓地闪出来。
const SCROLLBAR_EPSILON: f32 = 0.5;

/// 工作台页签条的高度 = 左栏「项目」标题行的高度(`ui::PANEL_HEADER_H`):
/// 两者并排在同一水平线上,不一样高时底边线在分栏处会断成两截。
///
/// 这一层是「页」(终端页 / 各个文档页),它下面还叠着终端自己的叶子 tab 栏;
/// 层级靠字号(13 对 12)与形态(顶线页签对圆角胶囊)拉开,不靠高度。
const TAB_BAR_H: f32 = ui::PANEL_HEADER_H;
/// 工作台页签的字号。比叶子 tab 栏的 12 大一档,理由同 [`TAB_BAR_H`]。
const TAB_FONT: f32 = 13.0;

/// 可见 thumb 的粗细。页签条统共 36px 高，再粗就压掉一层。
const TAB_THUMB_HEIGHT: f32 = 4.0;
/// thumb 的拖拽热区高度（外层透明壳）。4px 的可见条太难按中。
const TAB_THUMB_HIT_HEIGHT: f32 = 10.0;
/// thumb 再短也要够得着。
const TAB_THUMB_MIN_WIDTH: f32 = 24.0;
/// thumb 平时/拖动中的不透明度。它压在页签文字底下，安静一点。
const TAB_THUMB_ALPHA: f32 = 0.28;
const TAB_THUMB_ALPHA_DRAG: f32 = 0.5;

/// 页签滚动条 thumb 的 **(长度, 左偏)**，单位 px。
///
/// `container` 是滚动区可视宽度，`max_offset` 是还能往左滚多少，`offset` 是 gpui
/// 记的当前偏移（**向左滚为负**）。不溢出（或首帧还没量到尺寸）时返回 `None`，
/// 调用方据此整条不画。
fn tab_thumb_geometry(container: f32, max_offset: f32, offset: f32) -> Option<(f32, f32)> {
    if container <= 0.0 || max_offset <= SCROLLBAR_EPSILON {
        return None;
    }
    let content = container + max_offset;
    let thumb =
        (container * container / content).clamp(TAB_THUMB_MIN_WIDTH.min(container), container);
    let track = container - thumb;
    let progress = ((-offset) / max_offset).clamp(0.0, 1.0);
    Some((thumb, track * progress))
}

/// 拖 thumb 时把鼠标位移 `delta` 换算成新的滚动偏移（向左为负，已夹在可滚范围内）。
///
/// 换算比例是**轨道剩余长度**对 `max_offset`，不是容器宽对内容宽 —— thumb 走完
/// 轨道正好等于内容滚到底，用后者会让指针跑在 thumb 前面。
fn tab_drag_offset(container: f32, max_offset: f32, anchor_offset: f32, delta: f32) -> f32 {
    let Some((thumb, _)) = tab_thumb_geometry(container, max_offset, anchor_offset) else {
        return anchor_offset;
    };
    let track = container - thumb;
    if track <= 0.0 {
        return anchor_offset;
    }
    (anchor_offset - delta * max_offset / track).clamp(-max_offset, 0.0)
}

/// 批量关闭确认框里列出的未保存文件名。与关窗确认同一套口径：只列前几条，
/// 其余折成一行「另有 N 项」，免得一次关几十个页签时确认框长到出屏。
const DIRTY_PREVIEW_LIMIT: usize = 5;

fn dirty_preview(names: &[String]) -> Vec<String> {
    let mut lines = names
        .iter()
        .take(DIRTY_PREVIEW_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = names.len().saturating_sub(lines.len());
    if remaining > 0 {
        lines.push(tr!("app", "closeConfirm.remaining", count = remaining));
    }
    lines
}

fn stable_hash(key: &TabKey) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> TabKey {
        TabKey::Document(DocumentKey {
            project_id: "p".into(),
            backend: DocumentBackendKey::Local,
            normalized_path: path.into(),
        })
    }

    fn page(path: &str) -> WorkbenchPage {
        WorkbenchPage::Tab(key(path))
    }

    /// diff 页签换了身份:没有撞上别的页签时只是活动页跟着新身份走(原本停在它上面的话)。
    #[test]
    fn 换身份时活动页跟着走() {
        let keys = [key("a"), key("b"), key("c")];
        assert_eq!(
            retarget_plan(&keys, &page("b"), 1, &key("x")),
            (None, page("x"))
        );
        // 活动页是别的页签 / 终端页:不动
        assert_eq!(
            retarget_plan(&keys, &page("a"), 1, &key("x")),
            (None, page("a"))
        );
        assert_eq!(
            retarget_plan(&keys, &WorkbenchPage::Terminal, 1, &key("x")),
            (None, WorkbenchPage::Terminal)
        );
        // 身份没变(只是标题刷新):什么都不关
        assert_eq!(
            retarget_plan(&keys, &page("b"), 1, &key("b")),
            (None, page("b"))
        );
    }

    /// 换到的目标已经有另一个页签:换文件的这个接管,旧的那个要关掉;活动页仍是换文件的这个
    /// (身份已是新的)。原本就停在旧的那个上面时,活动页落到接管的这个身上(同一身份)。
    #[test]
    fn 换到已开着的目标时关掉旧页签() {
        let keys = [key("a"), key("b"), key("c")];
        assert_eq!(
            retarget_plan(&keys, &page("b"), 1, &key("c")),
            (Some(2), page("c"))
        );
        assert_eq!(
            retarget_plan(&keys, &page("b"), 1, &key("a")),
            (Some(0), page("a"))
        );
        assert_eq!(
            retarget_plan(&keys, &page("c"), 1, &key("c")),
            (Some(2), page("c"))
        );
        // 下标越界(不该出现,防御):什么都不动
        assert_eq!(
            retarget_plan(&keys, &page("b"), 9, &key("a")),
            (None, page("b"))
        );
    }

    #[test]
    fn document_key_separates_remote_connection_identity() {
        let a = DocumentKey {
            project_id: "p".into(),
            backend: DocumentBackendKey::Remote {
                connection_id: "ssh".into(),
                connection_fingerprint: 1,
            },
            normalized_path: "/work/a.rs".into(),
        };
        let mut b = a.clone();
        b.backend = DocumentBackendKey::Remote {
            connection_id: "ssh".into(),
            connection_fingerprint: 2,
        };
        assert_ne!(a, b);
    }

    #[cfg(windows)]
    #[test]
    fn normalized_paths_deduplicate_separator_variants_on_windows() {
        let normalized = normalize_local_document_path(Path::new("C:\\Work\\src\\main.rs"));
        assert!(!normalized.contains('\\'));
    }

    #[cfg(not(windows))]
    #[test]
    fn local_posix_paths_preserve_backslash_file_names() {
        assert_ne!(
            normalize_local_document_path(Path::new("/work/a\\b.rs")),
            normalize_local_document_path(Path::new("/work/a/b.rs"))
        );
    }

    #[test]
    fn remote_paths_remain_case_sensitive_on_windows_hosts() {
        assert_ne!(
            normalize_remote_document_path(Path::new("/work/A.rs")),
            normalize_remote_document_path(Path::new("/work/a.rs"))
        );
        assert_ne!(
            normalize_remote_document_path(Path::new("/work/a\\b.rs")),
            normalize_remote_document_path(Path::new("/work/a/b.rs"))
        );
    }

    #[test]
    fn close_scope_never_includes_the_anchor_itself() {
        let keys = [key("a"), key("b"), key("c")];
        for scope in [CloseScope::Others, CloseScope::ToRight, CloseScope::ToLeft] {
            assert!(
                !documents_in_scope(&keys, 1, scope).contains(&key("b")),
                "{scope:?} 不该关掉被右键的那一页"
            );
        }
    }

    #[test]
    fn close_scope_splits_by_position() {
        let keys = [key("a"), key("b"), key("c")];
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::Others),
            vec![key("a"), key("c")]
        );
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::ToRight),
            vec![key("c")]
        );
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::ToLeft),
            vec![key("a")]
        );
    }

    /// 两端的页签各有一档「没得可关」——菜单据此置灰，而不是抽掉那一项。
    #[test]
    fn close_scope_is_empty_at_the_edges() {
        let keys = [key("a"), key("b")];
        assert!(documents_in_scope(&keys, 0, CloseScope::ToLeft).is_empty());
        assert!(documents_in_scope(&keys, 1, CloseScope::ToRight).is_empty());
        assert!(documents_in_scope(&[key("a")], 0, CloseScope::Others).is_empty());
    }

    /// 不溢出（含首帧还没量到尺寸）就整条不画 —— 空轨道比没有更碍眼。
    #[test]
    fn 页签滚动条只在真溢出时出现() {
        assert_eq!(tab_thumb_geometry(0.0, 0.0, 0.0), None, "首帧没尺寸");
        assert_eq!(tab_thumb_geometry(400.0, 0.0, 0.0), None, "没溢出");
        assert_eq!(
            tab_thumb_geometry(400.0, SCROLLBAR_EPSILON, 0.0),
            None,
            "亚像素残差不算溢出"
        );
        assert!(tab_thumb_geometry(400.0, 200.0, 0.0).is_some());
    }

    /// thumb 长度 = 可视占内容的比例，两端分别贴住轨道两头。
    #[test]
    fn 页签滚动条按比例取长并走满轨道() {
        // 可视 400 / 内容 600 → thumb 占 2/3
        let (width, left) = tab_thumb_geometry(400.0, 200.0, 0.0).expect("该有 thumb");
        assert!((width - 400.0 * 400.0 / 600.0).abs() < 1e-3);
        assert_eq!(left, 0.0, "没滚时贴左");

        // 滚到底（偏移为负的 max）时 thumb 右缘正好压住轨道右端
        let (width_end, left_end) = tab_thumb_geometry(400.0, 200.0, -200.0).expect("该有 thumb");
        assert!((width_end - width).abs() < 1e-3, "长度不随偏移变");
        assert!((left_end + width_end - 400.0).abs() < 1e-3);

        // 内容长到 thumb 会缩成一条线时，用最短长度兜底
        let (tiny, _) = tab_thumb_geometry(400.0, 40_000.0, 0.0).expect("该有 thumb");
        assert!((tiny - TAB_THUMB_MIN_WIDTH).abs() < 1e-3);
    }

    /// 拖动换算的比例基准是**轨道剩余长度**：thumb 拖到轨道尽头 = 内容滚到底。
    /// 按「容器宽 : 内容宽」换算的话指针会跑在 thumb 前面。
    #[test]
    fn 页签滚动条拖动与轨道等长并夹在可滚范围内() {
        let (width, _) = tab_thumb_geometry(400.0, 200.0, 0.0).expect("该有 thumb");
        let track = 400.0 - width;
        // 从最左把 thumb 拖满整条轨道
        assert!((tab_drag_offset(400.0, 200.0, 0.0, track) + 200.0).abs() < 1e-3);
        // 半程即一半偏移
        assert!((tab_drag_offset(400.0, 200.0, 0.0, track / 2.0) + 100.0).abs() < 1e-3);
        // 两端都夹住：往左拖过头停在 0，往右拖过头停在 -max
        assert_eq!(tab_drag_offset(400.0, 200.0, 0.0, -999.0), 0.0);
        assert_eq!(tab_drag_offset(400.0, 200.0, -200.0, 999.0), -200.0);
        // 没溢出时拖不动
        assert_eq!(tab_drag_offset(400.0, 0.0, 0.0, 50.0), 0.0);
    }

    #[test]
    fn dirty_preview_folds_the_long_tail() {
        let names = (0..8).map(|i| format!("f{i}.rs")).collect::<Vec<_>>();
        let lines = dirty_preview(&names);
        assert_eq!(lines.len(), DIRTY_PREVIEW_LIMIT + 1);
        assert_eq!(lines[..DIRTY_PREVIEW_LIMIT], names[..DIRTY_PREVIEW_LIMIT]);
        assert!(lines[DIRTY_PREVIEW_LIMIT].contains('3'), "剩余 3 项要算准");
        assert_eq!(dirty_preview(&names[..2]), names[..2].to_vec());
    }

    #[test]
    fn closing_active_document_prefers_right_neighbor_then_left() {
        assert_eq!(
            next_document_after_close(&[key("a"), key("c")], 1),
            Some(key("c"))
        );
        assert_eq!(
            next_document_after_close(&[key("a"), key("b")], 2),
            Some(key("b"))
        );
        assert_eq!(next_document_after_close(&[], 0), None);
    }

    /// 最近打开:新的在前、重复的挪到最前而不是再记一条、封顶;Windows 上只差大小写算同一个。
    #[test]
    fn 最近打开去重挪前并封顶() {
        let mut recent = Vec::new();
        push_recent(&mut recent, Path::new("/p/a.rs"), 3);
        push_recent(&mut recent, Path::new("/p/b.rs"), 3);
        push_recent(&mut recent, Path::new("/p/a.rs"), 3);
        assert_eq!(recent, [PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")]);
        push_recent(&mut recent, Path::new("/p/c.rs"), 3);
        push_recent(&mut recent, Path::new("/p/d.rs"), 3);
        assert_eq!(
            recent,
            [
                PathBuf::from("/p/d.rs"),
                PathBuf::from("/p/c.rs"),
                PathBuf::from("/p/a.rs")
            ]
        );
        if cfg!(windows) {
            push_recent(&mut recent, Path::new(r"\P\A.RS"), 3);
            assert_eq!(recent.len(), 3, "只差大小写与分隔符的是同一个文件");
            assert_eq!(recent[0], PathBuf::from(r"\P\A.RS"));
        }
    }
}
