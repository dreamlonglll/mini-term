//! 编排控制面的桌面侧接线(ADR 0003 / 工单 02)。
//!
//! `mt-ai` 的控制面不认识配置层,项目表与启动器名单经
//! [`mt_ai::OrchestratorHost`] 注入 —— 与移动端中转的 `RelayHost` 同一个模式,
//! 连「主线程刷新、后台线程只读一份镜像」的处理都照抄
//! (控制端点跑在 hook 那条 HTTP 线程上,碰不得 gpui 实体)。
//!
//! ```text
//! AppStore(主线程) ──refresh_orchestrator_mirror()──→ OrchestratorMirror
//!                                                            │只读
//!                              hook/控制 HTTP 线程 ──HostImpl─┘
//! ```
//!
//! # 分组归属在这里算,不在 mt-ai 算
//!
//! 「可达范围 = 本项目 + 同分组」这条裁决住在 `mt_ai::control`,但**分组是什么
//! 形状**是配置层的事(`project_tree` 是棵可嵌套的树)。于是这里把它压平成一个
//! 可比较的 `group_id`,控制面只做「标签相等」的判断。

use std::sync::Arc;

use mt_ai::{ControlLauncher, ControlProject, OrchestratorHost};
use mt_config::{AppConfig, ProjectTreeItem};
use parking_lot::Mutex;

/// 起 PTY 时要不要给这个 pane 发一枚编排令牌。
///
/// 做成具名类型而不是裸 `bool`:起 PTY 的调用点有七八处,绝大多数是「不发」,
/// 一个 `false` 混进参数表里读不出是在说什么;也免得哪天顺手传成 `true`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrchestratorGrant {
    /// 普通 pane:什么都不注入。
    #[default]
    None,
    /// 编排者 pane:注入令牌 + 自身身份。
    Grant,
}

impl OrchestratorGrant {
    /// 授予与否**只看启动器上的那个开关** —— 这是唯一的信任根。
    pub fn from_launcher(launcher: &mt_config::AiLauncher) -> Self {
        if launcher.orchestration {
            Self::Grant
        } else {
            Self::None
        }
    }

    pub fn is_granted(self) -> bool {
        matches!(self, Self::Grant)
    }
}

/// 主线程按配置刷新、控制面 HTTP 线程只读的那一份桌面状态切面。
#[derive(Default)]
pub struct OrchestratorMirror {
    launchers: Vec<ControlLauncher>,
    projects: Vec<ControlProject>,
}

/// 镜像的共享句柄:[`AiBridge`] 持一份给刷新用,[`HostImpl`] 持一份给读。
///
/// [`AiBridge`]: crate::ai::AiBridge
pub type SharedMirror = Arc<Mutex<OrchestratorMirror>>;

impl OrchestratorMirror {
    /// 整体替换。**每次配置落盘都刷**(见 `store::layout::save_config_now`)——
    /// 「改分组即时生效」靠的就是这条,控制面自己每次请求现读镜像。
    pub fn replace(&mut self, config: &AppConfig) {
        self.launchers = launcher_facets(config);
        self.projects = project_facets(config);
    }
}

/// 注入给 `mt-ai` 的宿主实现。
pub struct HostImpl {
    mirror: SharedMirror,
}

impl HostImpl {
    pub fn new(mirror: SharedMirror) -> Self {
        Self { mirror }
    }
}

impl OrchestratorHost for HostImpl {
    fn launchers(&self) -> Vec<ControlLauncher> {
        self.mirror.lock().launchers.clone()
    }

    fn projects(&self) -> Vec<ControlProject> {
        self.mirror.lock().projects.clone()
    }
}

/// 启动器投影:**全量**(任何启动器都能当乐手,ADR 0003),只留 id 与展示名。
///
/// 「允许编排」那个开关**不投影** —— 它是「谁能当编排者」的授予位,不是
/// 「谁能被起」的过滤位;而受编排会话一律不发令牌(禁套娃,工单 03),
/// 编排者知道这个位也没有用处。
pub fn launcher_facets(config: &AppConfig) -> Vec<ControlLauncher> {
    config
        .mobile_relay
        .as_ref()
        .map(|r| r.launchers.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|l| ControlLauncher {
            id: l.id.clone(),
            name: l.name.clone(),
        })
        .collect()
}

/// 项目投影:每个项目配一个**已解析好的所属分组 id**。
///
/// 三条口径:
///
/// - 嵌套分组取**最内层**那个(最具体的那次「这是一件事」的表达);
/// - 不在项目树里的项目(顶层 / 异常配置)是未分组 —— 只能在本项目内编排;
/// - 子项目(worktree「设为项目」,不进项目树)**继承父项目的分组** ——
///   它与父项目本来就是同一件事,否则从 worktree 里起的编排者够不到主仓项目。
pub fn project_facets(config: &AppConfig) -> Vec<ControlProject> {
    let mut groups: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(tree) = config.project_tree.as_ref() {
        collect_groups(tree, None, &mut groups);
    }

    config
        .projects
        .iter()
        .map(|p| ControlProject {
            id: p.id.clone(),
            name: p.name.clone(),
            path: p.path.clone(),
            group_id: resolve_group(config, &groups, &p.id),
        })
        .collect()
}

/// 深度优先压平项目树:每个项目 id → 最内层分组 id。
fn collect_groups(
    items: &[ProjectTreeItem],
    current: Option<&str>,
    out: &mut std::collections::HashMap<String, String>,
) {
    for item in items {
        match item {
            ProjectTreeItem::Group(group) => {
                collect_groups(&group.children, Some(&group.id), out);
            }
            ProjectTreeItem::ProjectId(id) => {
                if let Some(group) = current {
                    out.entry(id.clone()).or_insert_with(|| group.to_string());
                }
            }
        }
    }
}

/// 项目自己的分组;没有就沿 `parent_project_id` 往上找(子项目继承父项目)。
///
/// 上溯**有上限**:配置是用户可编辑的数据,环状 `parentProjectId` 不该把
/// 控制面转死在这里。
fn resolve_group(
    config: &AppConfig,
    groups: &std::collections::HashMap<String, String>,
    project_id: &str,
) -> Option<String> {
    const MAX_DEPTH: usize = 8;
    let mut id = project_id.to_string();
    for _ in 0..MAX_DEPTH {
        if let Some(group) = groups.get(&id) {
            return Some(group.clone());
        }
        let parent = config
            .projects
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.parent_project_id.clone())?;
        if parent == id {
            return None;
        }
        id = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::{AiLauncher, MobileRelayConfig, ProjectGroup};

    fn project(id: &str) -> mt_config::ProjectConfig {
        mt_config::ProjectConfig {
            id: id.to_string(),
            name: format!("项目{id}"),
            path: format!("D:\\repos\\{id}"),
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
        }
    }

    fn config(
        projects: Vec<mt_config::ProjectConfig>,
        tree: Option<Vec<ProjectTreeItem>>,
    ) -> AppConfig {
        AppConfig {
            projects,
            project_tree: tree,
            ..AppConfig::default()
        }
    }

    fn group(id: &str, children: Vec<ProjectTreeItem>) -> ProjectTreeItem {
        ProjectTreeItem::Group(ProjectGroup {
            id: id.to_string(),
            name: format!("组{id}"),
            collapsed: false,
            children,
        })
    }

    fn leaf(id: &str) -> ProjectTreeItem {
        ProjectTreeItem::ProjectId(id.to_string())
    }

    #[test]
    fn 顶层项目未分组() {
        let c = config(vec![project("a"), project("b")], Some(vec![leaf("a"), leaf("b")]));
        let facets = project_facets(&c);
        assert!(facets.iter().all(|p| p.group_id.is_none()));
        assert_eq!(facets[0].path, "D:\\repos\\a");
    }

    #[test]
    fn 同组项目拿到同一个分组_id() {
        let c = config(
            vec![project("a"), project("b"), project("c")],
            Some(vec![group("g1", vec![leaf("a"), leaf("b")]), leaf("c")]),
        );
        let facets = project_facets(&c);
        let by = |id: &str| {
            facets
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .group_id
                .clone()
        };
        assert_eq!(by("a"), Some("g1".into()));
        assert_eq!(by("a"), by("b"));
        assert_eq!(by("c"), None);
    }

    /// 嵌套分组取**最内层**:更具体的那次表达说了算。
    #[test]
    fn 嵌套分组取最内层() {
        let c = config(
            vec![project("a"), project("b")],
            Some(vec![group(
                "outer",
                vec![leaf("a"), group("inner", vec![leaf("b")])],
            )]),
        );
        let facets = project_facets(&c);
        assert_eq!(facets[0].group_id, Some("outer".into()));
        assert_eq!(facets[1].group_id, Some("inner".into()));
    }

    /// 不在树里的项目(异常配置兜底)是未分组,不该凭空并进某个组。
    #[test]
    fn 树外项目未分组() {
        let c = config(vec![project("a"), project("ghost")], Some(vec![group("g1", vec![leaf("a")])]));
        let facets = project_facets(&c);
        assert_eq!(facets[1].group_id, None);
    }

    /// 子项目(worktree「设为项目」)继承父项目的分组 —— 它不进项目树。
    #[test]
    fn 子项目继承父项目分组() {
        let mut child = project("child");
        child.parent_project_id = Some("a".into());
        let c = config(
            vec![project("a"), child],
            Some(vec![group("g1", vec![leaf("a")])]),
        );
        let facets = project_facets(&c);
        assert_eq!(facets[1].group_id, Some("g1".into()));
    }

    /// 环状 parentProjectId(用户手改配置)不许把控制面转死。
    #[test]
    fn 父子成环不死循环() {
        let mut a = project("a");
        a.parent_project_id = Some("b".into());
        let mut b = project("b");
        b.parent_project_id = Some("a".into());
        let c = config(vec![a, b], None);
        assert!(project_facets(&c).iter().all(|p| p.group_id.is_none()));
    }

    /// 启动器投影是全量的,且**只带 id 与名字**(命令不给编排者看)。
    #[test]
    fn 启动器投影全量且不带命令() {
        let c = AppConfig {
            mobile_relay: Some(MobileRelayConfig {
                relay_url: String::new(),
                desktop_key: String::new(),
                launchers: vec![
                    AiLauncher {
                        id: "l1".into(),
                        name: "Claude".into(),
                        shell: Some("wsl".into()),
                        command: "claude --dangerously".into(),
                        orchestration: true,
                    },
                    AiLauncher {
                        id: "l2".into(),
                        name: "Codex".into(),
                        shell: None,
                        command: "codex".into(),
                        orchestration: false,
                    },
                ],
            }),
            ..AppConfig::default()
        };
        let facets = launcher_facets(&c);
        assert_eq!(facets.len(), 2, "没勾「允许编排」的也能当乐手");
        assert_eq!(facets[0].id, "l1");
        assert_eq!(facets[0].name, "Claude");
        // 类型上就没有命令字段,这里钉的是「别哪天顺手加回去」
        let json = serde_json::to_string(&serde_json::json!({
            "id": facets[0].id,
            "name": facets[0].name,
        }))
        .unwrap();
        assert!(!json.contains("dangerously"));
    }

    /// 授予与否只看启动器上的开关,不看名字、命令或别的任何东西。
    #[test]
    fn 授予只认启动器开关() {
        let mut l = AiLauncher {
            id: "l".into(),
            name: "Claude".into(),
            shell: None,
            command: "claude".into(),
            orchestration: false,
        };
        assert_eq!(OrchestratorGrant::from_launcher(&l), OrchestratorGrant::None);
        assert!(!OrchestratorGrant::from_launcher(&l).is_granted());

        l.orchestration = true;
        assert!(OrchestratorGrant::from_launcher(&l).is_granted());
        // 缺省态必须是「不发」
        assert!(!OrchestratorGrant::default().is_granted());
    }

    #[test]
    fn 镜像整体替换() {
        let mut mirror = OrchestratorMirror::default();
        let c = config(vec![project("a")], None);
        mirror.replace(&c);
        assert_eq!(mirror.projects.len(), 1);

        mirror.replace(&AppConfig::default());
        assert!(mirror.projects.is_empty(), "整体替换,不许留旧项目");
    }
}
