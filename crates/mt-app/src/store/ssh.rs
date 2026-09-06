//! SSH 相关的 `AppStore` 方法。原 `store.rs` 里那个独立的 `impl AppStore` 块
//! 整块搬来(含块前的段注释),逻辑一行未改。

use std::collections::HashSet;

use gpui::{Context, Task};
use mt_config::{ProjectConfig, SshConnection};

use crate::tree::{gen_id, PaneState, PaneStatus};

use super::{AppStore, ProjectState, SshAssocOutcome};

// ===========================================================================
// SSH(audit #28,BB-a 批)
// ===========================================================================
//
// 三块:① 连接表 / 分组 CRUD(`SshModal.tsx` 的 `persist` 那一串);
// ② 远程项目(`AddRemoteProjectModal.tsx` + `remoteProject.ts`);
// ③ 「关联 SSH」的启用/停用(`SshAssocModal.tsx::handleSave`);
// 外加断线重连(`PaneGroup.tsx::handleReconnect` + `resetPaneForReconnect`)。
//
// BB-b 已把消费方全部接上(三个弹窗 + 远程项目 UI + 断线遮罩 + 文件树/会话
// 面板的远程分流),BB-a 留的 `allow(dead_code)` 随之删除 —— 从此这里多一个
// 没人调的函数就会在 `cargo check` 里红。
impl AppStore {
    /// 已保存的 SSH 连接(`config.sshConnections`)。
    pub fn ssh_connections(&self) -> &[SshConnection] {
        &self.config.ssh_connections
    }

    /// 显式创建的 SSH 分组名(允许空组;连接的 `group` 字段仍是归属单一来源)。
    pub fn ssh_groups(&self) -> &[String] {
        &self.config.ssh_groups
    }

    /// 新增或更新一条连接(按 id 判定,`SshModal.tsx::handleSave`)。
    ///
    /// **立即落盘**而不是 500ms 防抖:原版这条路是 `await saveConfigToDisk`,
    /// 密码/私钥路径这类东西不该在防抖窗口里被一次崩溃吃掉。
    ///
    /// 改动落库后,若「连到哪台机器、以什么身份登录」变了就**作废池里那条
    /// session**:池键是纯 `connection.id`,不作废的话旧服务器的连接会一直被
    /// 复用到 reaper 回收(idle 10min / lifetime 2h)。判据见
    /// [`crate::ssh_conn::ssh_session_identity_changed`]。
    /// (只作废本进程的池;三个 sidecar 各自独立进程独立池,不在这条链路上。)
    ///
    /// **密码在这里封存**:表单交来的是明文,落进 `config` 之前换成 `mt-secret` 信封
    /// (没改的沿用旧信封,判据见 [`crate::secrets::stored_password`])。封存失败就
    /// **不存密码**(其余字段照存)并把原因交给调用方提示 —— 宁可让用户再填一次,
    /// 也不把明文写进库。
    pub fn upsert_ssh_connection(
        &mut self,
        conn: SshConnection,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut conn = conn;
        let id = conn.id.clone();
        let mut seal_error = None;
        if let Some(plain) = conn.password.take().filter(|p| !p.is_empty()) {
            let existing = self
                .config
                .ssh_connections
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.password.as_deref());
            match crate::secrets::stored_password(&plain, existing) {
                Ok(stored) => conn.password = Some(stored),
                Err(err) => seal_error = Some(err),
            }
        }
        let mut identity_changed = false;
        match self
            .config
            .ssh_connections
            .iter_mut()
            .find(|c| c.id == conn.id)
        {
            Some(slot) => {
                identity_changed = crate::ssh_conn::ssh_session_identity_changed(slot, &conn);
                *slot = conn;
            }
            None => self.config.ssh_connections.push(conn),
        }
        if identity_changed {
            crate::remote_ssh::invalidate_connection(&id);
        }
        self.save_config_now();
        cx.notify();
        match seal_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// 删除一条连接(二次确认由调用方做 —— 原版同款)。
    ///
    /// **不级联清理**引用它的项目 / `sshConnectionIds`:原版就是这个语义,
    /// 远程项目因此进入「断链」错误态(仍可见、可删),关联范围静默收窄。
    ///
    /// 但**池里那条 session 必须无条件作废** —— 连接都删了还留着一条活的 TCP
    /// 长连(以及 home / gitignore 缓存)没有任何道理,且新建一条同 id 的连接
    /// (id 由调用方生成,理论上可复用)会直接命中旧机器的 session。
    pub fn remove_ssh_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.config.ssh_connections.len();
        self.config.ssh_connections.retain(|c| c.id != id);
        if self.config.ssh_connections.len() == before {
            return;
        }
        crate::remote_ssh::invalidate_connection(id);
        self.save_config_now();
        cx.notify();
    }

    /// 新建一个空分组(重名则只切选中态,由调用方处理)。返回是否真的新建了。
    pub fn create_ssh_group(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let exists = self
            .config
            .ssh_groups
            .iter()
            .any(|n| n.trim() == name)
            || self
                .config
                .ssh_connections
                .iter()
                .any(|c| c.group.as_deref().map(str::trim) == Some(name));
        if exists {
            return false;
        }
        self.config.ssh_groups.push(name.to_string());
        self.save_config_now();
        cx.notify();
        true
    }

    /// 分组改名:连接归属改名 + `sshGroups` 同步替换。
    /// 重命名为已有组名时**自然合并、去重**(原版 `renameGroup` 的注释原话)。
    pub fn rename_ssh_group(&mut self, old_name: &str, new_name: &str, cx: &mut Context<Self>) {
        let next = new_name.trim();
        if next.is_empty() || next == old_name {
            return;
        }
        self.config.ssh_groups =
            crate::ssh_conn::merge_ssh_groups_on_rename(&self.config.ssh_groups, old_name, next);
        for c in &mut self.config.ssh_connections {
            if c.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) == Some(old_name) {
                c.group = Some(next.to_string());
            }
        }
        self.save_config_now();
        cx.notify();
    }

    /// 解散分组:组里的连接回落「未分组」,组名从 `sshGroups` 移除(连接不删)。
    pub fn dissolve_ssh_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.config.ssh_groups.retain(|n| n.trim() != name);
        for c in &mut self.config.ssh_connections {
            if c.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) == Some(name) {
                c.group = None;
            }
        }
        self.save_config_now();
        cx.notify();
    }

    /// 把一条连接挪进某个分组(`group = None` = 挪到未分组)。
    pub fn move_ssh_connection_to_group(
        &mut self,
        conn_id: &str,
        group: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let target = group.map(str::trim).filter(|g| !g.is_empty());
        let Some(conn) = self
            .config
            .ssh_connections
            .iter_mut()
            .find(|c| c.id == conn_id)
        else {
            return;
        };
        let current = conn.group.as_deref().map(str::trim).filter(|g| !g.is_empty());
        if current == target {
            return;
        }
        conn.group = target.map(str::to_string);
        self.save_config_now();
        cx.notify();
    }

    // --- 远程项目 ---

    /// 这个项目是 SSH 远程项目吗(`remoteProject.ts::isRemoteProject`)。
    pub fn is_remote_project(&self, project_id: &str) -> bool {
        self.project(project_id)
            .is_some_and(crate::ssh_conn::is_remote_project)
    }

    /// 远程项目引用的连接;**断链**(连接被删)时 `None`。
    ///
    /// 返回克隆而不是引用:调用方多半要把它丢进 `background_executor`
    /// (`remote_ssh` 的入口全是阻塞函数,见那个模块的线程口径)。
    pub fn remote_connection_of(&self, project_id: &str) -> Option<SshConnection> {
        let project = self.project(project_id)?;
        crate::ssh_conn::remote_connection(project, &self.config.ssh_connections).cloned()
    }

    /// pane 显示名的统一口径:自定义名 > 远程连接名 > shell 名
    /// (`remoteProject.ts::paneDisplayLabel`)。tab 栏与项目预览浮层共用,
    /// 防两处口径漂移。
    pub fn pane_display_label(&self, project_id: &str, pane: &PaneState) -> String {
        if let Some(title) = pane.custom_title.as_deref().filter(|t| !t.is_empty()) {
            return title.to_string();
        }
        if let Some(project) = self.project(project_id)
            && crate::ssh_conn::is_remote_project(project)
        {
            return crate::ssh_conn::remote_pane_label(project, &self.config.ssh_connections);
        }
        pane.shell_name.clone()
    }

    /// 添加一个 SSH 远程项目并返回它的 id(`AddRemoteProjectModal.tsx::handleSave`
    /// 的落盘那一半 —— 远程路径的 `~` 展开与目录校验由调用方先跑
    /// [`crate::remote_ssh::validate_dir`],这里只接**已 canonicalize 的绝对路径**)。
    ///
    /// - `name` 为空时取路径末段(再取不到就用整条路径),与原版一字不差;
    /// - 远程项目**不参与** [`Self::find_project_by_path`] 的去重(那条判据显式
    ///   排除了 `ssh_connection_id.is_some()` 的项目):两台机器上的
    ///   `/home/u/proj` 是两个项目;
    /// - `target_group` 非空时落进该分组(分组折叠由调用方展开)。
    pub fn add_remote_project(
        &mut self,
        name: &str,
        connection_id: &str,
        remote_path: &str,
        target_group: Option<&str>,
        cx: &mut Context<Self>,
    ) -> String {
        let final_name = crate::ssh_conn::remote_project_name(name, remote_path);
        let id = gen_id("proj");
        self.config.projects.push(ProjectConfig {
            id: id.clone(),
            name: final_name,
            path: remote_path.to_string(),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: Some(connection_id.to_string()),
            parent_project_id: None,
            kind_override: None,
        });
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        if let Some(group_id) = target_group {
            self.move_item(&id, Some(group_id), None, cx);
        }
        self.save_config_now();
        cx.notify();
        id
    }

    // --- 「关联 SSH」(SSH 工具 = CLI + Skill)---

    /// 把「关联 SSH」的结果写回项目配置(`SshAssocModal.tsx` 落盘那一段)。
    ///
    /// 范围**始终存显式 id 列表**,不用 `None` 表示「全选」—— 见
    /// [`crate::ssh_conn::plan_assoc_save`] 里那条 v0.6.3 承诺。
    pub fn set_project_ssh_assoc(
        &mut self,
        project_id: &str,
        enabled: bool,
        project_token: Option<String>,
        scope: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == project_id) else {
            return;
        };
        project.ssh_mcp_enabled = enabled;
        project.ssh_cli_token = if enabled { project_token } else { None };
        project.ssh_connection_ids = if enabled { Some(scope) } else { None };
        self.save_config_now();
        cx.notify();
    }

    /// 「关联 SSH」保存的**完整**动作:算计划 → 后台跑注册器 → 回主线程落配置。
    ///
    /// 返回 `Task`,BB-b 的弹窗 `await` 它拿结果:`Ok(None)` = 什么都没做
    /// (从未启用且这次也没勾),`Ok(Some(outcome))` = 已落盘,按
    /// [`SshAssocOutcome::silent`] 决定弹不弹提示。
    ///
    /// **注册器是阻塞文件 IO**(还要写 home 下的 Codex / Claude 配置),
    /// 全程在 `background_executor` 上,主线程只负责最后那一次 `set_...`。
    pub fn apply_ssh_assoc(
        &mut self,
        project_id: &str,
        checked: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<SshAssocOutcome>, String>> {
        let Some(project) = self.project(project_id).cloned() else {
            return Task::ready(Ok(None));
        };
        let all_ids: Vec<String> = self
            .config
            .ssh_connections
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let plan = crate::ssh_conn::plan_assoc_save(&project, &checked, &all_ids);
        let project_id = project_id.to_string();
        let project_dir = project.path.clone();
        let existing_token = project.ssh_cli_token.clone();

        cx.spawn(async move |this, cx| {
            let outcome = match plan {
                crate::ssh_conn::AssocPlan::NoOp => return Ok(None),
                crate::ssh_conn::AssocPlan::Enable {
                    silent,
                    was_enabled,
                } => {
                    let dir = project_dir.clone();
                    let token = existing_token.clone();
                    let res = cx
                        .background_executor()
                        .spawn(async move {
                            crate::ssh_registry::enable(&dir, token.as_deref())
                        })
                        .await?;
                    SshAssocOutcome {
                        enabled: true,
                        was_enabled,
                        silent,
                        scope_len: checked.len(),
                        total_len: all_ids.len(),
                        project_token: Some(res.project_token),
                        message: res.message,
                    }
                }
                crate::ssh_conn::AssocPlan::Disable => {
                    let dir = project_dir.clone();
                    let message = cx
                        .background_executor()
                        .spawn(async move { crate::ssh_registry::disable(&dir) })
                        .await?;
                    SshAssocOutcome {
                        enabled: false,
                        was_enabled: true,
                        silent: false,
                        scope_len: 0,
                        total_len: all_ids.len(),
                        project_token: None,
                        message,
                    }
                }
            };
            let scope = if outcome.enabled { checked } else { Vec::new() };
            let token = outcome.project_token.clone();
            let enabled = outcome.enabled;
            this.update(cx, |store: &mut AppStore, cx| {
                store.set_project_ssh_assoc(&project_id, enabled, token, scope, cx);
            })
            .map_err(|e| e.to_string())?;
            Ok(Some(outcome))
        })
    }

    // =======================================================================
    // 断线重连(exitedPtyIds 体系的写侧)
    // =======================================================================

    // 原版 `store.ts::clearPtyExited` 在这里**刻意没有对应物**:
    // GPUI 侧唯一的调用时机是重连,而 `reset_pane_for_reconnect` 走的
    // `dispose_terminal` 已经把退出登记连同标记/游标一起摘了(见那边的注释)。
    // 再留一个公开的单点摘除函数只会多一条会漂移的路。

    /// 远程 pane 重连:回收旧 PTY(连同标记/退出登记),就地起一条新的。
    ///
    /// 对应 `PaneGroup.tsx::handleReconnect` + `store.ts::resetPaneForReconnect`
    /// 那一对。原版是「置 `ptyId=undefined` + `status=idle`,让懒创建 effect
    /// 重新 `create_pty`」两步;GPUI 侧 PTY 是即时创建的,于是并成一步 ——
    /// **可观察行为完全一致**(旧终端连同回滚缓冲一并销毁,新会话从空屏开始)。
    ///
    /// 选清屏而非保留历史,理由照抄原版:新 PTY 的输出从头开始,旧 buffer 的
    /// 光标/滚动状态与新会话无法衔接,保留反而会出现「半屏旧内容 + 新登录横幅」
    /// 的错位;且 dispose 一并回收标记,链路与关 tab 完全一致,无新状态机。
    ///
    /// 返回新 PTY 编号;项目/pane 不在了返回 `None`。
    /// **本地 pane 同样适用** —— 原版覆盖层只画在远程 pane 上,但动作本身与
    /// 「远程」无关,判定留给调用方(BB-b 的覆盖层)。
    pub fn reset_pane_for_reconnect(
        &mut self,
        project_id: &str,
        pane_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<u32> {
        let project = self.project(project_id)?.clone();
        let old_pty = self
            .project_states
            .get(project_id)
            .and_then(|s| s.pane(pane_id))
            .and_then(|p| p.pty_id);
        // dispose 里已经做了:kill 子进程 + 清标记与游标 + 摘退出登记
        // (`clearMarkersForPty` / `clearPtyExited` 在原版是分开两调,这里同源)
        if let Some(old) = old_pty {
            self.dispose_terminal(old, cx);
        }

        let (shell_name, cwd) = {
            let pane = self
                .project_states
                .get(project_id)
                .and_then(|s| s.pane(pane_id))?;
            (pane.shell_name.clone(), pane.cwd.clone())
        };
        let shell = self.resolve_shell(Some(&shell_name))?;
        let new_pty = self.start_pty(&project, &shell, cwd.as_deref(), cx);

        let state = self.project_states.get_mut(project_id)?;
        let pane = state.pane_mut(pane_id)?;
        pane.pty_id = Some(new_pty);
        pane.status = PaneStatus::Idle;
        state.status = state.highest_status();
        self.after_layout_change(project_id, cx);
        Some(new_pty)
    }
}
