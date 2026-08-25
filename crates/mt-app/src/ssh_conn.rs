//! SSH 连接列表 / 关联范围的**纯逻辑**(BB-b 的三个 Modal 共用)。
//!
//! 对应 `src/components/SshModal.tsx` 里被另外两个弹窗 import 的那几个导出
//! (`connectionSummary` / `buildGroupBuckets` / `SshGroupBucket`)、
//! `SshAssocModal.tsx` 的 `initialChecked` / `sameScope`,以及
//! `src/utils/remoteProject.ts` 的远程项目判定。
//!
//! **一处实现三处用**是原版的刻意安排(注释原话:「避免两边分组顺序/空组处理
//! 走样」),移植时保持不变 —— 视图层不许各自再写一份分组。
//!
//! 全是纯函数,不碰 store、不碰网络,单测直接钉。

use mt_config::{ProjectConfig, SshConnection};

/// `user@host:port` 摘要(端口为 22 时省略)。
pub fn connection_summary(conn: &SshConnection) -> String {
    if conn.port != 0 && conn.port != 22 {
        format!("{}@{}:{}", conn.user, conn.host, conn.port)
    } else {
        format!("{}@{}", conn.user, conn.host)
    }
}

/// 归一化分组名:trim 后空串视为未分组(`None`)。
fn normalize_group(group: Option<&str>) -> Option<&str> {
    group.map(str::trim).filter(|g| !g.is_empty())
}

/// 一个分组桶。`group = None` 表示「未分组」桶。
///
/// 不派生 `PartialEq`:`SshConnection` 住在 mt-core(三个 sidecar 都链接它),
/// 为了本地一个断言去给共享类型加派生不划算 —— 测试按字段比即可。
#[derive(Debug, Clone)]
pub struct SshGroupBucket {
    pub group: Option<String>,
    pub items: Vec<SshConnection>,
}

/// 分组归类结果。
#[derive(Debug, Clone, Default)]
pub struct GroupBuckets {
    /// 具名分组,按「连接里首次出现的顺序」→ 再接显式创建的空分组。
    pub named: Vec<(String, Vec<SshConnection>)>,
    /// 未分组连接。
    pub ungrouped: Vec<SshConnection>,
}

impl GroupBuckets {
    /// 拍平成「具名桶 + (非空时)未分组桶」的展示序 —— 三个 Modal 的右栏
    /// 都是这个顺序(`AddRemoteProjectModal.tsx:99-102` 的 `groups`)。
    pub fn display_order(&self) -> Vec<SshGroupBucket> {
        let mut out: Vec<SshGroupBucket> = self
            .named
            .iter()
            .map(|(g, items)| SshGroupBucket {
                group: Some(g.clone()),
                items: items.clone(),
            })
            .collect();
        if !self.ungrouped.is_empty() {
            out.push(SshGroupBucket {
                group: None,
                items: self.ungrouped.clone(),
            });
        }
        out
    }

    /// 具名分组名(左栏用)。
    pub fn group_names(&self) -> Vec<String> {
        self.named.iter().map(|(g, _)| g.clone()).collect()
    }
}

/// 按分组归类连接。具名分组 = 连接中出现的组(按首次出现顺序)∪ 显式创建的
/// `ssh_groups`(允许空组);未分组连接单独成桶。
pub fn build_group_buckets(
    connections: &[SshConnection],
    ssh_groups: &[String],
) -> GroupBuckets {
    let mut named: Vec<(String, Vec<SshConnection>)> = Vec::new();
    let ensure = |named: &mut Vec<(String, Vec<SshConnection>)>, name: &str| -> usize {
        if let Some(i) = named.iter().position(|(g, _)| g == name) {
            i
        } else {
            named.push((name.to_string(), Vec::new()));
            named.len() - 1
        }
    };
    for conn in connections {
        if let Some(g) = normalize_group(conn.group.as_deref()) {
            let g = g.to_string();
            let idx = ensure(&mut named, &g);
            named[idx].1.push(conn.clone());
        }
    }
    for raw in ssh_groups {
        let g = raw.trim();
        if !g.is_empty() {
            ensure(&mut named, g);
        }
    }
    let ungrouped = connections
        .iter()
        .filter(|c| normalize_group(c.group.as_deref()).is_none())
        .cloned()
        .collect();
    GroupBuckets { named, ungrouped }
}

/// 分组改名后的 `sshGroups` 新值(`SshModal.tsx::renameGroup` 里的那一段)。
///
/// 逐条 trim、丢空名、**按首次出现去重** —— 重命名成一个已存在的组名时
/// 两个桶自然合并成一个,而不是留下两条同名条目(原版注释:「重命名为已有
/// 组名时自然合并,去重」)。连接的 `group` 字段由调用方另行改名。
pub fn merge_ssh_groups_on_rename(groups: &[String], old_name: &str, new_name: &str) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in groups {
        let n = if raw.trim() == old_name {
            new_name.to_string()
        } else {
            raw.trim().to_string()
        };
        if !n.is_empty() && seen.insert(n.clone()) {
            out.push(n);
        }
    }
    out
}

/// 「添加远程项目」的项目名兜底(`AddRemoteProjectModal.tsx:69-70`)。
///
/// 用户填了就用用户的(trim);没填取远程路径的**末段**;末段也取不到
/// (路径就是 `/`)就用整条路径 —— 与原版
/// `name.trim() || canonical.split('/').filter(Boolean).pop() || canonical`
/// 三级回落逐字对应。远程路径来自 SFTP canonicalize,不会是空串。
pub fn remote_project_name(name: &str, remote_path: &str) -> String {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    remote_path
        .split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or(remote_path)
        .to_string()
}

// ---------------------------------------------------------------------------
// 「关联 SSH」的范围计算(SshAssocModal)
// ---------------------------------------------------------------------------

/// 弹窗打开时的初始勾选集合(`SshAssocModal.tsx::initialChecked`)。
///
/// - 未启用 SSH 工具 → 默认全选(保存即以全部范围启用);
/// - 已启用且设过范围 → 取已存范围(**过滤掉已删除连接的陈旧 id**);
/// - 已启用但未设范围(旧配置 `undefined`)→ 全部。
pub fn initial_checked(project: &ProjectConfig, all_ids: &[String]) -> Vec<String> {
    if !project.ssh_mcp_enabled {
        return all_ids.to_vec();
    }
    match project.ssh_connection_ids.as_ref() {
        Some(ids) => ids
            .iter()
            .filter(|id| all_ids.contains(id))
            .cloned()
            .collect(),
        None => all_ids.to_vec(),
    }
}

/// 两个范围是否等价。`None` 视为 `all_ids`(兼容旧配置)。
pub fn same_scope(a: Option<&[String]>, b: &[String], all_ids: &[String]) -> bool {
    let effective_a: &[String] = a.unwrap_or(all_ids);
    if effective_a.len() != b.len() {
        return false;
    }
    effective_a.iter().all(|id| b.contains(id))
}

/// 保存时该走哪条路(`SshAssocModal.tsx::handleSave` 的前半段判定)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssocPlan {
    /// 之前没启用、现在也没勾 → 没有生成物要 reconcile,直接关窗。
    NoOp,
    /// 要启用(或已启用需幂等 reconcile)。`silent` = 有效配置没变,落盘但不弹提示。
    Enable { silent: bool, was_enabled: bool },
    /// 要停用(之前启用、现在全取消)。
    Disable,
}

/// 依据「原项目配置 + 本次勾选」算出保存计划。
///
/// 旧配置 `ssh_connection_ids == None` 的语义是「含未来新增连接」,与显式 id
/// 列表并不等价:即便当前全选,若不落盘迁移成显式列表,之后新增 SSH 连接会被
/// 静默纳入该项目的可见范围(违背 v0.6.3「新增连接不自动纳入已有项目」的承诺)。
/// 故启用状态下 `None` 必须迁移;仅当迁移前后「当前有效范围」不变时静默落盘。
pub fn plan_assoc_save(project: &ProjectConfig, checked: &[String], all_ids: &[String]) -> AssocPlan {
    let was_enabled = project.ssh_mcp_enabled;
    let now_enabled = !checked.is_empty();
    let effective_unchanged = was_enabled == now_enabled
        && (!now_enabled || same_scope(project.ssh_connection_ids.as_deref(), checked, all_ids));

    if effective_unchanged && !now_enabled {
        return AssocPlan::NoOp;
    }
    if now_enabled {
        AssocPlan::Enable {
            silent: effective_unchanged,
            was_enabled,
        }
    } else {
        AssocPlan::Disable
    }
}

// ---------------------------------------------------------------------------
// 远程项目判定(`src/utils/remoteProject.ts`)
// ---------------------------------------------------------------------------

/// 是否为 SSH 远程项目(`isRemoteProject`)。
pub fn is_remote_project(project: &ProjectConfig) -> bool {
    project.ssh_connection_id.is_some()
}

/// 取远程项目引用的 SSH 连接;**断链**(连接被删除)时返回 `None`
/// (`getRemoteConnection`)。
pub fn remote_connection<'a>(
    project: &ProjectConfig,
    connections: &'a [SshConnection],
) -> Option<&'a SshConnection> {
    let id = project.ssh_connection_id.as_deref()?;
    connections.iter().find(|c| c.id == id)
}

/// 远程 pane 的显示名:连接名(断链时回退 `ssh`)——`remotePaneLabel`。
pub fn remote_pane_label(project: &ProjectConfig, connections: &[SshConnection]) -> String {
    remote_connection(project, connections)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "ssh".to_string())
}

/// 会话面板该从哪儿取会话(`SessionList.tsx::fetchSessions` 的第一道分叉)。
///
/// 这是「会话扫描走 remote 路径」的**唯一**分流开关 —— 判据只有一处,
/// 三条并发请求(宿主 / lineage / WSL)与远程那一条不会同时发出。
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// SSH 远程项目:**只**取远程来源。
    ///
    /// 本地 `get_ai_sessions` 对远程 POSIX 路径无意义(它会去本机
    /// `~/.claude/projects` 找一个同名编码目录,命中的是**另一台机器**上同路径
    /// 的会话);WSL 来源与远程互斥;分支边(`scan_session_lineage`)读的是本地
    /// 文件,同样不扫。原版这三条在 `fetchSessions` 里是直接 `setXxx([])` 清掉的。
    Remote(SshConnection),
    /// 断链的远程项目:连接已被删,**什么都取不到**,列表应为空 + 给断链提示。
    /// 绝不能因为「拿不到连接」就退回本地扫描 —— 那会把本机的同名会话贴上去。
    BrokenRemote,
    /// 本地(含 WSL 关联)项目:宿主 + lineage + 可选 WSL,三路并发,照旧。
    Local,
}

/// 依据项目与连接表算出会话来源(见 [`SessionSource`])。
pub fn session_source(project: &ProjectConfig, connections: &[SshConnection]) -> SessionSource {
    if !is_remote_project(project) {
        return SessionSource::Local;
    }
    match remote_connection(project, connections) {
        Some(conn) => SessionSource::Remote(conn.clone()),
        None => SessionSource::BrokenRemote,
    }
}

/// 两条同 id 的连接,「连到哪台机器、以什么身份登录」是否变了。
///
/// 会话池按 `connection.id` 缓存 session,`CachedSession` 不存这些字段;用户在
/// 弹窗里把 host 改成另一台服务器却保留同一个 id 时,旧 session 会被继续复用。
/// 本函数就是那道判据 —— 返回 `true` 即必须作废池里那条 session
/// (`remote_ssh::invalidate_connection`)。
///
/// **只看会话身份字段**:host / port / user / password / identity_file。
/// `name` 与 `group` 纯展示,改它们不该白扔一条已建好的连接。
///
/// 端口 0 与 22 等价(`build_session` 把 0 归一成 22),否则「补填默认端口」这种
/// 无实质变化的编辑会白白重连一次。
pub fn ssh_session_identity_changed(old: &SshConnection, new: &SshConnection) -> bool {
    fn norm_port(p: u16) -> u16 {
        if p == 0 { 22 } else { p }
    }
    old.host != new.host
        || norm_port(old.port) != norm_port(new.port)
        || old.user != new.user
        || old.password != new.password
        || old.identity_file != new.identity_file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, group: Option<&str>) -> SshConnection {
        SshConnection {
            id: id.to_string(),
            name: format!("n-{id}"),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: group.map(str::to_string),
        }
    }

    fn project(enabled: bool, ids: Option<Vec<&str>>) -> ProjectConfig {
        let mut p = ProjectConfig {
            id: "p1".into(),
            name: "proj".into(),
            path: "/home/u/proj".into(),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: enabled,
            ssh_cli_token: None,
            ssh_connection_ids: ids.map(|v| v.into_iter().map(str::to_string).collect()),
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        };
        p.ssh_mcp_enabled = enabled;
        p
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // --- 摘要 ---

    #[test]
    fn summary_omits_default_port() {
        let mut c = conn("a", None);
        assert_eq!(connection_summary(&c), "u@h");
        c.port = 2222;
        assert_eq!(connection_summary(&c), "u@h:2222");
        // 端口 0(配置缺省)按默认端口处理,不显示 `:0`
        c.port = 0;
        assert_eq!(connection_summary(&c), "u@h");
    }

    // --- 分组桶 ---

    #[test]
    fn buckets_keep_first_seen_order_and_split_ungrouped() {
        let list = vec![
            conn("a", Some("内网")),
            conn("b", None),
            conn("c", Some("客户A")),
            conn("d", Some("内网")),
        ];
        let b = build_group_buckets(&list, &[]);
        assert_eq!(b.group_names(), vec!["内网".to_string(), "客户A".into()]);
        assert_eq!(b.named[0].1.len(), 2);
        assert_eq!(b.ungrouped.len(), 1);
        assert_eq!(b.ungrouped[0].id, "b");
    }

    #[test]
    fn buckets_include_explicit_empty_groups_after_seen_ones() {
        let list = vec![conn("a", Some("内网"))];
        let b = build_group_buckets(&list, &ids(&["客户A", "内网", "  ", ""]));
        // 已出现的组不重复;空白名忽略;新空组接在后面
        assert_eq!(b.group_names(), vec!["内网".to_string(), "客户A".into()]);
        assert!(b.named[1].1.is_empty(), "显式创建的空组要保留且为空桶");
    }

    #[test]
    fn blank_group_string_counts_as_ungrouped() {
        let list = vec![conn("a", Some("   ")), conn("b", Some(""))];
        let b = build_group_buckets(&list, &[]);
        assert!(b.named.is_empty());
        assert_eq!(b.ungrouped.len(), 2);
    }

    #[test]
    fn display_order_appends_ungrouped_bucket_last() {
        let list = vec![conn("a", Some("g")), conn("b", None)];
        let order = build_group_buckets(&list, &[]).display_order();
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].group.as_deref(), Some("g"));
        assert_eq!(order[1].group, None);
        // 全部有组时不产生空的未分组桶
        let only = build_group_buckets(&[conn("a", Some("g"))], &[]).display_order();
        assert_eq!(only.len(), 1);
    }

    // --- 分组改名 / 远程项目名 ---

    #[test]
    fn rename_merges_into_existing_group_without_duplicates() {
        let groups = ids(&["内网", "客户A", "  ", "内网"]);
        // 「客户A」改名成已存在的「内网」→ 合并,列表里只留一条
        let out = merge_ssh_groups_on_rename(&groups, "客户A", "内网");
        assert_eq!(out, ids(&["内网"]));
    }

    #[test]
    fn rename_keeps_order_and_drops_blank_names() {
        let groups = ids(&["a", "", "  ", "b"]);
        assert_eq!(merge_ssh_groups_on_rename(&groups, "a", "z"), ids(&["z", "b"]));
        // 老名字不在表里(只有连接带着它)→ 列表原样保留(去空白/去重)
        assert_eq!(merge_ssh_groups_on_rename(&groups, "x", "y"), ids(&["a", "b"]));
    }

    #[test]
    fn remote_project_name_falls_back_to_last_path_segment() {
        assert_eq!(remote_project_name("  我的项目 ", "/home/u/proj"), "我的项目");
        assert_eq!(remote_project_name("", "/home/u/proj"), "proj");
        assert_eq!(remote_project_name("  ", "/home/u/proj/"), "proj");
        // 根目录:末段取不到 → 用整条路径
        assert_eq!(remote_project_name("", "/"), "/");
    }

    // --- 初始勾选 ---

    #[test]
    fn initial_checked_defaults_to_all_when_disabled() {
        let all = ids(&["a", "b"]);
        assert_eq!(initial_checked(&project(false, None), &all), all);
        // 未启用时即便残留旧范围也全选(与原版一致)
        assert_eq!(initial_checked(&project(false, Some(vec!["a"])), &all), all);
    }

    #[test]
    fn initial_checked_drops_stale_ids() {
        let all = ids(&["a", "b"]);
        let p = project(true, Some(vec!["a", "deleted"]));
        assert_eq!(initial_checked(&p, &all), ids(&["a"]));
    }

    #[test]
    fn initial_checked_legacy_undefined_means_all() {
        let all = ids(&["a", "b"]);
        assert_eq!(initial_checked(&project(true, None), &all), all);
    }

    // --- 范围等价 ---

    #[test]
    fn same_scope_treats_none_as_all() {
        let all = ids(&["a", "b"]);
        assert!(same_scope(None, &all, &all));
        assert!(!same_scope(None, &ids(&["a"]), &all));
    }

    #[test]
    fn same_scope_is_order_insensitive() {
        let all = ids(&["a", "b"]);
        assert!(same_scope(Some(&ids(&["b", "a"])), &ids(&["a", "b"]), &all));
        assert!(!same_scope(Some(&ids(&["a"])), &ids(&["b"]), &all));
    }

    // --- 保存计划 ---

    #[test]
    fn plan_noop_when_never_enabled_and_nothing_checked() {
        let all = ids(&["a"]);
        assert_eq!(plan_assoc_save(&project(false, None), &[], &all), AssocPlan::NoOp);
    }

    #[test]
    fn plan_disable_when_was_enabled_and_now_empty() {
        let all = ids(&["a"]);
        assert_eq!(
            plan_assoc_save(&project(true, Some(vec!["a"])), &[], &all),
            AssocPlan::Disable
        );
    }

    #[test]
    fn plan_enable_first_time_is_not_silent() {
        let all = ids(&["a", "b"]);
        assert_eq!(
            plan_assoc_save(&project(false, None), &ids(&["a"]), &all),
            AssocPlan::Enable {
                silent: false,
                was_enabled: false
            }
        );
    }

    #[test]
    fn plan_enable_unchanged_scope_is_silent_reconcile() {
        let all = ids(&["a", "b"]);
        let p = project(true, Some(vec!["a", "b"]));
        assert_eq!(
            plan_assoc_save(&p, &ids(&["b", "a"]), &all),
            AssocPlan::Enable {
                silent: true,
                was_enabled: true
            }
        );
    }

    #[test]
    fn plan_enable_legacy_undefined_scope_migrates_silently_when_effectively_same() {
        // 旧配置 `None` + 当前全选 = 有效范围没变,静默落盘迁移成显式列表
        let all = ids(&["a", "b"]);
        let p = project(true, None);
        assert_eq!(
            plan_assoc_save(&p, &all, &all),
            AssocPlan::Enable {
                silent: true,
                was_enabled: true
            }
        );
        // 但缩小了范围就不是静默
        assert_eq!(
            plan_assoc_save(&p, &ids(&["a"]), &all),
            AssocPlan::Enable {
                silent: false,
                was_enabled: true
            }
        );
    }

    // --- 远程项目判定 ---

    #[test]
    fn session_source_splits_three_ways() {
        let conns = vec![conn("c1", None)];
        let local = project(false, None);
        assert!(matches!(
            session_source(&local, &conns),
            SessionSource::Local
        ));

        let mut remote = project(false, None);
        remote.ssh_connection_id = Some("c1".into());
        match session_source(&remote, &conns) {
            SessionSource::Remote(c) => assert_eq!(c.id, "c1"),
            other => panic!("应当是 Remote,得到 {other:?}"),
        }

        // 断链:绝不能退回 Local —— 那会把本机同路径的会话贴到远程项目上
        remote.ssh_connection_id = Some("gone".into());
        assert!(matches!(
            session_source(&remote, &conns),
            SessionSource::BrokenRemote
        ));
    }

    #[test]
    fn remote_project_predicates_and_label() {
        let conns = vec![conn("c1", None)];
        let mut p = project(false, None);
        assert!(!is_remote_project(&p));
        p.ssh_connection_id = Some("c1".into());
        assert!(is_remote_project(&p));
        assert_eq!(remote_connection(&p, &conns).map(|c| c.id.as_str()), Some("c1"));
        assert_eq!(remote_pane_label(&p, &conns), "n-c1");
        // 断链:连接被删 → 标签回退 'ssh',但项目仍是远程项目
        p.ssh_connection_id = Some("gone".into());
        assert!(is_remote_project(&p));
        assert!(remote_connection(&p, &conns).is_none());
        assert_eq!(remote_pane_label(&p, &conns), "ssh");
    }

    // --- 池失效判据 --------------------------------------------------------

    #[test]
    fn identity_change_ignores_cosmetic_fields() {
        let old = conn("c1", None);
        let mut next = conn("c1", Some("生产"));
        next.name = "改了个名".into();
        assert!(
            !ssh_session_identity_changed(&old, &next),
            "改名 / 改分组不该白扔一条已建好的 session"
        );
        // 完全没改也不该失效。
        assert!(!ssh_session_identity_changed(&old, &conn("c1", None)));
    }

    #[test]
    fn identity_change_detects_each_session_field() {
        let base = conn("c1", None);

        let mut host = base.clone();
        host.host = "other.example.com".into();
        assert!(ssh_session_identity_changed(&base, &host), "换 host");

        let mut port = base.clone();
        port.port = 2222;
        assert!(ssh_session_identity_changed(&base, &port), "换端口");

        let mut user = base.clone();
        user.user = "deploy".into();
        assert!(ssh_session_identity_changed(&base, &user), "换登录用户");

        let mut pw = base.clone();
        pw.password = Some("s3cret".into());
        assert!(ssh_session_identity_changed(&base, &pw), "改密码");

        let mut key = base.clone();
        key.identity_file = Some("/home/u/.ssh/id_ed25519".into());
        assert!(ssh_session_identity_changed(&base, &key), "改密钥路径");
    }

    #[test]
    fn identity_change_treats_port_zero_as_22() {
        // `build_session` 把 0 归一成 22 —— 「补填默认端口」是无实质变化的编辑,
        // 不该触发一次白重连。
        let mut zero = conn("c1", None);
        zero.port = 0;
        let mut twenty_two = conn("c1", None);
        twenty_two.port = 22;
        assert!(!ssh_session_identity_changed(&zero, &twenty_two));
        assert!(!ssh_session_identity_changed(&twenty_two, &zero));
        // 但 0 → 2222 仍算换了。
        let mut other = conn("c1", None);
        other.port = 2222;
        assert!(ssh_session_identity_changed(&zero, &other));
    }
}
