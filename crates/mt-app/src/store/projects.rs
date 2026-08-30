//! 项目相关的 `AppStore` 方法:目录技术栈探测、项目 CRUD、项目分组。
//!
//! 从 `store.rs` 原样搬来的三段(`// === 目录技术栈探测 ===` /
//! `// === 项目 ===` / `// === 项目分组 ===`),段注释随代码走,逻辑一行未改。

use std::collections::HashSet;
use std::path::Path;

use gpui::Context;
use mt_config::ProjectConfig;
use mt_ui::icons::ProjectKind;

use crate::project_tree;
use crate::tree::gen_id;

use super::pure::remove_from_tree;
use super::{AppStore, ProjectState};

impl AppStore {
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
                        crate::project_kind::detect_local(std::path::Path::new(&probe))
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
        cx.notify();
    }

    /// 失效(`removeDirKind`):项目根的标记文件变动时调。下一轮 `ensure` 会重探。
    pub fn remove_dir_kind(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.dir_kinds.remove(path).is_some() {
            cx.notify();
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
        cx.notify();
    }

    /// 按路径找项目(`store.ts::findProjectByPath`)。
    ///
    /// 比对走 [`normalize_path`](crate::git_worktree::normalize_path)(分隔符统一 +
    /// 去尾斜杠 + 转小写),与 worktree「是否已是项目」的判据同一份。
    /// SSH 远程项目排除在外 —— worktree 的路径是本机路径。
    pub fn find_project_by_path(&self, path: &str) -> Option<&ProjectConfig> {
        let target = crate::git_worktree::normalize_path(path);
        self.config.projects.iter().find(|p| {
            p.ssh_connection_id.is_none() && crate::git_worktree::normalize_path(&p.path) == target
        })
    }

    /// 添加项目并**返回它的 id**;`parent` 非空时挂成子项目。
    ///
    /// 对应 `store.ts:777-799` 的 `addProject(project, parentProjectId)`:
    /// - 父项目必须真实存在,否则回落为普通顶层项目(防止产生渲染不出来的孤儿);
    /// - **子项目不进 `projectTree`**(移动出去时才转成普通树节点);
    /// - 路径已经是项目 → 返回既有 id,不重复添加(`GitWorktreeModal.tsx:341-351`)。
    ///
    /// 与 [`add_project`](Self::add_project) 的差别只有「带父项目 + 返回 id +
    /// 不自动切过去」三条 —— worktree「设为项目」要自己决定切不切。
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
        let id = gen_id("proj");
        let parent_ok = parent.filter(|pid| self.config.projects.iter().any(|p| p.id == *pid));

        self.config.projects.push(ProjectConfig {
            id: id.clone(),
            name,
            path: path_str,
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: parent_ok.map(str::to_string),
            kind_override: None,
        });
        if parent_ok.is_none() {
            let tree = self.config.project_tree.get_or_insert_with(Vec::new);
            tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        }
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.save_config_soon(cx);
        cx.notify();
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
        cx.notify();
    }

    /// 添加项目(目录路径)。名字取目录名。
    pub fn add_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path_str = path.to_string_lossy().to_string();
        if let Some(existing) = self
            .config
            .projects
            .iter()
            .find(|p| p.path == path_str)
            .map(|p| p.id.clone())
        {
            self.set_active_project(&existing, cx);
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path_str.clone());
        let id = gen_id("proj");

        self.config.projects.push(ProjectConfig {
            id: id.clone(),
            name,
            path: path_str,
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        });
        // projectTree 是「分组 + 排序」那一层;这里只保证新项目出现在树里,
        // 分组编辑是后续批次的事。
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));

        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.active_project_id = Some(id.clone());
        self.config.last_active_project_id = Some(id);
        self.save_config_soon(cx);
        cx.notify();
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
    }

    /// 移除项目:先回收它所有 pane 的 PTY,再从配置里摘掉。
    pub fn remove_project(&mut self, id: &str, cx: &mut Context<Self>) {
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
        self.config.projects.retain(|p| p.id != id);
        if let Some(tree) = self.config.project_tree.as_mut() {
            remove_from_tree(tree, id);
        }
        if self.active_project_id.as_deref() == Some(id) {
            self.active_project_id = self.config.projects.first().map(|p| p.id.clone());
            self.config.last_active_project_id = self.active_project_id.clone();
        }
        // 它在布局库里的那一行一并删掉。`flush_layout_now` 查不到项目时按删行
        // 处理,所以这里只要把 id 标脏即可(项目 id 不复用,不怕标错)。
        self.layout_dirty_projects.insert(id.to_string());
        self.schedule_layout_flush(cx);
        self.save_config_soon(cx);
        cx.notify();
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
            id: gen_id("group"),
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
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
        cx.notify();
        true
    }
}
