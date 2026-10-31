//! AI agent 的身份与能力表 —— 各家 CLI 的「知识」只住这一处。
//!
//! 此前这些知识以字符串 `match` 的形式散在五个 crate 里:续接命令三套、识别口径
//! 好几种(`== "codex"`、`contains("claude")`、先小写再全等、「其余一律按 Claude」),
//! 彼此已经分叉 —— 历史面板对认不出的 agent 照样拼出 `claude --resume`,分支菜单
//! 却知道 opencode / pi 续不了。现在:
//!
//! - **识别**只有一个入口 [`AgentKind::parse`](大小写、别名、宽松匹配的口径都在这);
//!   会话身份里「agent 缺省按 Claude」的约定走 [`AgentKind::from_session_agent`];
//! - **能力**查 [`AgentKind::spec`],调用方不再写 agent 名字面量;
//! - **命令**由表里的模板经 [`resume_command`] / [`fork_command`] 生成,
//!   拼接前一律过 [`session_id_ok`] 白名单 —— 认不出、不支持、id 可疑都**不产出
//!   命令**(「宁可不续也不敲错」)。
//!
//! 只收拢知识,不改协议:hook 上报的 agent 字段、布局里持久化的 agent 串、
//! 自记账分支边的 agent 串都还是原来的字符串,本模块只负责「认」与「查」。
//!
//! 新接一家 CLI:[`AgentKind`] 加一项、写一行 `AgentSpec`、在 [`AgentKind::spec`]
//! 与 [`AgentKind::ALL`] 里挂上;按 kind 分派的 `match`(会话正文读取、镜像定位)
//! 由编译器或 `_ =>` 分支兜底,逐个看要不要接。

/// 认得出的 AI CLI。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
    /// opencode:只靠输入检测识别,没有 hook、没有可解析的会话记录
    OpenCode,
    /// pi(pi.dev,earendil-works/pi):hook 走进程内 TS 扩展,会话记录与 omp 同形
    Pi,
    /// grok(xai-org/grok-build)
    Grok,
    /// oh-my-pi(pi 的分支):hook 走进程内 TS 扩展
    Omp,
}

/// 会话记录怎么按目录分桶 —— 决定续接 / fork 前要不要先找回会话的启动目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdBucket {
    /// 不按目录分桶:在哪个目录续接都找得到(codex)。
    Global,
    /// 按启动目录分桶,且能按会话 id 反查记录里的真实 cwd
    /// (claude:`~/.claude/projects/<编码 cwd>/<id>.jsonl`,见
    /// [`crate::sessions::lookup_ai_session_cwd`])。起于子目录的会话在项目根
    /// `--resume` 会报 `No conversation found`,必须先反查。
    PerCwdLookup,
    /// 按目录分桶,但没有反查手段:omp 的记录只接了镜像、没接谱系;grok / pi 的历史
    /// 列表只列「项目根这个桶」里的会话,新终端默认目录即正确目录。
    PerCwd,
    /// 没有可解析的会话记录,无从谈起(opencode)。
    Unknown,
}

/// 一家 CLI 的全部知识。字段只收**确有调用方在用**的那些。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSpec {
    pub kind: AgentKind,
    /// 规范标识。同时是:CLI 命令名(输入检测按 basename 全等认它)、会话记录的
    /// `session_type`、设置页 hook 注入的 key。
    pub key: &'static str,
    /// 同一家的其它写法(小写全等)。hook sidecar 按 payload 形状推断时把 Claude
    /// 写成 `claude-code`(`miniterm-hook::detect_agent`),输入检测写 `claude`。
    pub aliases: &'static [&'static str],
    /// 宽松识别:小写后**包含** `key` 也算这一家。镜像定位与「有无会话记录」一直是
    /// 这个口径,收拢时保留;名字短、容易误伤的(`pi` 会命中 `copilot`、`omp` 会命中
    /// `compose`)只认全等。
    pub loose_match: bool,
    /// 展示名(设置页 hook 注入的 tab / 结果行)。
    pub label: &'static str,
    /// 在新 PTY 里恢复(接管)会话的命令模板,`{id}` 由会话 id 替换。
    /// `None` = 不支持续接。
    pub resume: Option<&'static str>,
    /// 在新 PTY 里把会话 fork 成新会话的命令模板。`None` = 没有 CLI 级 fork
    /// (菜单不出分支入口)。
    pub fork: Option<&'static str>,
    pub cwd_bucket: CwdBucket,
    /// 有对话镜像能解析的本机会话记录(见 [`crate::sessions::agent_has_session_log`])。
    /// 为 `false` 的 agent 镜像必须跳过启发式绑定,否则会把同项目别家的最新会话
    /// 贴到该 pane 上(串台)。
    pub session_log: bool,
    /// 会话记录已接入 AI 历史面板(列表 / 正文读取)。用量统计另有自己的解析器,
    /// 只接了 claude / codex / grok(`mt_usage::AgentFilter`),不跟这一位走。
    pub history: bool,
    /// agent 自己会从系统剪贴板取图(`Alt+V`)—— 粘图时不落盘,直接发 `ESC v`。
    /// 官方文档把 `Alt+V` 列为 Windows / WSL 下的「Paste image from clipboard」。
    pub pastes_clipboard_image: bool,
    /// hook 的 `PermissionRequest` 在审批 UI 弹出**前**触发、批准后直接执行工具,
    /// 直到 `PostToolUse` 之前不再有事件 —— 该事件要映射成 ai-working 而不是
    /// ai-idle,否则批准后整段执行期间状态卡在 ai-idle,还会误报「任务完成」。
    pub permission_request_keeps_working: bool,
}

const CLAUDE: AgentSpec = AgentSpec {
    kind: AgentKind::Claude,
    key: "claude",
    aliases: &["claude-code"],
    loose_match: true,
    label: "Claude Code",
    resume: Some("claude --resume {id}"),
    fork: Some("claude --resume {id} --fork-session"),
    cwd_bucket: CwdBucket::PerCwdLookup,
    session_log: true,
    history: true,
    pastes_clipboard_image: true,
    permission_request_keeps_working: false,
};

const CODEX: AgentSpec = AgentSpec {
    kind: AgentKind::Codex,
    key: "codex",
    aliases: &[],
    loose_match: true,
    label: "Codex",
    resume: Some("codex resume {id}"),
    fork: Some("codex fork {id}"),
    cwd_bucket: CwdBucket::Global,
    session_log: true,
    history: true,
    pastes_clipboard_image: false,
    permission_request_keeps_working: true,
};

const OPENCODE: AgentSpec = AgentSpec {
    kind: AgentKind::OpenCode,
    key: "opencode",
    aliases: &[],
    loose_match: false,
    label: "OpenCode",
    // 拿不到会话身份(没有 hook、没有可解析记录),既续不了也 fork 不了
    resume: None,
    fork: None,
    cwd_bucket: CwdBucket::Unknown,
    session_log: false,
    history: false,
    pastes_clipboard_image: false,
    permission_request_keeps_working: false,
};

const PI: AgentSpec = AgentSpec {
    kind: AgentKind::Pi,
    key: "pi",
    aliases: &[],
    loose_match: false,
    label: "pi",
    // 会话 id 来自 hook 上报或历史面板;`--session` 先在当前目录桶里按 id 找,
    // 找到别的项目的同 id 会话会先问要不要 fork 过来,不会静默开错
    resume: Some("pi --session {id}"),
    // pi 有 `--fork <id>`,但分支树要靠谱系扫描画节点,pi 的谱系同 omp 未接
    fork: None,
    cwd_bucket: CwdBucket::PerCwd,
    session_log: true,
    history: true,
    pastes_clipboard_image: false,
    permission_request_keeps_working: false,
};

const GROK: AgentSpec = AgentSpec {
    kind: AgentKind::Grok,
    key: "grok",
    aliases: &[],
    loose_match: true,
    label: "Grok",
    resume: Some("grok --resume {id}"),
    // 无 CLI 级 fork:`--resume` 是**接管**原会话而非复制 → 菜单不出分支入口
    fork: None,
    cwd_bucket: CwdBucket::PerCwd,
    session_log: true,
    history: true,
    pastes_clipboard_image: false,
    permission_request_keeps_working: false,
};

const OMP: AgentSpec = AgentSpec {
    kind: AgentKind::Omp,
    key: "omp",
    aliases: &[],
    loose_match: false,
    label: "oh-my-pi",
    // 会话 id 来自 hook 上报;`--resume` 按 id 前缀或路径查当前目录桶
    resume: Some("omp --resume {id}"),
    // omp 有 `--fork <id>`,但分支树要靠会话记录解析画节点,而 omp 的谱系扫描
    // 尚未接进来 —— 此时开放 fork 只会得到一棵只有自记账边、没有节点的空树。
    // 与 grok 一样只留 resume 位(启动续接走它);等记录解析补上再开 fork。
    fork: None,
    cwd_bucket: CwdBucket::PerCwd,
    session_log: true,
    // AI 历史面板、用量统计与谱系扫描仍未接入(只接了移动镜像)
    history: false,
    pastes_clipboard_image: false,
    permission_request_keeps_working: false,
};

impl AgentKind {
    /// 全部 agent,顺序即输入检测 [`crate::AI_COMMANDS`] 的顺序。
    pub const ALL: [AgentKind; 6] = [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::OpenCode,
        AgentKind::Pi,
        AgentKind::Grok,
        AgentKind::Omp,
    ];

    /// 这一家的知识表。
    pub const fn spec(self) -> &'static AgentSpec {
        match self {
            AgentKind::Claude => &CLAUDE,
            AgentKind::Codex => &CODEX,
            AgentKind::OpenCode => &OPENCODE,
            AgentKind::Pi => &PI,
            AgentKind::Grok => &GROK,
            AgentKind::Omp => &OMP,
        }
    }

    /// 规范标识(= CLI 命令名 = `session_type`)。
    pub const fn key(self) -> &'static str {
        self.spec().key
    }

    /// **从字符串识别 agent 的唯一入口。** 来源不限:hook 上报的 `agent` 字段、
    /// 输入检测的命令名、会话记录的 `session_type`、布局里持久化的会话身份。
    ///
    /// 口径:去首尾空白、ASCII 小写后先按 `key` / 别名全等;不中再按
    /// [`AgentSpec::loose_match`] 做「包含」匹配(表序优先,实际来源不会同时含两家
    /// 名字)。空串与认不出的一律 `None` —— **不**兜底成 Claude,缺省按 Claude 是
    /// 会话身份的约定,归 [`Self::from_session_agent`] 管。
    pub fn parse(raw: &str) -> Option<AgentKind> {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return None;
        }
        Self::ALL
            .into_iter()
            .find(|k| {
                let spec = k.spec();
                spec.key == lower || spec.aliases.contains(&lower.as_str())
            })
            .or_else(|| {
                Self::ALL
                    .into_iter()
                    .find(|k| k.spec().loose_match && lower.contains(k.spec().key))
            })
    }

    /// 会话身份(`AiSessionRef.agent` / hook 上报的会话)里的 agent 字段:
    /// **缺省或空串按 Claude**(老布局只存了 id、hook 早期不带 agent 的约定),
    /// 其余交给 [`Self::parse`]。
    pub fn from_session_agent(agent: Option<&str>) -> Option<AgentKind> {
        match agent.map(str::trim).filter(|a| !a.is_empty()) {
            None => Some(AgentKind::Claude),
            Some(agent) => Self::parse(agent),
        }
    }

    /// 能力位:这一家有没有 CLI 级 fork。与 [`Self::fork_command`] 有意分开 ——
    /// 菜单的「未获会话身份」置灰提示锚在这一位上,那时压根没有 id 可校验。
    pub fn can_fork(self) -> bool {
        self.spec().fork.is_some()
    }

    /// 续接命令;不支持续接或 id 过不了白名单返回 `None`。
    pub fn resume_command(self, session_id: &str) -> Option<String> {
        fill_template(self.spec().resume?, session_id)
    }

    /// fork 命令;没有 fork 能力或 id 过不了白名单返回 `None`。
    pub fn fork_command(self, session_id: &str) -> Option<String> {
        fill_template(self.spec().fork?, session_id)
    }

    /// 续接 / fork 前要不要按 id 反查会话记录里的启动目录(见 [`CwdBucket::PerCwdLookup`])。
    pub fn looks_up_session_cwd(self) -> bool {
        self.spec().cwd_bucket == CwdBucket::PerCwdLookup
    }
}

/// 输入检测认的命令名表(= 各家 [`AgentSpec::key`],顺序同 [`AgentKind::ALL`])。
pub const AGENT_COMMANDS: [&str; AgentKind::ALL.len()] = {
    let mut out = [""; AgentKind::ALL.len()];
    let mut i = 0;
    while i < out.len() {
        out[i] = AgentKind::ALL[i].key();
        i += 1;
    }
    out
};

/// 会话身份 → 续接命令。**续接命令的唯一生成处**:AI 历史面板(恢复 / 复制命令 /
/// 分支树跳转)与启动自动续接都走它。agent 缺省按 Claude;认不出、不支持续接
/// (opencode)、id 可疑都返回 `None`。
pub fn resume_command(agent: Option<&str>, session_id: &str) -> Option<String> {
    AgentKind::from_session_agent(agent)?.resume_command(session_id)
}

/// 会话身份 → fork 命令(pane 右键「分支会话到新分屏」)。口径同 [`resume_command`]。
pub fn fork_command(agent: Option<&str>, session_id: &str) -> Option<String> {
    AgentKind::from_session_agent(agent)?.fork_command(session_id)
}

fn fill_template(template: &str, session_id: &str) -> Option<String> {
    session_id_ok(session_id).then(|| template.replace("{id}", session_id))
}

/// 拼进命令行的会话 id 白名单。
///
/// id 会被原样拼进**写进 PTY 的命令行**,来源(持久化布局、会话记录文件内容、
/// 对本机所有进程开放的 hook 端口)都不是可信输入。口径取此前各套里最严的那份
/// 再收一道:
///
/// - 非空、不超过 128 字节;
/// - 只含 ASCII 字母数字与 `-` `_`(Claude UUID / Codex rollout id / Grok UUIDv7 /
///   omp 十六进制 id 的实际形态)—— 空格、引号、管道、换行等 shell 元字符在此拦截;
/// - **不以 `-` 开头**:否则 `--dangerously-skip-permissions` 这样的「id」会被 CLI
///   当成选项吃掉(参数注入,不是命令注入,上面那条挡不住)。
///
/// 拼文件路径用的是另一道(`sessions::session_id_path_safe`,防 `../` 穿越),
/// 两者目的不同,不合并。
pub fn session_id_ok(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && !session_id.starts_with('-')
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0199a1b2-c3d4-7e8f-9012-3456789abcde";

    /// 表本身的自洽:`spec().kind` 指回自己、key 两两不同、ALL 不漏不重。
    #[test]
    fn 表自洽() {
        for (i, kind) in AgentKind::ALL.into_iter().enumerate() {
            assert_eq!(kind.spec().kind, kind);
            assert_eq!(AGENT_COMMANDS[i], kind.key());
            for other in &AgentKind::ALL[i + 1..] {
                assert_ne!(kind, *other, "ALL 里有重复");
                assert_ne!(kind.key(), other.key(), "key 撞车");
            }
            // key 必须是小写(parse 先小写再比)
            assert_eq!(kind.key(), kind.key().to_ascii_lowercase());
            for alias in kind.spec().aliases {
                assert_eq!(*alias, alias.to_ascii_lowercase());
                assert_eq!(AgentKind::parse(alias), Some(kind));
            }
        }
    }

    /// 输入检测的命令名表与改造前逐字相同(顺序也不变)。
    #[test]
    fn 命令名表不变() {
        assert_eq!(
            AGENT_COMMANDS,
            ["claude", "codex", "opencode", "pi", "grok", "omp"]
        );
    }

    #[test]
    fn 识别_key_别名与大小写() {
        for (raw, kind) in [
            ("claude", AgentKind::Claude),
            ("Claude", AgentKind::Claude),
            ("claude-code", AgentKind::Claude),
            ("CLAUDE-CODE", AgentKind::Claude),
            (" claude ", AgentKind::Claude),
            ("codex", AgentKind::Codex),
            ("CoDeX", AgentKind::Codex),
            ("grok", AgentKind::Grok),
            ("Grok", AgentKind::Grok),
            ("omp", AgentKind::Omp),
            ("OMP", AgentKind::Omp),
            ("opencode", AgentKind::OpenCode),
            ("pi", AgentKind::Pi),
            ("PI", AgentKind::Pi),
        ] {
            assert_eq!(AgentKind::parse(raw), Some(kind), "{raw:?}");
        }
    }

    /// 宽松匹配只给长名字的三家;短名字(pi / omp)与 opencode 只认全等。
    #[test]
    fn 识别_宽松匹配只给长名字() {
        assert_eq!(
            AgentKind::parse("my-claude-wrapper"),
            Some(AgentKind::Claude)
        );
        assert_eq!(AgentKind::parse("codex-cli"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::parse("grok-build"), Some(AgentKind::Grok));
        for raw in [
            "copilot",
            "compose",
            "ompx",
            "pip",
            "opencode-x",
            "gemini",
            "",
            "  ",
            "什么鬼",
        ] {
            assert_eq!(AgentKind::parse(raw), None, "{raw:?} 不该被认出");
        }
    }

    /// 会话身份的约定:缺省 / 空串按 Claude,其余照 parse。
    #[test]
    fn 会话身份缺省按_claude() {
        assert_eq!(AgentKind::from_session_agent(None), Some(AgentKind::Claude));
        assert_eq!(
            AgentKind::from_session_agent(Some("")),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::from_session_agent(Some("codex")),
            Some(AgentKind::Codex)
        );
        assert_eq!(AgentKind::from_session_agent(Some("gemini")), None);
    }

    /// 模板逐字照抄改造前三套实现(`session_panel` / `session_branch` / `store::pure`)。
    #[test]
    fn 命令模板照抄原版() {
        assert_eq!(
            resume_command(Some("claude"), ID),
            Some(format!("claude --resume {ID}"))
        );
        assert_eq!(
            resume_command(Some("claude-code"), ID),
            Some(format!("claude --resume {ID}"))
        );
        assert_eq!(
            resume_command(None, ID),
            Some(format!("claude --resume {ID}"))
        );
        assert_eq!(
            resume_command(Some("codex"), "rollout_9"),
            Some("codex resume rollout_9".to_string())
        );
        assert_eq!(
            resume_command(Some("grok"), "0199-x"),
            Some("grok --resume 0199-x".to_string())
        );
        assert_eq!(
            resume_command(Some("omp"), "1f9d2a6b9c0d1234"),
            Some("omp --resume 1f9d2a6b9c0d1234".to_string())
        );
        assert_eq!(
            resume_command(Some("pi"), ID),
            Some(format!("pi --session {ID}"))
        );
        assert_eq!(fork_command(Some("pi"), ID), None, "pi 谱系未接,不开 fork");
        assert_eq!(
            fork_command(Some("claude-code"), ID),
            Some(format!("claude --resume {ID} --fork-session"))
        );
        assert_eq!(
            fork_command(Some("codex"), ID),
            Some(format!("codex fork {ID}"))
        );
        assert_eq!(fork_command(Some("grok"), ID), None, "grok 无 CLI 级 fork");
        assert_eq!(
            fork_command(Some("omp"), ID),
            None,
            "omp 谱系未接,不开 fork"
        );
    }

    /// 不支持续接的 / 认不出的:一律不产出命令,**不再落到 claude**。
    #[test]
    fn 不支持续接的不产出命令() {
        for agent in ["opencode", "gemini", "什么鬼"] {
            assert_eq!(resume_command(Some(agent), ID), None, "{agent}");
            assert_eq!(fork_command(Some(agent), ID), None, "{agent}");
        }
        assert!(!AgentKind::OpenCode.can_fork());
        assert!(!AgentKind::Pi.can_fork());
        assert!(AgentKind::Claude.can_fork());
        assert!(AgentKind::Codex.can_fork());
    }

    /// 白名单:shell 元字符、超长、选项形态的 id 一律拦下。
    #[test]
    fn 会话_id_白名单() {
        for bad in [
            "",
            "a b",
            "a;rm -rf /",
            "a|b",
            "a&b",
            "a`b`",
            "a$(b)",
            "a\nb",
            "a\"b",
            "a'b",
            "../../etc/passwd",
            "a/b",
            "a\\b",
            // 参数注入
            "--dangerously-skip-permissions",
            "-p",
            "-",
        ] {
            assert!(!session_id_ok(bad), "{bad:?} 该被拦下");
            assert_eq!(resume_command(Some("claude"), bad), None, "{bad:?}");
            assert_eq!(fork_command(Some("claude"), bad), None, "{bad:?}");
        }
        for ok in [ID, "abc_DEF-123", "a", "rollout_9", "a-", "_a"] {
            assert!(session_id_ok(ok), "{ok} 该放行");
        }
        assert!(!session_id_ok(&"a".repeat(129)));
        assert!(session_id_ok(&"a".repeat(128)));
    }

    /// 反查启动目录只对 claude 系开(`~/.claude/projects` 里只有 claude 的桶)。
    #[test]
    fn 只有_claude_反查启动目录() {
        for kind in AgentKind::ALL {
            assert_eq!(
                kind.looks_up_session_cwd(),
                kind == AgentKind::Claude,
                "{kind:?}"
            );
        }
    }

    /// 能力位与 AGENTS.md「有会话记录的是 Claude/Codex/Pi/Grok/OMP」「历史面板接了
    /// Claude/Codex/Pi/Grok」对账。
    #[test]
    fn 能力位对账() {
        let with_log: Vec<_> = AgentKind::ALL
            .into_iter()
            .filter(|k| k.spec().session_log)
            .collect();
        assert_eq!(
            with_log,
            [
                AgentKind::Claude,
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::Grok,
                AgentKind::Omp
            ]
        );
        let with_history: Vec<_> = AgentKind::ALL
            .into_iter()
            .filter(|k| k.spec().history)
            .collect();
        assert_eq!(
            with_history,
            [
                AgentKind::Claude,
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::Grok
            ]
        );
        // 能续接的必有会话记录(否则拿不到 id)
        for kind in AgentKind::ALL {
            if kind.spec().resume.is_some() {
                assert!(kind.spec().session_log, "{kind:?}");
            }
            if kind.can_fork() {
                assert!(kind.spec().resume.is_some(), "{kind:?}");
            }
        }
    }
}
