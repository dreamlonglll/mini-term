//! 项目相关的 `AppStore` 方法:目录技术栈探测、项目 CRUD、项目分组。
//!
//! 从 `store.rs` 原样搬来的三段(`// === 目录技术栈探测 ===` /
//! `// === 项目 ===` / `// === 项目分组 ===`),段注释随代码走,逻辑一行未改。

use std::collections::HashSet;
use std::path::Path;

use gpui::{Context, Window};
use mt_config::{AppConfig, ProjectConfig};
use mt_project::project_kind::ProjectKind;

use crate::project_tree;
use crate::tree::gen_unique_id;

use super::events::StoreChanged;
use super::pure::remove_from_tree;
use super::{AppStore, ProjectState, StoreEvent};

/// Removing a parent also affects the recovery entry of its nested projects.
fn removal_source_pending<'a>(
    config: &'a AppConfig,
    id: &str,
    mut pending: impl FnMut(&ProjectConfig) -> bool,
) -> Option<&'a ProjectConfig> {
    config.projects.iter().find(|project| {
        let mut current = *project;
        for _ in 0..config.projects.len() {
            if current.id == id {
                return pending(project);
            }
            let Some(parent) = current
                .parent_project_id
                .as_deref()
                .and_then(|parent| config.projects.iter().find(|project| project.id == parent))
            else {
                break;
            };
            current = parent;
        }
        false
    })
}

/// Credentials and display metadata can change without redirecting a recovery entry.
fn remote_git_source_changed(original: &AppConfig, next: &AppConfig, project_id: &str) -> bool {
    let Some(before) = original
        .projects
        .iter()
        .find(|project| project.id == project_id)
    else {
        return false;
    };
    let Some(after) = next
        .projects
        .iter()
        .find(|project| project.id == project_id)
    else {
        return true;
    };
    if before.path != after.path || before.ssh_connection_id != after.ssh_connection_id {
        return true;
    }
    let mut ancestor = before;
    for _ in 0..original.projects.len() {
        let Some(parent) = ancestor
            .parent_project_id
            .as_deref()
            .and_then(|id| original.projects.iter().find(|project| project.id == id))
        else {
            break;
        };
        if !next.projects.iter().any(|project| project.id == parent.id) {
            return true;
        }
        ancestor = parent;
    }
    let before_connection = original
        .ssh_connections
        .iter()
        .find(|connection| before.ssh_connection_id.as_deref() == Some(connection.id.as_str()));
    let after_connection = next
        .ssh_connections
        .iter()
        .find(|connection| after.ssh_connection_id.as_deref() == Some(connection.id.as_str()));
    match (before_connection, after_connection) {
        (Some(before), Some(after)) => remote_git_endpoint_changed(before, after),
        (None, None) => false,
        _ => true,
    }
}

pub(super) fn remote_git_endpoint_changed(
    before: &mt_config::SshConnection,
    after: &mt_config::SshConnection,
) -> bool {
    before.host != after.host || before.port != after.port || before.user != after.user
}

/// Stage only while a recovery source is busy; rejected patches never touch live config.
pub(super) fn patch_preserving_remote_git_sources(
    config: &mut AppConfig,
    pending_sources: &[String],
    edit: impl FnOnce(&mut AppConfig),
) -> Option<String> {
    if pending_sources.is_empty() {
        edit(config);
        return None;
    }
    let mut next = config.clone();
    edit(&mut next);
    if let Some(id) = pending_sources
        .iter()
        .find(|id| remote_git_source_changed(config, &next, id))
    {
        return Some(id.clone());
    }
    *config = next;
    None
}

impl AppStore {
    /// Bypass consent gating here: revoking consent must not hide an in-flight/unknown write.
    pub(super) fn remote_git_source_pending(&self, project: &ProjectConfig) -> bool {
        let Some(connection) = self.config.ssh_connections.iter().find(|connection| {
            project.ssh_connection_id.as_deref() == Some(connection.id.as_str())
        }) else {
            return false;
        };
        let context = mt_remote::GitContext::remote(
            project.id.clone(),
            connection.clone(),
            project.path.clone(),
        );
        context.write_running() || context.unknown_write().is_some()
    }

    pub(super) fn notify_remote_git_source_blocked(
        &self,
        project_id: &str,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .project(project_id)
            .map(|project| project.name.as_str())
            .unwrap_or(project_id);
        crate::toast::push_message_deduped(
            crate::notify::ToastKind::PasteError,
            project_id.to_string(),
            name.to_string(),
            crate::i18n::t("gitHistory", "recoverySourceBlocked").to_string(),
            cx,
        );
    }

    /// 新项目 id:对 `projects` **与** `projectTree` 双双去重。树里也要查 ——
    /// 树上可能残留指向已删项目的 id,撞上会让新项目凭空出现在残留的位置。
    /// 背景见 [`gen_unique_id`] / `tree::gen_id` 的注释(曾把库里既有项目顶掉)。
    pub(super) fn fresh_project_id(&self) -> String {
        gen_unique_id("proj", |id| {
            self.config.projects.iter().any(|p| p.id == id)
                || self
                    .config
                    .project_tree
                    .as_deref()
                    .is_some_and(|tree| project_tree::contains_id(tree, id))
        })
    }

    /// 新分组 id:只需对树去重(分组只活在树里)。
    fn fresh_group_id(&self) -> String {
        gen_unique_id("group", |id| {
            self.config
                .project_tree
                .as_deref()
                .is_some_and(|tree| project_tree::contains_id(tree, id))
        })
    }

    // === 目录技术栈探测(`useProjectKinds.ts`) ===

    /// 读缓存。`None` = 还没探过;`Some(None)` = 探过但识别不出。
    pub fn dir_kind(&self, path: &str) -> Option<Option<ProjectKind>> {
        self.dir_kinds.get(path).copied()
    }

    /// 批量探测(去重 + 带缓存)。**只接本地路径**,远程由调用方跳过。
    ///
    /// 每条路径一个后台任务:`detect_local` 要读目录、可能还读 `package.json`,
    /// 在主线程上跑会把网络盘/WSL 上的一次悬停做成秒级卡顿。
    pub fn ensure_dir_kinds(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        for path in paths {
            if self.dir_kinds.contains_key(&path) || !self.dir_kinds_pending.insert(path.clone()) {
                continue;
            }
            cx.spawn(async move |this, cx| {
                let probe = path.clone();
                let kind = cx
                    .background_executor()
                    .spawn(async move {
                        mt_project::project_kind::detect_local(std::path::Path::new(&probe))
                    })
                    .await;
                let _ = this.update(cx, |store: &mut AppStore, cx| {
                    store.dir_kinds_pending.remove(&path);
                    store.set_dir_kind(path, kind, cx);
                });
            })
            .detach();
        }
    }

    /// 写缓存并通知(`setDirKind`)。识别不出也要写 —— 否则每帧重探。
    pub fn set_dir_kind(&mut self, path: String, kind: Option<ProjectKind>, cx: &mut Context<Self>) {
        self.dir_kinds.insert(path, kind);
        cx.changed(StoreEvent::DirKindsChanged);
    }

    /// 失效(`removeDirKind`):项目根的标记文件变动时调。下一轮 `ensure` 会重探。
    pub fn remove_dir_kind(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.dir_kinds.remove(path).is_some() {
            cx.changed(StoreEvent::DirKindsChanged);
        }
    }

    // === 项目 ===

    pub fn set_active_project(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.active_project_id.as_deref() == Some(id) {
            return;
        }
        self.active_project_id = Some(id.to_string());
        if let Some(state) = self.project_states.get_mut(id) {
            state.needs_attention = false;
        }
        self.config.last_active_project_id = Some(id.to_string());
        // 切过去才起 PTY:恢复出来的布局在这一刻补齐(旧版的懒创建时机)
        self.hydrate_project(id, cx);
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ActiveProjectChanged);
    }

    /// 添加项目之后的「打开」:切过去,并且**一个终端都没有**时用默认 shell
    /// 开一个并聚焦 —— 省掉「切过去 → 面对空态页 → 再点一次新建终端」那一步。
    ///
    /// 只给「添加」这条路用,**别塞进 [`set_active_project`]**:用户手动关光某个
    /// 项目的终端再切回去,空态是他自己要的,不该被自动补一个。路径撞上既有项目
    /// 时(`add_project*` 返回旧 id)也走这里 —— 旧项目要是有终端就只切不开。
    ///
    /// 末尾那句揭示:新项目追加在根层末尾(列表一长就在可视区下面),worktree 子项目
    /// 跟着父项目可能落在折叠组里,不揭示的话「加完看不见,像是没生效」。它排到本轮
    /// effect 之后才跑(见 [`crate::project_list::reveal`]),此刻拿着 store 的租约也不怕。
    ///
    /// [`set_active_project`]: Self::set_active_project
    pub fn open_added_project(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.set_active_project(id, cx);
        let has_terminal = self
            .project_states
            .get(id)
            .is_some_and(|s| !s.panels.is_empty());
        if !has_terminal {
            self.new_terminal(id, None, None, window, cx);
        }
        crate::project_list::reveal(id, cx);
    }

    /// 按路径找项目(`store.ts::findProjectByPath`)。
    ///
    /// 比对走 [`windows_eq_key`](mt_core::path_key::windows_eq_key)(分隔符统一 +
    /// 去尾斜杠 + 转小写),与 worktree「是否已是项目」的判据同一份。
    /// SSH 远程项目排除在外 —— worktree 的路径是本机路径。
    pub fn find_project_by_path(&self, path: &str) -> Option<&ProjectConfig> {
        use mt_core::path_key::windows_eq_key;
        let target = windows_eq_key(path);
        self.config
            .projects
            .iter()
            .find(|p| p.ssh_connection_id.is_none() && windows_eq_key(&p.path) == target)
    }

    /// Git worktrees are identified by execution backend as well as path.
    pub fn find_project_by_backend_path(
        &self,
        connection_id: Option<&str>,
        path: &str,
    ) -> Option<&ProjectConfig> {
        match connection_id {
            None => self.find_project_by_path(path),
            Some(connection_id) => self.config.projects.iter().find(|project| {
                project.ssh_connection_id.as_deref() == Some(connection_id)
                    && project.path.trim_end_matches('/') == path.trim_end_matches('/')
            }),
        }
    }

    pub fn add_git_worktree_project(
        &mut self,
        name: String,
        path: String,
        connection_id: Option<String>,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) -> String {
        if let Some(existing) = self.find_project_by_backend_path(connection_id.as_deref(), &path) {
            return existing.id.clone();
        }
        let Some(connection_id) = connection_id else {
            return self.add_project_at(Path::new(&path), parent.as_deref(), cx);
        };
        let id = self.fresh_project_id();
        let parent = parent.filter(|id| {
            self.config.projects.iter().any(|project| {
                project.id == *id
                    && project.ssh_connection_id.as_deref() == Some(connection_id.as_str())
            })
        });
        self.config.projects.push(ProjectConfig {
            ssh_connection_id: Some(connection_id),
            parent_project_id: parent.clone(),
            ..ProjectConfig::new(id.clone(), name, path)
        });
        if parent.is_none() {
            self.config
                .project_tree
                .get_or_insert_with(Vec::new)
                .push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        }
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.save_config_soon(cx);
        if parent.is_none() {
            cx.emit(StoreEvent::ProjectTreeChanged);
        }
        cx.changed(StoreEvent::ProjectsChanged);
        id
    }

    /// 添加项目并**返回它的 id**;`parent` 非空时挂成子项目。
    ///
    /// 对应 `store.ts:777-799` 的 `addProject(project, parentProjectId)`:
    /// - 父项目必须真实存在,否则回落为普通顶层项目(防止产生渲染不出来的孤儿);
    /// - **子项目不进 `projectTree`**(移动出去时才转成普通树节点);
    /// - 路径已经是项目 → 返回既有 id,不重复添加(`GitWorktreeModal.tsx:341-351`)。
    ///
    /// **只添加、不切过去**:调用方拿到 id 后自己决定 —— 用户主动添加的各入口
    /// 接 [`open_added_project`](Self::open_added_project)(切过去 + 开首个终端)。
    /// 这是全部本地项目的唯一添加口(原先另有一个不带父项目、自动切过去的
    /// `add_project`,查重还是逐字比对路径,已并入这里)。
    pub fn add_project_at(
        &mut self,
        path: &Path,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) -> String {
        let path_str = path.to_string_lossy().to_string();
        if let Some(existing) = self.find_project_by_path(&path_str).map(|p| p.id.clone()) {
            return existing;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path_str.clone());
        let id = self.fresh_project_id();
        let parent_ok = parent.filter(|pid| self.config.projects.iter().any(|p| p.id == *pid));

        self.config.projects.push(ProjectConfig {
            parent_project_id: parent_ok.map(str::to_string),
            ..ProjectConfig::new(id.clone(), name, path_str)
        });
        if parent_ok.is_none() {
            let tree = self.config.project_tree.get_or_insert_with(Vec::new);
            tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        }
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.save_config_soon(cx);
        if parent_ok.is_none() {
            cx.emit(StoreEvent::ProjectTreeChanged);
        }
        cx.changed(StoreEvent::ProjectsChanged);
        id
    }

    /// 只回收某个项目的终端,**不删项目**(`projectActions.ts:25-32` 的
    /// `disposeProjectTerminals`)。
    ///
    /// worktree 删除必须先走这一步:Windows 上 shell 占着目录会让
    /// `git worktree remove` 直接失败。
    pub fn dispose_project_terminals(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let pty_ids: Vec<u32> = self
            .project_states
            .get(project_id)
            .map(|s| s.pty_ids())
            .unwrap_or_default();
        for pty_id in pty_ids {
            self.dispose_terminal(pty_id, cx);
        }
        cx.changed(StoreEvent::LayoutChanged);
    }

    /// 改项目显示名(`store.ts::renameProject`)。空名不接受 —— 列表上会变成
    /// 一行只有路径的空条目,而原版的内联重命名框同样在空串时直接放弃。
    pub fn rename_project(&mut self, id: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.name == name {
            return;
        }
        project.name = name.to_string();
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectsChanged);
    }

    /// 设置项目需求描述;空串 = 清除(`store.ts::setProjectDescription` 的
    /// `description || undefined` 同语义 —— 存空串会让 `skip_serializing_if`
    /// 失效,配置文件里留一堆 `"description": ""`)。
    pub fn set_project_description(&mut self, id: &str, description: &str, cx: &mut Context<Self>) {
        let next = match description.trim() {
            "" => None,
            text => Some(text.to_string()),
        };
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.description == next {
            return;
        }
        project.description = next;
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectsChanged);
    }

    /// Persist explicit helper consent and synchronously update the deny-by-default registry.
    /// The caller obtains consent through git_backend::request_enable before enabling.
    pub fn set_project_remote_git_enabled(
        &mut self,
        project_id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return;
        };
        let connection = self.config.ssh_connections.iter().find(|connection| {
            project.ssh_connection_id.as_deref() == Some(connection.id.as_str())
        });
        project.set_remote_git_enabled(connection, enabled);
        mt_remote::set_project_git_permission(
            project_id,
            connection,
            &project.path,
            connection.is_some_and(|connection| project.remote_git_enabled(connection)),
        );
        self.save_config_now();
        cx.changed(StoreEvent::ProjectsChanged);
    }

    /// 项目级环境变量(`ProjectEnvVarsModal` 的落盘那一半)。
    ///
    /// **立即落盘**而不是 500ms 防抖:整屏手填的键值对不该在防抖窗口里被一次
    /// 崩溃吃掉(与 SSH 连接同一条理由)。入参已由弹窗清洗过 —— 这里不做校验,
    /// 校验的唯一实现在 `env_vars::compute_errors`(单测钉死)。
    ///
    /// 生效面:只影响**之后新建**的终端(`start_pty` 里读 `env_vars`),
    /// 已有终端不受影响 —— 弹窗底栏那句脚注说的就是这件事。
    pub fn set_project_env_vars(
        &mut self,
        project_id: &str,
        vars: Vec<mt_config::ProjectEnvVar>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == project_id) else {
            return;
        };
        project.env_vars = vars;
        self.save_config_now();
        cx.changed(StoreEvent::ProjectsChanged);
    }

    /// 项目类型徽标覆盖:`None` = 自动探测,`Some("none")` = 不显示,
    /// 其余是技术栈 key(直接喂 `TechIcon`)。对应 `ProjectList.tsx` 的
    /// `setProjectKindOverride`(它是「改 config + 立刻落盘」两步)。
    pub fn set_project_kind_override(
        &mut self,
        id: &str,
        kind: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let next = kind.map(|k| k.to_string());
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.kind_override == next {
            return;
        }
        project.kind_override = next;
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectsChanged);
    }

    /// 移除项目:先原子撤销 Git 来源/授权,再回收 pane 的 PTY 和布局状态。
    pub fn remove_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let source_change = mt_remote::remote_git_source_change_guard();
        if let Some(source) = removal_source_pending(&self.config, id, |project| {
            self.remote_git_source_pending(project)
        }) {
            self.notify_remote_git_source_blocked(&source.id, cx);
            return;
        }
        if crate::workbench_area::project_has_dirty_documents(id, cx) {
            let project_name = self
                .project(id)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| id.to_string());
            crate::toast::push_message_deduped(
                crate::notify::ToastKind::PasteError,
                id.to_string(),
                project_name,
                crate::i18n::t("fileViewer", "projectRemovalBlocked").to_string(),
                cx,
            );
            return;
        }
        // Commit the source/grant cutover before releasing dispatch; terminal shutdown
        // remains outside the lock because a remote PTY may need transport cleanup.
        mt_remote::set_project_git_permission(id, None, "", false);
        self.config.projects.retain(|p| p.id != id);
        drop(source_change);
        let pty_ids: Vec<u32> = self
            .project_states
            .get(id)
            .map(|s| s.pty_ids())
            .unwrap_or_default();
        for pty_id in pty_ids {
            self.dispose_terminal(pty_id, cx);
        }

        self.project_states.remove(id);
        self.expanded_dirs.remove(id);
        self.done.retain_panes(&self.live_pane_ids());
        // 它的 toast 一并撤掉(`store.ts:859`)—— 留着的话点下去会跳向一个
        // 已经不存在的项目
        crate::toast::remove_project(id, cx);
        if let Some(tree) = self.config.project_tree.as_mut() {
            remove_from_tree(tree, id);
        }
        if self.active_project_id.as_deref() == Some(id) {
            self.active_project_id = self.config.projects.first().map(|p| p.id.clone());
            self.config.last_active_project_id = self.active_project_id.clone();
            cx.emit(StoreEvent::ActiveProjectChanged);
        }
        // 它在布局库里的那一行一并删掉。`flush_layout_now` 查不到项目时按删行
        // 处理,所以这里只要把 id 标脏即可(项目 id 不复用,不怕标错)。
        self.layout_dirty_projects.insert(id.to_string());
        self.schedule_layout_flush(cx);
        self.save_config_soon(cx);
        cx.emit(StoreEvent::ProjectTreeChanged);
        cx.changed(StoreEvent::ProjectsChanged);
    }

    // === 项目分组(`store.ts:1266-1313` 的五个 action) ===

    /// `ensureTree`(`store.ts:611-617`)的 Rust 版:第一次碰分组时把
    /// `projectTree` 补齐,免得后面的树操作全落进 `None` 里静默失效。
    ///
    /// **旧格式迁移不在这里**:`projectGroups`/`projectOrdering` → `projectTree`
    /// 已经由 `mt_config::migrate_config` 在读盘时做过一遍(config.rs:646-676),
    /// 这里只补「压根没有过分组」的那一档。
    ///
    /// ⚠️ 与 TS 的一处有意偏差:铺初值时**跳过 worktree 子项目**。那边是
    /// `projects.map(p => p.id)` 一个不落,但「子项目不进 projectTree」是两侧
    /// 共同的不变量(见 [`Self::add_project_at`]),把它们塞进去会让
    /// `get_ordered_tree` 同时按树序和父项目序各排一次。
    fn ensure_tree(&mut self) {
        if self
            .config
            .project_tree
            .as_ref()
            .is_some_and(|tree| !tree.is_empty())
        {
            return;
        }
        let ids: Vec<mt_config::ProjectTreeItem> = self
            .config
            .projects
            .iter()
            .filter(|p| p.parent_project_id.is_none())
            .map(|p| mt_config::ProjectTreeItem::ProjectId(p.id.clone()))
            .collect();
        self.config.project_tree = Some(ids);
    }

    /// 新建分组。`parent_group_id` 为 `None` = 建在顶层,**一律追加到末尾**。
    ///
    /// 父组找不到时 `insert_into_tree` 返回 false,原版就此**静默丢弃**
    /// (`store.ts:1266-1273` 不看返回值)—— 这里照抄:能走到这一步说明右键菜单
    /// 拿的是刚渲染过的组 id,丢弃比"悄悄建到顶层"更容易暴露真正的 bug。
    pub fn create_group(
        &mut self,
        name: &str,
        parent_group_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.ensure_tree();
        let group = mt_config::ProjectGroup {
            id: self.fresh_group_id(),
            name: name.to_string(),
            collapsed: false,
            children: Vec::new(),
        };
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        project_tree::insert_into_tree(
            tree,
            parent_group_id,
            mt_config::ProjectTreeItem::Group(group),
            None,
        );
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectTreeChanged);
    }

    /// 删分组。**组员(含子组)原位晋升到父级,一个都不删** —— 与原版
    /// `removeGroupAndPromoteChildren` 同语义,所以确认框那句"会移到上一级"是真的。
    pub fn remove_group(&mut self, group_id: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        if !project_tree::remove_group_and_promote_children(tree, group_id) {
            return;
        }
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectTreeChanged);
    }

    /// 改分组名。空名不接受(调用方那边也 `trim` 过一道,两处都拦)。
    pub fn rename_group(&mut self, group_id: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        let Some(group) = project_tree::find_group_in_tree_mut(tree, group_id) else {
            return;
        };
        if group.name == name {
            return;
        }
        group.name = name.to_string();
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectTreeChanged);
    }

    /// 折叠 / 展开。**只影响侧栏渲染**:移动端快照那条路
    /// (`mobile_relay::ordered_projects`)刻意不跳过折叠组,折不折叠由手机自己决定。
    pub fn toggle_group_collapse(&mut self, group_id: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        let Some(group) = project_tree::find_group_in_tree_mut(tree, group_id) else {
            return;
        };
        group.collapsed = !group.collapsed;
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectTreeChanged);
    }

    /// 揭示项目:把挡着它那一行的**折叠分组**逐层展开(worktree 子项目按父项目的
    /// 位置算,判据见 [`project_tree::collapsed_ancestors`])。与 [`Self::toggle_group_collapse`]
    /// 同一条路:改 `projectTree` 里的 `collapsed`、防抖落盘、发 `ProjectTreeChanged`;
    /// 一层都不用展开时什么都不发。
    ///
    /// 唯一的调用方是 [`crate::project_list::reveal`](从「项目」块之外切过来的项目)。
    pub fn expand_groups_to_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let groups = project_tree::collapsed_ancestors(&self.config, project_id);
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        let mut expanded = false;
        for group_id in &groups {
            if let Some(group) = project_tree::find_group_in_tree_mut(tree, group_id) {
                group.collapsed = false;
                expanded = true;
            }
        }
        if !expanded {
            return;
        }
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ProjectTreeChanged);
    }

    /// 把节点(项目或分组)移到 `target_group_id` 里的 `index` 位置。
    /// `target_group_id = None` = 根层;`index = None` = 追加到末尾。
    ///
    /// 全部边界语义(自环先验 / worktree 子项目脱离父项目 / 树外孤儿收编 /
    /// 目标组被并发删掉的兜底)在 [`project_tree::move_item_in_tree`],
    /// 这里只是搬运 + save/notify。
    ///
    /// 返回值 = 这次有没有真的动过树。
    pub fn move_item(
        &mut self,
        item_id: &str,
        target_group_id: Option<&str>,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.ensure_tree();
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        if !project_tree::move_item_in_tree(
            tree,
            &mut self.config.projects,
            item_id,
            target_group_id,
            index,
        ) {
            return false;
        }
        self.save_config_soon(cx);
        // 挪出父项目的 worktree 子项目会改 `parent_project_id`,项目表也算变了
        cx.emit(StoreEvent::ProjectsChanged);
        cx.changed(StoreEvent::ProjectTreeChanged);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;
    use mt_config::{ProjectGroup, ProjectTreeItem, SshConnection};

    fn recovery_config() -> AppConfig {
        let connection: SshConnection = serde_json::from_value(serde_json::json!({
            "id": "source-ssh", "name": "Source server", "host": "source.invalid",
            "port": 22, "user": "developer"
        }))
        .unwrap();
        let mut source = ProjectConfig::new("source", "Source", "/srv/repository");
        source.ssh_connection_id = Some(connection.id.clone());
        source.parent_project_id = Some("parent".into());
        source.set_remote_git_enabled(Some(&connection), true);
        let mut config = AppConfig::default();
        config.ssh_connections.push(connection);
        config.projects = vec![
            ProjectConfig::new("parent", "Parent", "/srv"),
            source,
            ProjectConfig::new("ordinary", "Ordinary", "/ordinary"),
        ];
        config.project_tree = Some(vec![
            ProjectTreeItem::Group(ProjectGroup {
                id: "group".into(),
                name: "Group".into(),
                collapsed: false,
                children: vec![ProjectTreeItem::ProjectId("parent".into())],
            }),
            ProjectTreeItem::ProjectId("ordinary".into()),
        ]);
        config
    }

    #[test]
    fn pending_source_blocks_own_and_ancestor_removal_but_not_other_projects() {
        let config = recovery_config();
        for id in ["source", "parent"] {
            let source = removal_source_pending(&config, id, |project| project.id == "source");
            assert_eq!(source.map(|project| project.id.as_str()), Some("source"));
        }
        assert!(
            removal_source_pending(&config, "ordinary", |project| project.id == "source").is_none()
        );
        assert!(removal_source_pending(&config, "source", |_| false).is_none());
        assert!(removal_source_pending(&config, "parent", |_| false).is_none());
    }

    #[test]
    fn busy_source_target_patches_are_rejected_without_mutating_configuration() {
        let edits: [fn(&mut AppConfig); 9] = [
            |config| config.projects[1].path = "/different".into(),
            |config| config.projects[1].ssh_connection_id = Some("different-ssh".into()),
            |config| config.ssh_connections[0].host = "different.invalid".into(),
            |config| config.ssh_connections[0].port = 2222,
            |config| config.ssh_connections[0].user = "different-user".into(),
            |config| config.ssh_connections.clear(),
            |config| config.projects.retain(|project| project.id != "source"),
            |config| config.projects.retain(|project| project.id != "parent"),
            |config| config.projects[1].ssh_connection_id = None,
        ];
        for edit in edits {
            let mut config = recovery_config();
            let before = serde_json::to_value(&config).unwrap();
            assert_eq!(
                patch_preserving_remote_git_sources(&mut config, &["source".into()], edit,)
                    .as_deref(),
                Some("source")
            );
            assert_eq!(serde_json::to_value(&config).unwrap(), before);
        }
    }

    #[test]
    fn busy_source_allows_credentials_metadata_grant_revocation_and_group_promotion() {
        let mut config = recovery_config();
        assert!(
            patch_preserving_remote_git_sources(&mut config, &["source".into()], |config| {
                config.ssh_connections[0].password = Some("replacement-credential".into());
                config.ssh_connections[0].identity_file = Some("/replacement-key".into());
                config.ssh_connections[0].name = "Renamed connection".into();
                config.projects[1].name = "Renamed source".into();
                config.projects[1].remote_git_permission = None;
                project_tree::remove_group_and_promote_children(
                    config.project_tree.as_mut().unwrap(),
                    "group",
                );
            })
            .is_none()
        );
        assert!(config.projects[1].remote_git_permission.is_none());
        assert_eq!(
            config.ssh_connections[0].password.as_deref(),
            Some("replacement-credential")
        );
        assert_eq!(
            config.ssh_connections[0].identity_file.as_deref(),
            Some("/replacement-key")
        );
        assert_eq!(config.projects[1].name, "Renamed source");
        assert!(project_tree::contains_id(
            config.project_tree.as_ref().unwrap(),
            "parent"
        ));
        assert!(
            project_tree::find_group_in_tree(config.project_tree.as_ref().unwrap(), "group")
                .is_none()
        );
    }

    #[test]
    fn resolved_source_accepts_previously_blocked_changes() {
        let mut config = recovery_config();
        assert!(
            patch_preserving_remote_git_sources(&mut config, &[], |config| {
                config.projects.retain(|project| project.id != "source");
                config.ssh_connections[0].host = "different.invalid".into();
            })
            .is_none()
        );
        assert!(!config.projects.iter().any(|project| project.id == "source"));
        assert_eq!(config.ssh_connections[0].host, "different.invalid");
    }

    #[gpui::test]
    fn ordinary_project_removal_and_group_promotion_keep_their_existing_behavior(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::toast::init);
        let store = cx.new(|cx| AppStore::git_test_store(recovery_config(), cx));
        cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.remove_project("ordinary", cx);
                assert!(store.project("ordinary").is_none());
                assert!(store.project("source").is_some());
                store.remove_group("group", cx);
                assert!(store.project("source").is_some());
                assert!(store.project("parent").is_some());
                assert!(project_tree::contains_id(
                    store.config.project_tree.as_ref().unwrap(),
                    "parent"
                ));
                store.remove_project("source", cx);
                assert!(store.project("source").is_none());
            })
        });
    }
}
