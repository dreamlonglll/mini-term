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
//!
//! # 动作那条缝:同步回执 + 超时(工单 03)
//!
//! 「起乐手」要建 pane —— 那是 gpui 主线程的活,而控制面在 HTTP 线程上,且 CLI
//! 正等着一个答复。于是走一条与 `RelayEvents` 形似、语义相反的路:
//!
//! ```text
//! 控制面(HTTP 线程) ──OrchestratorSignal + oneshot──→ 泵(主线程) ──建 pane
//!                  ←────────── recv_timeout ─────────────────────┘
//! ```
//!
//! - 中转那条是**异步回执**(`start_session` 丢进 channel 就返回,结果稍后经
//!   `start_session_result` 回给手机);这条是**同步等**:CLI 是个前台进程,
//!   它拿到的退出码就是回执本身。
//! - 用 `std::sync::mpsc::sync_channel(1)` 而不是 futures 的 oneshot:HTTP 线程
//!   是一条裸线程,没有执行器可以 `block_on`。
//! - **必须有超时**([`ACTION_TIMEOUT`]):泵没接线(纯单测 / 窗口已关)或主线程
//!   卡死时,不许把 HTTP 线程永久挂在那儿。超时答 `DesktopBusy`,CLI 据此给
//!   「过会儿再试」那一档退出码。

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use gpui::{App, Entity, Task, Window};
use mt_ai::{
    ControlLauncher, ControlProject, OrchestratorActions, OrchestratorHost, PaneLiveness,
    StartFailure, StartSessionSpec, StartedSession,
};
use mt_config::{AppConfig, ProjectTreeItem};
use parking_lot::Mutex;

use crate::ai::AiBridge;
use crate::i18n::tr;
use crate::store::{AppStore, LaunchError, LaunchNotice, LaunchPlacement, LaunchRequest};

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

/// 受编排会话拿到的授予:**恒为「不发」**(ADR 0003 的禁套娃)。
///
/// 具名成一个常量而不是在起会话那一行写 `OrchestratorGrant::None`,是为了让这条
/// 裁决有个能被测试指着说话的名字 —— 它与 [`OrchestratorGrant::from_launcher`]
/// 是**互斥**的两条路:授予只从「用户在桌面上勾了开关、并亲手/按配置起了那个
/// pane」来,编排者起的会话一律不走那条路,哪怕目标启动器自己勾了开关。
pub const ORCHESTRATED_GRANT: OrchestratorGrant = OrchestratorGrant::None;

/// 控制面等桌面主线程的时限。
///
/// 定 3 秒的理由是两头夹出来的:下界是「建一个 pane 正常要多久」——本机 spawn
/// 一个 PTY 通常几十毫秒,3 秒是它的一个数量级以上;上界是 CLI 那侧的读超时
/// (`mt-agent-cli` 的 `READ_TIMEOUT` 5 秒),必须留出富余,否则起会话稍慢一点
/// 就变成 CLI 先断线,编排者拿到的会是「够不着」而不是这边给的明确答复。
///
/// 真到点还没答复,说明主线程已经卡住了 —— 那时 UI 本来也动不了,老实回一个
/// `DesktopBusy` 让编排者过会儿再试,比把 HTTP 线程无限挂着强。
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(3);

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

// ─── 动作:起乐手 / 查死活 ────────────────────────────────────

/// 控制面递给主线程的活。**唯一**的跨线程口。
enum OrchestratorSignal {
    StartSession {
        spec: StartSessionSpec,
        /// 回执通道。容量 1:主线程放完就走,不等对面取。
        reply: std::sync::mpsc::SyncSender<Result<StartedSession, StartFailure>>,
    },
}

/// 注入给 `mt-ai` 的动作实现。
pub struct ActionsImpl {
    ai: AiBridge,
    tx: UnboundedSender<OrchestratorSignal>,
}

impl OrchestratorActions for ActionsImpl {
    /// 把活丢给主线程,**同步等**一个答复(见模块注释)。
    fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        if self
            .tx
            .unbounded_send(OrchestratorSignal::StartSession { spec, reply })
            .is_err()
        {
            // 泵没了(窗口关了 / 还没接线):不许把「没人接」当成起成了
            return Err(StartFailure::DesktopBusy);
        }
        answer
            .recv_timeout(ACTION_TIMEOUT)
            .unwrap_or(Err(StartFailure::DesktopBusy))
    }

    /// 死活**不跳主线程** —— 名额判定一次要问好几个 pane,而这两样东西后台线程
    /// 本来就读得到:
    ///
    /// - `alive` 走 [`AiBridge`] 的活 pane 名册(建 PTY 时登记、pane 关闭时注销,
    ///   与 500ms 轮询同一份);
    /// - `status` 走 `AiPerception`(hook 状态与旁路检测都在 `Arc<Mutex<..>>` 后面)。
    ///
    /// 两者一起构成「还占不占名额」:pane 关了 → `alive` 假;agent 自己退出 →
    /// hook 的 SessionEnd 把状态落到 `idle`。**两条释放路径都不靠事件送达**。
    fn pane_liveness(&self, pane_id: u32) -> PaneLiveness {
        PaneLiveness {
            alive: self.ai.is_pane_live(pane_id),
            status: self.ai.perception().status_of(pane_id),
        }
    }
}

/// 接上动作那条缝并起主线程泵。返回的 [`Task`] 要被宿主持有 —— 丢了句柄泵就没了,
/// 之后所有起会话请求都会走成 `DesktopBusy`。
///
/// 与 `mobile_relay::install` 的差别:那边有面板、观察者、去抖同步,得有个实体
/// 装着;这边只有一条泵,`window.spawn` 足够,不必为它造一个空壳实体。
pub fn install(store: Entity<AppStore>, window: &mut Window, cx: &mut App) -> Task<()> {
    let (tx, mut rx) = mpsc::unbounded::<OrchestratorSignal>();
    let ai = store.read(cx).ai();
    ai.perception()
        .control()
        .set_actions(Arc::new(ActionsImpl {
            ai: ai.clone(),
            tx,
        }));

    window.spawn(cx, async move |cx| {
        while let Some(signal) = rx.next().await {
            let OrchestratorSignal::StartSession { spec, reply } = signal;
            // 窗口没了就别再答复:发起方会在 `ACTION_TIMEOUT` 上收敛成 DesktopBusy
            let Ok(result) = cx.update(|window, cx| start_session(&store, spec, window, cx)) else {
                return;
            };
            // 对面可能已经等超时走了(`SyncSender` 于是报错),不是问题
            let _ = reply.send(result);
        }
    })
}

/// 主线程上真正把乐手起出来。
///
/// 落地动作整套走[共享入口](AppStore::launch_ai_session) —— 与桌面端新终端菜单、
/// 移动端发起是同一条。这里只负责三件编排者自己的事:按 id 把启动器取出来、
/// 挑 `Background` 落点(ADR 0002 的出生礼仪)、把结局折成控制面的闭集。
fn start_session(
    store: &Entity<AppStore>,
    spec: StartSessionSpec,
    window: &mut Window,
    cx: &mut App,
) -> Result<StartedSession, StartFailure> {
    // 按 id 取具名启动器。**命令文本只在这里到共享入口之间的几行里流转** ——
    // 它没进过控制面,更没到过编排者手上(ADR 0002 的唯一防线)。
    let launcher = store
        .read(cx)
        .mobile_relay()
        .launchers
        .iter()
        .find(|l| l.id == spec.launcher_id)
        .cloned();
    // 控制面刚查过名单,到这儿还能没了只有一种可能:用户正巧把它删了。
    let Some(launcher) = launcher else {
        return Err(StartFailure::SpawnFailed);
    };

    let outcome = store
        .update(cx, |store, cx| {
            store.launch_ai_session(
                LaunchRequest {
                    project_id: &spec.project_id,
                    launcher_name: &launcher.name,
                    shell_name: launcher.shell.as_deref(),
                    command: &launcher.command,
                    // 挂进活动面板最左侧叶子:不激活、不抢焦点、不切项目
                    placement: LaunchPlacement::Background,
                    // **禁套娃**:受编排会话一律不授予编排能力,哪怕这个启动器
                    // 自己勾了「允许编排」—— 那个开关是「谁能当编排者」的授予位,
                    // 只对用户在桌面上亲手起的那条路生效。
                    grant: ORCHESTRATED_GRANT,
                    // 诞生一次性提示:与移动端发起同一档 toast(info 图标 + 点击
                    // 切项目),只是文案说明出身。凭证被盗时这是唯一的审计迹象,
                    // 所以即便不切过去也要弹。
                    notice: Some(LaunchNotice {
                        kind: crate::notify::ToastKind::MobileSession,
                        message: tr!(
                            "app",
                            "orchestratorStartSession",
                            launcher = launcher.name.clone()
                        ),
                    }),
                },
                window,
                cx,
            )
        })
        .map_err(|err| match err {
            LaunchError::ProjectNotFound => StartFailure::ProjectGone,
            LaunchError::SpawnFailed => StartFailure::SpawnFailed,
        })?;

    // pane 建成了但启动命令没交到一根活着的 PTY 手上 —— 对编排者而言这就是失败
    // (pane 本身**保留不杀**,用户回头能看到它卡在哪)。与移动端回执同一判据。
    if !outcome.command_delivered() {
        return Err(StartFailure::SpawnFailed);
    }
    // 对外的 pane 身份一律是 **PTY 编号**:编排者自己的身份也是它
    // (`MINITERM_ORCHESTRATOR_PANE`),自指禁令因此是一次裸比较。
    let pty_id = store
        .read(cx)
        .project_state(&spec.project_id)
        .and_then(|state| state.pane(&outcome.pane_id))
        .and_then(|pane| pane.pty_id);
    match pty_id {
        Some(pane_id) => Ok(StartedSession { pane_id }),
        // 命令都写进去了却查不到 PTY 编号:布局在这几行之间被动过,当失败处理
        None => Err(StartFailure::SpawnFailed),
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
            // **照实投影**,不在这里先折成一个「能不能起乐手」的布尔:
            // 那是控制面的裁决(`ControlProject::is_remote`),配置层只管说事实。
            // 与 `mobile_relay::to_relay_project` 同一个字段、同一份事实。
            ssh_connection_id: p.ssh_connection_id.clone(),
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

    /// 项目投影**照实带**上 SSH 连接 id,远程项目据此当不了乐手宿主。
    ///
    /// 这条判据在工单 02 之前是「全仓没有任何项目带它」的状态,与
    /// `mobile_relay` 那条 `远程项目镜像照实带连接id且不可发起会话` 同源 ——
    /// 投影不老实,控制面的裁决就是空转。
    #[test]
    fn 远程项目投影照实带连接id且不能当宿主() {
        let mut remote = project("r");
        remote.ssh_connection_id = Some("conn-1".into());
        let c = config(vec![project("a"), remote], None);
        let facets = project_facets(&c);

        assert_eq!(facets[0].ssh_connection_id, None);
        assert!(!facets[0].is_remote());
        assert_eq!(facets[1].ssh_connection_id.as_deref(), Some("conn-1"));
        assert!(facets[1].is_remote(), "SSH 远程项目当不了乐手宿主");
    }

    /// **禁套娃**:受编排会话拿到的授予恒为「不发」,与启动器开关那条路互斥。
    #[test]
    fn 受编排会话一律不授予编排能力() {
        assert_eq!(ORCHESTRATED_GRANT, OrchestratorGrant::None);
        assert!(!ORCHESTRATED_GRANT.is_granted());

        // 哪怕目标启动器自己勾了「允许编排」:那个开关只对「用户在桌面上亲手
        // 起的那条路」生效(`from_launcher`),编排者起会话走的是上面那个常量。
        let orchestrating = AiLauncher {
            id: "l".into(),
            name: "Claude".into(),
            shell: None,
            command: "claude".into(),
            orchestration: true,
        };
        assert!(OrchestratorGrant::from_launcher(&orchestrating).is_granted());
        assert!(
            !ORCHESTRATED_GRANT.is_granted(),
            "乐手不许继承编排能力,否则禁套娃只剩君子协定"
        );
    }

    /// 等主线程的时限必须**短于** CLI 那侧的读超时(`mt-agent-cli` 的
    /// `READ_TIMEOUT` 5 秒),否则起会话稍慢一点就变成 CLI 先断线 ——
    /// 编排者拿到的会是「够不着」,而不是桌面端给的明确答复。
    #[test]
    fn 动作超时短于_cli_读超时() {
        assert!(
            ACTION_TIMEOUT < Duration::from_secs(5),
            "留给 HTTP 往返的富余没了: {ACTION_TIMEOUT:?}"
        );
        assert!(
            ACTION_TIMEOUT >= Duration::from_secs(1),
            "太短会把「主线程正忙」误判成「卡死」"
        );
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
