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

/// 把连接 `dragged_id` 挪到连接 `target_id` 的前面 / 后面(`after`),返回**整张**
/// 连接表的新顺序;什么都不用改(拖到自己身上、id 不存在、落点就是原位)时 `None`。
///
/// # 为什么不是「在 `connections` 里先删后插」
///
/// 分组的**展示顺序**是「连接里首次出现的顺序」([`build_group_buckets`]),直接在
/// 平铺表里搬一条会连带改掉组序:`[a1(A), b1(B), a2(A)]` 里把 a1 挪到 a2 之后,
/// 平铺表变成 `[b1, a2, a1]`,左栏的 A/B 就对调了 —— 用户只是在 A 组里换了个位置。
/// 所以这里按桶操作:先归桶、在目标桶里插位、再按桶序拍平回去。顺带把平铺表
/// 归一成「桶序」(与右栏画出来的一致),往后每次拖拽都是同一把尺。
///
/// 落到**另一个桶**的某条连接前后 = 顺带改归属(与拖到左栏分组上同义,只是多了
/// 个位置);归属写的是桶名(已 trim),与 `move_ssh_connection_to_group` 同款。
pub fn reorder_connection(
    connections: &[SshConnection],
    ssh_groups: &[String],
    dragged_id: &str,
    target_id: &str,
    after: bool,
) -> Option<Vec<SshConnection>> {
    if dragged_id == target_id {
        return None;
    }
    let mut dragged = connections.iter().find(|c| c.id == dragged_id)?.clone();
    let target_group = normalize_group(
        connections
            .iter()
            .find(|c| c.id == target_id)?
            .group
            .as_deref(),
    )
    .map(str::to_string);

    let mut buckets = build_group_buckets(connections, ssh_groups);
    for (_, items) in &mut buckets.named {
        items.retain(|c| c.id != dragged_id);
    }
    buckets.ungrouped.retain(|c| c.id != dragged_id);

    dragged.group = target_group.clone();
    let slot = match &target_group {
        Some(g) => &mut buckets.named.iter_mut().find(|(name, _)| name == g)?.1,
        None => &mut buckets.ungrouped,
    };
    let target_idx = slot.iter().position(|c| c.id == target_id)?;
    slot.insert(if after { target_idx + 1 } else { target_idx }, dragged);

    let next: Vec<SshConnection> = buckets
        .named
        .into_iter()
        .flat_map(|(_, items)| items)
        .chain(buckets.ungrouped)
        .collect();
    let unchanged = next.len() == connections.len()
        && next
            .iter()
            .zip(connections)
            .all(|(n, o)| n.id == o.id && n.group == o.group);
    if unchanged { None } else { Some(next) }
}

/// 「复制」出来的那条连接叫什么:`原名 (N)`,N 从 1 起取第一个没被占用的号。
///
/// 原名已经带 ` (N)` 尾巴(复制的复制)时先剥掉再编号 —— 复制「prod (1)」得
/// 「prod (2)」,而不是越叠越长的「prod (1) (1)」。占用判据是**整张连接表的名字**
/// (trim 后比):名字是用户与 agent 按名引用连接的字面量(`mini-term-ssh` 那类),
/// 两条同名会让按名查找含糊。
pub fn duplicate_name(name: &str, connections: &[SshConnection]) -> String {
    let base = strip_copy_suffix(name.trim());
    let mut n = 1usize;
    loop {
        let candidate = if base.is_empty() {
            format!("({n})")
        } else {
            format!("{base} ({n})")
        };
        if !connections.iter().any(|c| c.name.trim() == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// 剥掉名字末尾一段 ` (数字)`;没有这种尾巴(或剥完只剩空串)就原样返回。
fn strip_copy_suffix(name: &str) -> &str {
    let Some(inner) = name.strip_suffix(')') else {
        return name;
    };
    let Some(open) = inner.rfind(" (") else {
        return name;
    };
    let digits = &inner[open + 2..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return name;
    }
    let base = inner[..open].trim_end();
    if base.is_empty() { name } else { base }
}

/// 复制一条连接:换上 `new_id` 与 [`duplicate_name`] 起的名字,其余字段 —— 密码
/// 信封、私钥路径、分组、新版本写的未知字段(`extra`)—— 原样照抄,插在原连接
/// **紧后面**(同组相邻,左栏组序不动)。返回新的整张连接表;`source_id` 不存在时 `None`。
///
/// 密码信封直接照抄而不是解开再封:同一把钥匙解得开;而「表单 → 封存」那条路
/// (`AppStore::upsert_ssh_connection`)认的是明文,把信封喂进去会被当明文再封一层。
pub fn duplicate_connection(
    connections: &[SshConnection],
    source_id: &str,
    new_id: String,
) -> Option<Vec<SshConnection>> {
    let idx = connections.iter().position(|c| c.id == source_id)?;
    let source = &connections[idx];
    let copy = SshConnection {
        id: new_id,
        name: duplicate_name(&source.name, connections),
        ..source.clone()
    };
    let mut next = connections.to_vec();
    next.insert(idx + 1, copy);
    Some(next)
}

/// 分组改名 / 合并 / 解散之后,整组关联过旧组的项目跟着改。返回是否动了任何项目。
///
/// - `rename_to = Some(新名)`:改成一个此前**不存在**的组名 → 项目里的组名跟着改,
///   关联不断;
/// - `rename_to = None`:旧组从此不复存在(解散,或改名并进了另一个已有组)→ 把旧组
///   **此刻**的成员摊成显式 id 补进 `ssh_connection_ids`,再摘掉旧组名。
///
/// 后一条不能偷懒:合并时直接把关联挪到目标组,项目会凭空多看到目标组原有的连接;
/// 解散时只摘组名,项目会悄悄丢掉那几台。两样都是「整理分组」顺手改了 agent 的
/// 权限 —— 摊成显式 id 让有效范围原样不变,要收窄 / 放宽由用户回「关联 SSH」里改。
///
/// `connections` 必须是**改组之前**的连接表(要据它数旧组的成员)。
pub fn regroup_project_scopes(
    projects: &mut [ProjectConfig],
    connections: &[SshConnection],
    old: &str,
    rename_to: Option<&str>,
) -> bool {
    let old = old.trim();
    let members = effective_scope(&[], &[old.to_string()], connections);
    let mut changed = false;
    for project in projects {
        if !project
            .ssh_connection_groups
            .iter()
            .any(|g| g.trim() == old)
        {
            continue;
        }
        changed = true;
        match rename_to.map(str::trim) {
            Some(new) => {
                let mut next: Vec<String> = Vec::new();
                for g in &project.ssh_connection_groups {
                    let g = if g.trim() == old { new } else { g.trim() };
                    if !next.iter().any(|n| n == g) {
                        next.push(g.to_string());
                    }
                }
                project.ssh_connection_groups = next;
            }
            None => {
                project.ssh_connection_groups.retain(|g| g.trim() != old);
                if let Some(ids) = project.ssh_connection_ids.as_mut() {
                    for id in &members {
                        if !ids.contains(id) {
                            ids.push(id.clone());
                        }
                    }
                }
            }
        }
    }
    changed
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

/// 弹窗打开时**整组**勾选的分组(按 `group_names` 的展示序)。
///
/// 只认此刻还在的组名(组已不存在就不勾,保存时自然摘掉);未启用、或旧配置
/// 「未设范围」(`None` = 单条已全勾)时一个组都不勾。
pub fn initial_checked_groups(project: &ProjectConfig, group_names: &[String]) -> Vec<String> {
    if !project.ssh_mcp_enabled || project.ssh_connection_ids.is_none() {
        return Vec::new();
    }
    group_names
        .iter()
        .filter(|g| {
            project
                .ssh_connection_groups
                .iter()
                .any(|linked| linked.trim() == g.as_str())
        })
        .cloned()
        .collect()
}

/// 一次勾选的有效范围:单勾的 id ∪ 勾选分组**此刻**的成员。
///
/// 与写 sidecar 投影走同一条规则([`ProjectConfig::effective_ssh_connection_ids`]),
/// 弹窗里数出来的「已选 N」与 agent 实际看得到的不会两样。`checked` 为空、`groups`
/// 非空时就是「这几个组罩住了哪些连接」—— 弹窗据此把组内的行画成已勾、不可单取消。
pub fn effective_scope(
    checked: &[String],
    groups: &[String],
    connections: &[SshConnection],
) -> Vec<String> {
    ProjectConfig {
        ssh_connection_ids: Some(checked.to_vec()),
        ssh_connection_groups: groups.to_vec(),
        ..ProjectConfig::default()
    }
    .effective_ssh_connection_ids(connections)
    .unwrap_or_default()
}

/// 两个范围是否等价。`None` 视为 `all_ids`(兼容旧配置)。
pub fn same_scope(a: Option<&[String]>, b: &[String], all_ids: &[String]) -> bool {
    let effective_a: &[String] = a.unwrap_or(all_ids);
    if effective_a.len() != b.len() {
        return false;
    }
    effective_a.iter().all(|id| b.contains(id))
}

/// 两份整组关联是否等价(按 trim 后的组名集合比,不看顺序)。
fn same_groups(a: &[String], b: &[String]) -> bool {
    let norm = |v: &[String]| -> std::collections::BTreeSet<String> {
        v.iter()
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect()
    };
    norm(a) == norm(b)
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

/// 依据「原项目配置 + 本次勾选(单条 `checked` + 整组 `groups`)」算出保存计划。
///
/// 旧配置 `ssh_connection_ids == None` 的语义是「含未来新增连接」,与显式 id
/// 列表并不等价:即便当前全选,若不落盘迁移成显式列表,之后新增 SSH 连接会被
/// 静默纳入该项目的可见范围(违背 v0.6.3「新增连接不自动纳入已有项目」的承诺)。
/// 故启用状态下 `None` 必须迁移;仅当迁移前后「当前有效范围」不变时静默落盘。
///
/// 整组关联是用户**明说**的「组内以后新加的也算」,与上面那条承诺不冲突。只勾了
/// 一个空组也算启用(用户要的就是往这组里加的连接都能用)。「有效配置没变」要求
/// 此刻的有效范围与整组关联**两样都没变** —— 组集合变了,将来的范围就变了。
pub fn plan_assoc_save(
    project: &ProjectConfig,
    checked: &[String],
    groups: &[String],
    connections: &[SshConnection],
) -> AssocPlan {
    let all_ids: Vec<String> = connections.iter().map(|c| c.id.clone()).collect();
    let was_enabled = project.ssh_mcp_enabled;
    let now_enabled = !checked.is_empty() || !groups.is_empty();
    let effective_unchanged = was_enabled == now_enabled
        && (!now_enabled
            || (same_scope(
                project.effective_ssh_connection_ids(connections).as_deref(),
                &effective_scope(checked, groups, connections),
                &all_ids,
            ) && same_groups(&project.ssh_connection_groups, groups)));

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
/// (`mt_remote::invalidate_connection`)。
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
            extra: Default::default(),
        }
    }

    fn project(enabled: bool, ids: Option<Vec<&str>>) -> ProjectConfig {
        ProjectConfig {
            ssh_mcp_enabled: enabled,
            ssh_connection_ids: ids.map(|v| v.into_iter().map(str::to_string).collect()),
            ..ProjectConfig::new("p1", "proj", "/home/u/proj")
        }
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

    // --- 拖拽排序 ---

    fn order_of(list: &[SshConnection]) -> Vec<(String, Option<String>)> {
        list.iter()
            .map(|c| (c.id.clone(), c.group.clone()))
            .collect()
    }

    fn pair(id: &str, group: Option<&str>) -> (String, Option<String>) {
        (id.to_string(), group.map(str::to_string))
    }

    #[test]
    fn reorder_within_group_keeps_group_order() {
        // A 组里把 a1 挪到 a2 之后:平铺表里 b1 夹在中间,直接先删后插会让 B 组
        // 跑到 A 组前面 —— 这里按桶序拍平,A 仍然在前
        let list = vec![
            conn("a1", Some("A")),
            conn("b1", Some("B")),
            conn("a2", Some("A")),
        ];
        let next = reorder_connection(&list, &[], "a1", "a2", true).expect("有变化");
        assert_eq!(
            order_of(&next),
            vec![
                pair("a2", Some("A")),
                pair("a1", Some("A")),
                pair("b1", Some("B"))
            ]
        );
        assert_eq!(
            build_group_buckets(&next, &[]).group_names(),
            vec!["A".to_string(), "B".into()],
            "组序不能因为组内换位而变"
        );
    }

    #[test]
    fn reorder_before_and_after_target() {
        let list = vec![conn("a", None), conn("b", None), conn("c", None)];
        let before = reorder_connection(&list, &[], "c", "a", false).unwrap();
        assert_eq!(
            order_of(&before),
            vec![pair("c", None), pair("a", None), pair("b", None)]
        );
        let after = reorder_connection(&list, &[], "a", "c", true).unwrap();
        assert_eq!(
            order_of(&after),
            vec![pair("b", None), pair("c", None), pair("a", None)]
        );
    }

    #[test]
    fn reorder_into_another_bucket_changes_group() {
        let list = vec![conn("a", Some("A")), conn("b", Some("B")), conn("u", None)];
        // 未分组的 u 拖到 B 组的 b 前面 → 进 B 组、排在 b 前
        let next = reorder_connection(&list, &[], "u", "b", false).unwrap();
        assert_eq!(
            order_of(&next),
            vec![
                pair("a", Some("A")),
                pair("u", Some("B")),
                pair("b", Some("B"))
            ]
        );
        // 反过来:有组的 a 拖到未分组的 u 之后 → 变未分组
        let next = reorder_connection(&list, &[], "a", "u", true).unwrap();
        assert_eq!(
            order_of(&next),
            vec![pair("b", Some("B")), pair("u", None), pair("a", None)]
        );
    }

    #[test]
    fn reorder_target_group_name_is_trimmed() {
        // 目标连接的组名带空白(手改配置的存量):写进去的是桶名(已 trim)
        let list = vec![conn("a", Some(" A ")), conn("u", None)];
        let next = reorder_connection(&list, &[], "u", "a", true).unwrap();
        assert_eq!(next[1].group.as_deref(), Some("A"));
    }

    #[test]
    fn reorder_noop_returns_none() {
        let list = vec![conn("a", None), conn("b", None)];
        assert!(
            reorder_connection(&list, &[], "a", "a", true).is_none(),
            "拖到自己身上"
        );
        assert!(
            reorder_connection(&list, &[], "x", "a", true).is_none(),
            "被拖的不存在"
        );
        assert!(
            reorder_connection(&list, &[], "a", "x", true).is_none(),
            "目标不存在"
        );
        assert!(
            reorder_connection(&list, &[], "a", "b", false).is_none(),
            "落点就是原位"
        );
        assert!(
            reorder_connection(&list, &[], "b", "a", true).is_none(),
            "落点就是原位"
        );
    }

    #[test]
    fn reorder_leaves_explicit_empty_groups_alone() {
        // 显式空组只住在 `ssh_groups` 里,拍平回去不会凭空多出连接
        let list = vec![conn("a", None), conn("b", None)];
        let next = reorder_connection(&list, &ids(&["空组"]), "b", "a", false).unwrap();
        assert_eq!(order_of(&next), vec![pair("b", None), pair("a", None)]);
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

    /// 一组未分组的连接(保存计划的老用例只关心 id)。
    fn conns(v: &[&str]) -> Vec<SshConnection> {
        v.iter().map(|id| conn(id, None)).collect()
    }

    #[test]
    fn plan_noop_when_never_enabled_and_nothing_checked() {
        assert_eq!(
            plan_assoc_save(&project(false, None), &[], &[], &conns(&["a"])),
            AssocPlan::NoOp
        );
    }

    #[test]
    fn plan_disable_when_was_enabled_and_now_empty() {
        assert_eq!(
            plan_assoc_save(&project(true, Some(vec!["a"])), &[], &[], &conns(&["a"])),
            AssocPlan::Disable
        );
    }

    #[test]
    fn plan_enable_first_time_is_not_silent() {
        assert_eq!(
            plan_assoc_save(
                &project(false, None),
                &ids(&["a"]),
                &[],
                &conns(&["a", "b"])
            ),
            AssocPlan::Enable {
                silent: false,
                was_enabled: false
            }
        );
    }

    #[test]
    fn plan_enable_unchanged_scope_is_silent_reconcile() {
        let p = project(true, Some(vec!["a", "b"]));
        assert_eq!(
            plan_assoc_save(&p, &ids(&["b", "a"]), &[], &conns(&["a", "b"])),
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
        let list = conns(&["a", "b"]);
        let p = project(true, None);
        assert_eq!(
            plan_assoc_save(&p, &all, &[], &list),
            AssocPlan::Enable {
                silent: true,
                was_enabled: true
            }
        );
        // 但缩小了范围就不是静默
        assert_eq!(
            plan_assoc_save(&p, &ids(&["a"]), &[], &list),
            AssocPlan::Enable {
                silent: false,
                was_enabled: true
            }
        );
    }

    // --- 整组关联 ---

    fn grouped_project(ids_: Vec<&str>, groups: &[&str]) -> ProjectConfig {
        ProjectConfig {
            ssh_connection_groups: ids(groups),
            ..project(true, Some(ids_))
        }
    }

    #[test]
    fn plan_only_an_empty_group_still_enables() {
        // 只勾了一个(此刻还空着的)组:用户要的就是「往这组里加的都能用」
        assert_eq!(
            plan_assoc_save(&project(false, None), &[], &ids(&["新组"]), &conns(&["a"])),
            AssocPlan::Enable {
                silent: false,
                was_enabled: false
            }
        );
    }

    #[test]
    fn plan_group_change_is_not_silent_even_if_current_scope_matches() {
        // a 在 A 组里:「单勾 a」与「整组勾 A」此刻范围一样,但将来不一样 —— 不能静默
        let list = vec![conn("a", Some("A")), conn("u", None)];
        let p = project(true, Some(vec!["a"]));
        assert_eq!(
            plan_assoc_save(&p, &[], &ids(&["A"]), &list),
            AssocPlan::Enable {
                silent: false,
                was_enabled: true
            }
        );
        // 组没变、单勾也没变 → 静默(组名带空白的存量也算同一个组)
        let p = grouped_project(vec!["u"], &[" A "]);
        assert_eq!(
            plan_assoc_save(&p, &ids(&["u"]), &ids(&["A"]), &list),
            AssocPlan::Enable {
                silent: true,
                was_enabled: true
            }
        );
    }

    #[test]
    fn initial_groups_only_when_enabled_and_still_existing() {
        let names = ids(&["A", "B"]);
        let p = grouped_project(vec![], &["B", "已删的组"]);
        assert_eq!(initial_checked_groups(&p, &names), ids(&["B"]));
        // 未启用:一个组都不勾(默认态是单条全选)
        let mut off = p.clone();
        off.ssh_mcp_enabled = false;
        assert!(initial_checked_groups(&off, &names).is_empty());
        // 旧配置「未设范围」:单条已全勾,组不勾
        let legacy = ProjectConfig {
            ssh_connection_groups: ids(&["A"]),
            ..project(true, None)
        };
        assert!(initial_checked_groups(&legacy, &names).is_empty());
    }

    #[test]
    fn effective_scope_unions_checked_and_group_members() {
        let list = vec![
            conn("a1", Some("A")),
            conn("b1", Some("B")),
            conn("a2", Some("A")),
            conn("u", None),
        ];
        assert_eq!(
            effective_scope(&ids(&["u", "a2"]), &ids(&["A"]), &list),
            ids(&["u", "a2", "a1"])
        );
        // 只给组:就是这组罩住的连接(弹窗据此把行画成已勾)
        assert_eq!(effective_scope(&[], &ids(&["B"]), &list), ids(&["b1"]));
    }

    #[test]
    fn regroup_rename_to_fresh_name_follows() {
        let list = vec![conn("a1", Some("A"))];
        let mut projects = vec![grouped_project(vec!["x"], &["A", "C"])];
        assert!(regroup_project_scopes(
            &mut projects,
            &list,
            "A",
            Some("新A")
        ));
        assert_eq!(projects[0].ssh_connection_groups, ids(&["新A", "C"]));
        assert_eq!(
            projects[0].ssh_connection_ids.as_deref(),
            Some(&ids(&["x"])[..]),
            "改名不该动显式列表"
        );
    }

    #[test]
    fn regroup_dissolve_or_merge_flattens_members_into_explicit_ids() {
        let list = vec![
            conn("a1", Some("A")),
            conn("b1", Some("B")),
            conn("a2", Some("A")),
        ];
        // 解散 / 并进别的组:旧组成员摊成显式 id,有效范围原样不变,不多看到 B 组的 b1
        let mut projects = vec![grouped_project(vec!["a2"], &["A"])];
        let before = projects[0].effective_ssh_connection_ids(&list);
        assert!(regroup_project_scopes(&mut projects, &list, "A", None));
        assert!(projects[0].ssh_connection_groups.is_empty());
        assert_eq!(
            projects[0].ssh_connection_ids.as_deref(),
            Some(&ids(&["a2", "a1"])[..])
        );
        let mut after_list = list.clone();
        for c in &mut after_list {
            if c.group.as_deref() == Some("A") {
                c.group = Some("B".into());
            }
        }
        let mut before_ids = before.unwrap();
        let mut after_ids = projects[0]
            .effective_ssh_connection_ids(&after_list)
            .unwrap();
        before_ids.sort();
        after_ids.sort();
        assert_eq!(before_ids, after_ids, "合并进 B 之后范围仍是原来那几台");
    }

    #[test]
    fn regroup_leaves_unrelated_projects_alone() {
        let list = vec![conn("a1", Some("A"))];
        let mut projects = vec![grouped_project(vec!["x"], &["B"]), project(false, None)];
        assert!(!regroup_project_scopes(&mut projects, &list, "A", None));
        assert_eq!(projects[0].ssh_connection_groups, ids(&["B"]));
        assert_eq!(
            projects[0].ssh_connection_ids.as_deref(),
            Some(&ids(&["x"])[..])
        );
    }

    // --- 复制连接 ---

    fn named(id: &str, name: &str) -> SshConnection {
        SshConnection {
            name: name.to_string(),
            ..conn(id, None)
        }
    }

    #[test]
    fn duplicate_name_picks_first_free_number() {
        let list = vec![named("1", "prod")];
        assert_eq!(duplicate_name("prod", &list), "prod (1)");
        let list = vec![
            named("1", "prod"),
            named("2", "prod (1)"),
            named("3", "prod (3)"),
        ];
        assert_eq!(duplicate_name("prod", &list), "prod (2)", "取第一个空号");
    }

    #[test]
    fn duplicate_name_of_a_copy_does_not_stack_suffixes() {
        let list = vec![named("1", "prod"), named("2", "prod (1)")];
        assert_eq!(duplicate_name("prod (1)", &list), "prod (2)");
        // 括号里不是纯数字的不算副本尾巴
        assert_eq!(duplicate_name("db (主库)", &[]), "db (主库) (1)");
        // 整个名字就是「(3)」:前面没有名字,不当副本尾巴
        assert_eq!(duplicate_name("(3)", &[]), "(3) (1)");
    }

    #[test]
    fn duplicate_connection_copies_everything_but_id_and_name() {
        let mut src = conn("c1", Some("内网"));
        src.name = "prod".into();
        src.port = 2222;
        src.password = Some("enc:v1:xxxx".into());
        src.identity_file = Some("/k".into());
        src.extra = serde_json::from_str(r#"{"jumpHost":"bastion"}"#).unwrap();
        let list = vec![src.clone(), conn("c2", None)];

        let next = duplicate_connection(&list, "c1", "c9".into()).expect("源连接存在");
        assert_eq!(
            next.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["c1", "c9", "c2"],
            "副本紧跟在原连接后面"
        );
        let copy = &next[1];
        assert_eq!(copy.name, "prod (1)");
        assert_eq!(copy.host, src.host);
        assert_eq!(copy.port, 2222);
        assert_eq!(copy.user, src.user);
        assert_eq!(copy.password, src.password, "信封原样照抄,不再封一层");
        assert_eq!(copy.identity_file, src.identity_file);
        assert_eq!(copy.group, src.group);
        assert_eq!(copy.extra, src.extra);

        assert!(duplicate_connection(&list, "gone", "c9".into()).is_none());
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
