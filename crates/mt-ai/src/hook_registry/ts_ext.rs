use super::*;

// ─── 进程内 TS 扩展（oh-my-pi / pi）注册/卸载 ───
//
// 两家同一套做法（[`TsExtension`]）：整份模板写进 `{agent_dir}/extensions/miniterm.ts`。
// 「注册现状」的判据是扩展文件在场且带 `miniterm-hook` 标识；事件计数按文件里
// `pi.on("<event>"` 的出现与否对账，老版本模板缺的事件就显示成「旧版本 N/M」。
// 启动期自愈（`sync_*_hooks_if_registered`）在文件内容与当前模板不同时整份重写
// ——模板修了 bug / 加了事件，老用户下次启动即拿到，不必再点一次「注册」。
//
// 两家的扩展 API 已经分叉（pi 有 `agent_settled`、换会话走 shutdown + start；omp
// 有 `session_switch` / 工具审批事件），模板各写一份、不能互换。文件名相同是有意的：
// 有人把 omp 的模板手抄进 pi 的扩展目录（issue #85 就是这么试的），它带着标识，
// pi 的自愈会认出「旧版本」并换成 pi 的模板。设了 `PI_CODING_AGENT_DIR` 时两家落在
// 同一个文件上，归属改按模板自报的 `const AGENT` 认（见 [`TsExtension::owns`]）。

/// 写进扩展目录的文件名（omp 与 pi 相同，各在各的 agent 目录下）
pub(super) const EXTENSION_FILE: &str = "miniterm.ts";

/// omp 扩展源码（随主程序编译进来，注册时整份落盘）。文件本身不含任何机器相关
/// 信息：端口走 `MINITERM_HOOK_PORT` / hook-server.json，pane 走 `MINITERM_PTY_ID`，
/// 所以任何机器上的内容都逐字相同，「是否需要刷新」按整份相等判断即可。
pub(super) const OMP_EXTENSION_SOURCE: &str = include_str!("../../assets/miniterm-omp.ts");

/// omp 扩展订阅的 omp 事件（omp 自己的 snake_case 事件名）。面板的「已注册 N/M」
/// 与启动期补齐按它对账；与 `OMP_EXTENSION_SOURCE` 里的 `pi.on(...)` 逐条对应
/// （单测钉住两边一致）。
pub(super) const OMP_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "session_switch",
    "session_branch",
    "session_shutdown",
    "agent_start",
    "agent_end",
    "tool_call",
    "tool_result",
    "tool_approval_requested",
    "tool_approval_resolved",
    "auto_retry_start",
    "auto_compaction_start",
    "auto_compaction_end",
];

/// omp 扩展会上报给 hook 服务器的事件名（PascalCase，与 Claude 同名）。
/// `hook_server::map_event_to_status` 必须认识其中每一个（`SessionEnd` 单独处理），
/// 由 hook_server 的单测钉住——漏一个就是一段状态空洞。
/// （`pub` 而非 `pub(crate)`：它只被单测消费，crate 私有会在非测试构建里报 dead_code。）
pub const OMP_REPORTED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Elicitation",
    "ElicitationResult",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "PreCompact",
    "PostCompact",
];

/// omp 的 agent 目录（纯函数，环境取值由 [`omp_agent_dir`] 负责）：
/// `PI_CODING_AGENT_DIR` 优先；否则 `~/{PI_CONFIG_DIR 或 .omp}/agent`，带 `PI_PROFILE`
/// 时是 `~/.omp/profiles/{profile}/agent`——与 omp 自身 `getAgentDir()` 的口径一致。
/// Linux 上经 `omp config init-xdg` 迁走的 XDG 布局不支持：那是罕见的显式迁移。
pub(super) fn omp_agent_dir_from(
    home: &std::path::Path,
    coding_agent_dir: Option<&str>,
    config_dir: Option<&str>,
    profile: Option<&str>,
) -> PathBuf {
    if let Some(dir) = coding_agent_dir.filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    let config_dir = config_dir
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(".omp");
    let root = home.join(config_dir);
    let root = match profile.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => root.join("profiles").join(p),
        None => root,
    };
    root.join("agent")
}

/// omp 的用户级 agent 目录（缺省 `~/.omp/agent`）
pub fn omp_agent_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let env = |k: &str| std::env::var(k).ok();
    Some(omp_agent_dir_from(
        &home,
        env("PI_CODING_AGENT_DIR").as_deref(),
        env("PI_CONFIG_DIR").as_deref(),
        env("PI_PROFILE").as_deref(),
    ))
}

/// pi 扩展源码（同 [`OMP_EXTENSION_SOURCE`]，不含任何机器相关信息）
pub(super) const PI_EXTENSION_SOURCE: &str = include_str!("../../assets/miniterm-pi.ts");

/// pi 扩展订阅的 pi 事件，与 `PI_EXTENSION_SOURCE` 里的 `pi.on(...)` 逐条对应
/// （单测钉住两边一致）。`agent_settled` 需 pi ≥ 0.80.4、`ui_prompt_*` 需 ≥ 0.84.4；
/// 更早的 pi 收不到这几个事件，回合结束靠停摆兜底收敛（不播报完成）。
pub(super) const PI_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "session_shutdown",
    "agent_start",
    "agent_end",
    "agent_settled",
    "tool_call",
    "tool_result",
    "ui_prompt_start",
    "ui_prompt_end",
];

/// pi 扩展会上报给 hook 服务器的事件名，口径同 [`OMP_REPORTED_EVENTS`]。
pub const PI_REPORTED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Elicitation",
    "ElicitationResult",
];

/// pi 的 agent 目录（纯函数，环境取值由 [`pi_agent_dir`] 负责）：`PI_CODING_AGENT_DIR`
/// 优先（与 pi 的 `normalizePath` 一样认 `~` / `~/` 前缀，Windows 上另认 `~\`），
/// 否则 `~/.pi/agent`——与 pi 自身 `getAgentDir()` 的口径一致。
///
/// ⚠️ omp 也认 `PI_CODING_AGENT_DIR`（见 [`omp_agent_dir_from`]）。设了它，两家落在
/// 同一个目录、只放得下一个 `miniterm.ts`：归属按模板里的 `const AGENT = "…"` 认
/// （[`TsExtension::owns`]），自愈不再把对方的文件当成自己的旧版改写。
pub(super) fn pi_agent_dir_from(home: &std::path::Path, coding_agent_dir: Option<&str>) -> PathBuf {
    let Some(dir) = coding_agent_dir.filter(|d| !d.trim().is_empty()) else {
        return home.join(".pi").join("agent");
    };
    if dir == "~" {
        return home.to_path_buf();
    }
    let rest = dir
        .strip_prefix("~/")
        .or_else(|| cfg!(windows).then(|| dir.strip_prefix("~\\")).flatten());
    match rest {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    }
}

/// pi 的用户级 agent 目录（缺省 `~/.pi/agent`）
pub fn pi_agent_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(pi_agent_dir_from(
        &home,
        std::env::var("PI_CODING_AGENT_DIR").ok().as_deref(),
    ))
}

/// 一家「hook 是进程内 TS 扩展」的 CLI：整份模板写进 `{agent_dir}/extensions/miniterm.ts`。
pub(super) struct TsExtension {
    /// 结果文案里的展示名（与 agent 表的 label 一致）
    pub(super) label: &'static str,
    /// 日志里的短名
    pub(super) tag: &'static str,
    pub(super) source: &'static str,
    /// 模板订阅的事件（面板「N/M」的分母）
    pub(super) events: &'static [&'static str],
    pub(super) agent_dir: fn() -> Option<PathBuf>,
    /// 正在运行的实例怎么装上新扩展：它们都只在启动时扫描扩展目录，结果文案里
    /// 必须把这句带给用户，否则他会以为注册没生效。
    pub(super) reload_hint: &'static str,
}

pub(super) const OMP_EXTENSION: TsExtension = TsExtension {
    label: "oh-my-pi",
    tag: "omp",
    source: OMP_EXTENSION_SOURCE,
    events: OMP_HOOK_EVENTS,
    agent_dir: omp_agent_dir,
    reload_hint: "正在运行的 omp 需重启或执行 /reload-plugins 才生效",
};

pub(super) const PI_EXTENSION: TsExtension = TsExtension {
    label: "pi",
    tag: "pi",
    source: PI_EXTENSION_SOURCE,
    events: PI_HOOK_EVENTS,
    agent_dir: pi_agent_dir,
    reload_hint: "正在运行的 pi 需重启或执行 /reload 才生效",
};

/// 模板自报的身份(`const AGENT = "omp";` 里的那个串);没有这一行返回 `None`。
pub(super) fn template_agent(source: &str) -> Option<&str> {
    let rest = &source[source.find("const AGENT = \"")? + "const AGENT = \"".len()..];
    rest.find('"').map(|end| &rest[..end])
}

impl TsExtension {
    /// mini-term 写入的扩展文件路径：`{agent_dir}/extensions/miniterm.ts`
    pub(super) fn path(&self) -> Option<PathBuf> {
        (self.agent_dir)().map(|d| d.join("extensions").join(EXTENSION_FILE))
    }

    /// 同一类的另一家(omp ↔ pi)
    pub(super) fn other(&self) -> &'static TsExtension {
        if self.tag == OMP_EXTENSION.tag {
            &PI_EXTENSION
        } else {
            &OMP_EXTENSION
        }
    }

    /// 两家是否落在同一个文件上(设了 `PI_CODING_AGENT_DIR`,见 [`pi_agent_dir_from`])
    fn shares_file_with_other(&self) -> bool {
        self.path().is_some_and(|p| Some(p) == self.other().path())
    }

    /// 这份文件算不算本家的(含旧版)。不带 miniterm-hook 标识的不是我们写的,一律不算
    /// ——绝不把用户自己的同名扩展算成我们的。带标识的按模板自报的身份认:
    ///
    /// - 是本家 → 算;
    /// - 是另一家:两家共用一个文件时,那就是另一家的正经注册,**不能**算(否则启动
    ///   自愈会把它改写成本家模板——只用 omp 的用户每次启动都被换成 pi 模板,完成
    ///   判定随之失效);各在各的目录时,只可能是手抄错了地方(issue #85 就是把 omp
    ///   的模板抄进了 pi 的目录),算成本家旧版,自愈好把它换掉;
    /// - 没有身份行(不是任何一版模板写出来的)→ 按所在目录认。
    pub(super) fn owns(&self, source: &str, shared_file: bool) -> bool {
        source.contains(HOOK_MARKER)
            && match template_agent(source) {
                Some(agent) => agent == self.tag || !shared_file,
                None => true,
            }
    }

    /// 某段扩展源码里订阅了本家的哪些事件（`pi.on("<event>"`）;不是本家的文件
    /// ([`Self::owns`])一律按「没注册」。
    pub(super) fn events_in_source(
        &self,
        source: &str,
        shared_file: bool,
    ) -> std::collections::HashSet<String> {
        if !self.owns(source, shared_file) {
            return std::collections::HashSet::new();
        }
        self.events
            .iter()
            .filter(|e| source.contains(&format!("pi.on(\"{}\"", e)))
            .map(|e| e.to_string())
            .collect()
    }

    pub(super) fn registered_events(&self) -> std::collections::HashSet<String> {
        let Some(path) = self.path() else {
            return std::collections::HashSet::new();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return std::collections::HashSet::new();
        };
        self.events_in_source(&content, self.shares_file_with_other())
    }

    /// 把扩展写到位。返回是否真的落盘：内容已是当前模板时一个字节不写（启动期自愈
    /// 每次启动都会走这里，不能每次都重写用户目录里的文件）。
    ///
    /// 同名文件在场却**不带**我们的标识 → 拒绝覆盖：那是用户自己的扩展，撞名也不能吃掉。
    fn write(&self) -> Result<bool, String> {
        let path = self
            .path()
            .ok_or_else(|| "无法获取 home 目录".to_string())?;
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() == Some(self.source) {
            return Ok(false);
        }
        if current.is_some_and(|c| !c.contains(HOOK_MARKER)) {
            return Err(format!(
                "{} 已存在且不是 mini-term 写入的文件，未覆盖",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建 extensions 目录失败: {}", e))?;
        }
        crate::util::atomic_write(&path, self.source.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {}", EXTENSION_FILE, e))?;
        Ok(true)
    }

    pub(super) fn register(&self) -> Result<String, String> {
        // 两家共用一个文件时,注册本家只能顶掉另一家:结果行里说清楚,别让人以为两家都装着
        let replaces_other = self.shares_file_with_other()
            && self
                .path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|c| self.other().owns(&c, true));
        let written = self.write()?;
        Ok(if written {
            let replaced = if replaces_other {
                format!(
                    "；{} 与它共用 PI_CODING_AGENT_DIR 指向的目录，原来的 {} 扩展已被替换",
                    self.other().label,
                    self.other().label
                )
            } else {
                String::new()
            };
            format!(
                "{}: 扩展已写入 (共 {} 个事件)；{}{}",
                self.label,
                self.events.len(),
                self.reload_hint,
                replaced
            )
        } else {
            format!(
                "{}: 扩展已是最新 (共 {} 个事件)",
                self.label,
                self.events.len()
            )
        })
    }

    /// 卸载：整个文件都是我们的，直接删；不带标识的同名文件、以及两家共用一个文件时
    /// 另一家的注册([`Self::owns`])都不动。
    pub(super) fn unregister(&self) -> Result<String, String> {
        let removed = self.remove_entries()?;
        Ok(if removed == 0 {
            format!(
                "{}: 未发现 mini-term 写入的 {}，无需卸载",
                self.label, EXTENSION_FILE
            )
        } else {
            format!("{}: 已移除扩展文件", self.label)
        })
    }

    pub(super) fn remove_entries(&self) -> Result<usize, String> {
        let shared = self.shares_file_with_other();
        self.path().map_or(Ok(0), |path| {
            remove_extension_at(&path, |source| self.owns(source, shared))
        })
    }

    /// 已注册用户的启动期自愈：扩展文件与当前模板不同就整份重写。
    ///
    /// 与 grok 刷新二进制副本同一个理由——mini-term 升级后模板变了，用户目录里那份
    /// 还是旧的；只要注册过就无条件对齐，从未注册的用户不碰。
    fn sync_if_registered(&self) {
        if self.registered_events().is_empty() {
            return;
        }
        match self.write() {
            Ok(true) => tracing::info!("[hook-registry] {} 扩展已刷新至当前模板", self.tag),
            Ok(false) => {}
            Err(e) => tracing::warn!("[hook-registry] {} 扩展刷新失败: {}", self.tag, e),
        }
    }
}

/// 删掉我们写的扩展文件,返回删掉的文件数(0 / 1)。文件不在、或 `owns` 不认它
/// (用户自己的扩展 / 共用目录里另一家的注册)时不动。
pub(super) fn remove_extension_at(
    path: &std::path::Path,
    owns: impl Fn(&str) -> bool,
) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {}", EXTENSION_FILE, e))?;
    if !owns(&content) {
        return Ok(0);
    }
    std::fs::remove_file(path).map_err(|e| format!("删除 {} 失败: {}", EXTENSION_FILE, e))?;
    Ok(1)
}

/// omp 的启动期自愈，见 [`TsExtension::sync_if_registered`]
pub fn sync_omp_hooks_if_registered() {
    OMP_EXTENSION.sync_if_registered();
}

/// pi 的启动期自愈，见 [`TsExtension::sync_if_registered`]
pub fn sync_pi_hooks_if_registered() {
    PI_EXTENSION.sync_if_registered();
}
