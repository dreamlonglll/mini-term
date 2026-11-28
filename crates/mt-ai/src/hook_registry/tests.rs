use super::*;

/// 状态判定依赖的 Claude 事件必须都在注册列表里：注册列表是白名单，
/// 漏一个就等于该时刻没有任何事件覆盖，状态只能卡到下一个事件。
/// StopFailure 尤其关键——官方文档明确 API 错误结束回合时 `Stop` 不触发。
#[test]
fn claude_registration_covers_status_critical_events() {
    for event in [
        "SessionStart",
        "SessionEnd",
        "Stop",
        "StopFailure",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "PermissionRequest",
        "PermissionDenied",
        "Elicitation",
        "ElicitationResult",
    ] {
        assert!(
            CLAUDE_HOOK_EVENTS.contains(&event),
            "{event} 未注册，该时刻的状态无事件覆盖"
        );
    }
}

/// 老用户（注册于事件列表增长之前）启动时应被补齐，且只补缺的那几个
#[test]
fn stale_registration_is_detected_as_missing() {
    // v0.10.2 及更早的事件集
    let old: std::collections::HashSet<String> = [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "PreCompact",
        "PostCompact",
        "PermissionRequest",
        "Notification",
        "Elicitation",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let missing = missing_claude_events(&old);
    assert!(
        missing.contains(&"StopFailure"),
        "StopFailure 未被识别为缺失"
    );
    assert_eq!(missing.len(), CLAUDE_HOOK_EVENTS.len() - old.len());
}

/// 从未注册过的用户不该被静默写配置；已是最新的不该反复重写
#[test]
fn sync_is_noop_when_never_registered_or_already_current() {
    assert!(missing_claude_events(&std::collections::HashSet::new()).is_empty());

    let current: std::collections::HashSet<String> =
        CLAUDE_HOOK_EVENTS.iter().map(|s| s.to_string()).collect();
    assert!(missing_claude_events(&current).is_empty());
}

/// 事件名重复会在 settings.json 里写出两条相同 hook，AI 每次事件多跑一个进程
#[test]
fn claude_registration_has_no_duplicates() {
    let mut seen = std::collections::HashSet::new();
    for event in CLAUDE_HOOK_EVENTS {
        assert!(seen.insert(*event), "重复注册事件: {event}");
    }
}

/// grok 的状态判定同样依赖这批事件；`Notification` 尤其关键——grok 没有
/// `PermissionRequest`，「等待授权」只能从 Notification 的
/// `permission_prompt` 类型认出来，漏注册就等于没有黄灯。
#[test]
fn grok_registration_covers_status_critical_events() {
    for event in [
        "SessionStart",
        "SessionEnd",
        "Stop",
        "StopFailure",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionDenied",
        "Notification",
    ] {
        assert!(
            GROK_HOOK_EVENTS.contains(&event),
            "{event} 未注册，该时刻的状态无事件覆盖"
        );
    }
}

/// grok 没有这些事件，注册了只会在 `/hooks` 面板里留下无效条目
#[test]
fn grok_registration_omits_events_grok_lacks() {
    for event in [
        "PermissionRequest",
        "PostToolBatch",
        "Elicitation",
        "ElicitationResult",
    ] {
        assert!(
            !GROK_HOOK_EVENTS.contains(&event),
            "{event} 不是 grok 的事件"
        );
    }
}

#[test]
fn grok_registration_has_no_duplicates() {
    let mut seen = std::collections::HashSet::new();
    for event in GROK_HOOK_EVENTS {
        assert!(seen.insert(*event), "重复注册事件: {event}");
    }
}

/// grok 条目的命令**必须**是不含空格的裸文件名：一旦带上空格（绝对路径或
/// 事件名参数），grok 就会把它丢给 shell，而 Windows 上具体是 git-bash /
/// pwsh / powershell / cmd 中的哪一个由环境决定，四家引号语义互斥。
#[test]
fn grok_entry_command_never_reaches_a_shell() {
    let entry = build_grok_hook_entry();
    let command = entry["hooks"][0]["command"].as_str().expect("应有命令");
    assert!(
        command.contains(HOOK_MARKER),
        "条目须可被 entry_is_miniterm 认出"
    );
    for meta in [' ', '|', '&', ';', '>', '<', '$'] {
        assert!(
            !command.contains(meta),
            "命令 {:?} 含 shell 元字符 {:?}，会被 grok 交给 shell 执行",
            command,
            meta
        );
    }
    assert!(!command.starts_with('~'), "前导 ~ 同样会触发 shell 分支");
    assert!(entry.get("matcher").is_none(), "grok 条目不该写 matcher");
    assert!(entry_is_miniterm(&entry));
}

/// 选择性注入的目标解析：显式列表按原样（去重），缺省/空列表回落三家全上
/// —— 「一键注册」在不勾选任何项时仍是全量，语义不因本功能改变。
#[test]
fn targets_default_to_all_and_dedupe_explicit_lists() {
    assert_eq!(resolve_targets(None), HookAgent::ALL.to_vec());
    assert_eq!(resolve_targets(Some(vec![])), HookAgent::ALL.to_vec());
    assert_eq!(
        resolve_targets(Some(vec![HookAgent::Grok])),
        vec![HookAgent::Grok]
    );
    // 重复项会让同一个配置文件被写两遍(幂等但白跑)
    assert_eq!(
        resolve_targets(Some(vec![
            HookAgent::Codex,
            HookAgent::Codex,
            HookAgent::Claude
        ])),
        vec![HookAgent::Codex, HookAgent::Claude]
    );
}

/// 未知 agent 必须在 serde 层被拒——静默退化成「全量注册」会往用户
/// 根本没在用的 CLI 里写配置。
#[test]
fn unknown_agent_is_rejected_at_deserialization() {
    assert!(serde_json::from_str::<HookAgent>("\"grok\"").is_ok());
    assert!(serde_json::from_str::<HookAgent>("\"omp\"").is_ok());
    assert!(serde_json::from_str::<HookAgent>("\"pi\"").is_ok());
    assert!(serde_json::from_str::<HookAgent>("\"agy\"").is_ok());
    assert!(serde_json::from_str::<HookAgent>("\"gemini\"").is_err());
    assert!(serde_json::from_str::<HookAgent>("\"antigravity\"").is_err());
    assert!(serde_json::from_str::<HookAgent>("\"Claude\"").is_err());
}

/// 每家的元信息都得齐：key 唯一、事件集非空、展示名不空——
/// 面板的勾选项与状态徽章全靠它们渲染。
#[test]
fn every_agent_exposes_complete_metadata() {
    let mut keys = std::collections::HashSet::new();
    for &agent in HookAgent::ALL {
        assert!(keys.insert(agent.key()), "key 重复: {}", agent.key());
        assert!(!agent.label().is_empty());
        assert!(!agent.events().is_empty(), "{} 的事件集为空", agent.key());
    }
    assert_eq!(keys.len(), 7);
}

/// key / 展示名改由 agent 表供给后,与改造前逐字相同,且与 serde 名对齐
/// (设置页拿 key 当勾选值、后端拿 serde 名反序列化,两边必须是同一个串)。
#[test]
fn key_label_come_from_agent_table_unchanged() {
    let expected = [
        (HookAgent::Claude, "claude", "Claude Code"),
        (HookAgent::Codex, "codex", "Codex"),
        (HookAgent::Grok, "grok", "Grok"),
        (HookAgent::Omp, "omp", "oh-my-pi"),
        (HookAgent::Pi, "pi", "pi"),
        (HookAgent::Agy, "agy", "Antigravity CLI"),
        (HookAgent::CodeBuddy, "codebuddy", "CodeBuddy"),
    ];
    assert_eq!(expected.len(), HookAgent::ALL.len());
    for (agent, key, label) in expected {
        assert_eq!(agent.key(), key);
        assert_eq!(agent.label(), label);
        assert_eq!(HookAgent::from_key(key), Some(agent));
        assert_eq!(
            serde_json::from_str::<HookAgent>(&format!("\"{key}\"")).ok(),
            Some(agent)
        );
    }
    // 反查只认 key 原文(不走 agent 表的别名/宽松口径):勾选值只可能是 key
    assert_eq!(HookAgent::from_key("claude-code"), None);
    assert_eq!(HookAgent::from_key("Claude"), None);
    assert_eq!(HookAgent::from_key("opencode"), None);
}

// ---- 进程内 TS 扩展（omp / pi） ----

/// 从扩展源码里抠出所有 `pi.on("<event>"` 的事件名。
fn subscribed_events(source: &str) -> std::collections::BTreeSet<String> {
    source
        .match_indices("pi.on(\"")
        .filter_map(|(idx, needle)| {
            let rest = &source[idx + needle.len()..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .collect()
}

/// 模板订阅的事件与声明的事件集必须逐条相等：面板的「N/M」与启动期
/// 对账都按后者算，两边一漂移，要么永远显示「旧版本」，要么漏掉新事件。
#[test]
fn ts_templates_subscribe_exactly_the_declared_events() {
    for ext in [&OMP_EXTENSION, &PI_EXTENSION] {
        let declared: std::collections::BTreeSet<String> =
            ext.events.iter().map(|e| e.to_string()).collect();
        assert_eq!(subscribed_events(ext.source), declared, "{}", ext.tag);
        for shared in [false, true] {
            assert_eq!(
                ext.events_in_source(ext.source, shared).len(),
                ext.events.len(),
                "{} 的当前模板必须被判成「已注册全部事件」",
                ext.tag
            );
        }
        assert_eq!(template_agent(ext.source), Some(ext.tag));
        let mut seen = std::collections::HashSet::new();
        for event in ext.events {
            assert!(seen.insert(*event), "{} 重复订阅事件: {event}", ext.tag);
        }
    }
}

/// 模板上报的事件名都得在 `*_REPORTED_EVENTS` 里（hook_server 按那张表验映射），
/// 且表里每一个都真的出现在模板中——表比模板宽会让 hook_server 的测试守着
/// 不存在的事件。
#[test]
fn ts_templates_report_only_declared_event_names() {
    for (source, reported) in [
        (OMP_EXTENSION_SOURCE, OMP_REPORTED_EVENTS),
        (PI_EXTENSION_SOURCE, PI_REPORTED_EVENTS),
    ] {
        for event in reported {
            assert!(
                source.contains(&format!("\"{}\"", event)),
                "模板里没有上报 {event}"
            );
        }
        // Claude 专有、两家模板都不该冒出来的事件名
        for absent in ["SubagentStart", "SubagentStop", "PostToolBatch"] {
            assert!(
                !source.contains(&format!("\"{}\"", absent)),
                "{absent} 出现在模板里却不在上报表里"
            );
        }
    }
    // pi 没有工具审批与重试事件,模板里不该有对应的上报
    for absent in ["PermissionRequest", "PermissionDenied", "Notification"] {
        assert!(
            !PI_EXTENSION_SOURCE.contains(&format!("\"{}\"", absent)),
            "{absent} 出现在 pi 模板里却不在 PI_REPORTED_EVENTS"
        );
    }
}

/// pi 模板不能是 omp 模板的翻版:omp 的换会话 / 审批 / 重试事件 pi 一个都不发,
/// 照抄就等于那几段状态空着;pi 的回合结束只认 agent_settled,omp 没有它。
#[test]
fn pi_template_follows_pi_lifecycle_not_omps() {
    let pi_events = subscribed_events(PI_EXTENSION_SOURCE);
    for omp_only in [
        "session_switch",
        "session_branch",
        "tool_approval_requested",
        "tool_approval_resolved",
        "auto_retry_start",
        "auto_compaction_start",
        "auto_compaction_end",
    ] {
        assert!(
            !pi_events.contains(omp_only),
            "pi 模板订阅了 omp 专有事件 {omp_only}"
        );
    }
    assert!(pi_events.contains("agent_settled"));
    assert!(!subscribed_events(OMP_EXTENSION_SOURCE).contains("agent_settled"));
    // 手抄进 pi 目录的 omp 模板(issue #85)要被认成「旧版本」,自愈才会换掉它
    let copied = PI_EXTENSION.events_in_source(OMP_EXTENSION_SOURCE, false);
    assert!(!copied.is_empty() && copied.len() < PI_HOOK_EVENTS.len());
}

/// 设了 `PI_CODING_AGENT_DIR`、两家共用一个 `miniterm.ts` 时按模板自报的身份认归属:
/// 互不把对方的文件当成自己的旧版(E2E 实测过的回归——只用 omp 的用户每次启动
/// omp 扩展都被 pi 的自愈换成 pi 模板),面板也不会把没注册的那家显示成「旧版本」。
#[test]
fn shared_file_is_owned_by_the_template_identity() {
    for (ext, other) in [
        (&OMP_EXTENSION, &PI_EXTENSION),
        (&PI_EXTENSION, &OMP_EXTENSION),
    ] {
        assert!(ext.owns(ext.source, true), "{}", ext.tag);
        assert!(
            !ext.owns(other.source, true),
            "{} 不该认 {} 的文件",
            ext.tag,
            other.tag
        );
        assert!(ext.events_in_source(other.source, true).is_empty());
        // 各在各的目录时,另一家的模板只可能是抄错了地方 → 认成本家旧版
        assert!(ext.owns(other.source, false), "{}", ext.tag);
        // 没有身份行的(不是哪一版模板写的,但带标识)按目录认
        let anonymous = format!("// {HOOK_MARKER}\npi.on(\"session_start\", f);");
        assert!(ext.owns(&anonymous, true) && ext.owns(&anonymous, false));
        assert_eq!(ext.other().tag, other.tag);
    }
    assert_eq!(template_agent("export default () => {}"), None);
}

/// 模板的几条硬约束：带标识（注册现状与卸载都按它认）、按 pane/端口环境变量
/// 定位（缺失即空操作）、只让主会话上报（子代理是同进程里的独立会话）、
/// 换会话按 clear 收尾、打断按 aborted 上报（不算完成）。
#[test]
fn omp_template_keeps_its_contract_strings() {
    for needle in [
        HOOK_MARKER,
        "MINITERM_PTY_ID",
        "MINITERM_HOOK_PORT",
        "hook-server.json",
        "/hook",
        "ctx.mode === \"tui\"",
        "reason: \"clear\"",
        "reason: \"aborted\"",
        "willContinue",
        "127.0.0.1",
    ] {
        assert!(OMP_EXTENSION_SOURCE.contains(needle), "模板缺少 {needle:?}");
    }
    // 只许打回环地址，不许出现别的主机
    assert!(!OMP_EXTENSION_SOURCE.contains("https://"));
}

/// pi 模板的硬约束：同 omp 的标识 / 环境变量 / 主会话过滤 / 回环地址;换会话
/// 打 clear 墓碑、/reload 不报、打断按 aborted 上报、对话框只在回合内点黄灯。
#[test]
fn pi_template_keeps_its_contract_strings() {
    for needle in [
        HOOK_MARKER,
        "MINITERM_PTY_ID",
        "MINITERM_HOOK_PORT",
        "hook-server.json",
        "/hook",
        "ctx.mode === \"tui\"",
        "const AGENT = \"pi\"",
        "\"clear\"",
        "reason === \"reload\"",
        "reason: \"aborted\"",
        "event?.aborted === true",
        "!busy",
        "127.0.0.1",
    ] {
        assert!(PI_EXTENSION_SOURCE.contains(needle), "模板缺少 {needle:?}");
    }
    assert!(!PI_EXTENSION_SOURCE.contains("https://"));
}

/// 不带标识的同名文件是用户自己的扩展：既不算「已注册」，也绝不能被覆盖。
#[test]
fn ts_source_without_marker_is_not_ours() {
    let foreign = "export default function (pi) { pi.on(\"session_start\", () => {}); }";
    for ext in [&OMP_EXTENSION, &PI_EXTENSION] {
        for shared in [false, true] {
            assert!(
                ext.events_in_source(foreign, shared).is_empty(),
                "{}",
                ext.tag
            );
        }
        // 老版本模板（带标识但事件不全）识别成「旧版本 N/M」
        let stale = format!(
            "// {HOOK_MARKER}\nconst AGENT = \"{}\";\npi.on(\"session_start\", f); pi.on(\"agent_end\", g);",
            ext.tag
        );
        let got = ext.events_in_source(&stale, true);
        assert_eq!(got.len(), 2, "{}", ext.tag);
        assert!(got.contains("session_start") && got.contains("agent_end"));
    }
}

/// agent 目录的解析口径与 pi 自身一致：`PI_CODING_AGENT_DIR` 压倒一切且认 `~`,
/// 否则 `~/.pi/agent`。
#[test]
fn pi_agent_dir_follows_pi_env_precedence() {
    let home = std::path::Path::new("/home/u");
    assert_eq!(
        pi_agent_dir_from(home, None),
        PathBuf::from("/home/u/.pi/agent")
    );
    assert_eq!(
        pi_agent_dir_from(home, Some("  ")),
        PathBuf::from("/home/u/.pi/agent")
    );
    assert_eq!(
        pi_agent_dir_from(home, Some("/custom/agent")),
        PathBuf::from("/custom/agent")
    );
    assert_eq!(pi_agent_dir_from(home, Some("~")), PathBuf::from("/home/u"));
    assert_eq!(
        pi_agent_dir_from(home, Some("~/dots/pi")),
        PathBuf::from("/home/u/dots/pi")
    );
    // `~user` 不是 home 前缀,原样当路径
    assert_eq!(pi_agent_dir_from(home, Some("~x")), PathBuf::from("~x"));
}

/// agent 目录的解析口径与 omp 自身一致：`PI_CODING_AGENT_DIR` 压倒一切，
/// `PI_CONFIG_DIR` 换根目录名，`PI_PROFILE` 落到 profiles/ 下。
#[test]
fn omp_agent_dir_follows_omp_env_precedence() {
    let home = std::path::Path::new("/home/u");
    assert_eq!(
        omp_agent_dir_from(home, None, None, None),
        PathBuf::from("/home/u/.omp/agent")
    );
    assert_eq!(
        omp_agent_dir_from(home, Some("/custom/agent"), Some(".pi"), Some("work")),
        PathBuf::from("/custom/agent")
    );
    assert_eq!(
        omp_agent_dir_from(home, Some("  "), Some(".pi"), None),
        PathBuf::from("/home/u/.pi/agent")
    );
    assert_eq!(
        omp_agent_dir_from(home, None, None, Some("work")),
        PathBuf::from("/home/u/.omp/profiles/work/agent")
    );
    assert_eq!(
        omp_agent_dir_from(home, None, Some(""), Some(" ")),
        PathBuf::from("/home/u/.omp/agent")
    );
}

/// 与 Claude 同样的自愈语义：从未注册过的用户不写配置，已是最新的不重复补
#[test]
fn grok_sync_is_noop_when_never_registered_or_current() {
    assert!(missing_grok_events(&std::collections::HashSet::new()).is_empty());
    let current: std::collections::HashSet<String> =
        GROK_HOOK_EVENTS.iter().map(|s| s.to_string()).collect();
    assert!(missing_grok_events(&current).is_empty());

    let stale: std::collections::HashSet<String> = ["SessionStart", "Stop"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(missing_grok_events(&stale).contains(&"SessionEnd"));
}

#[test]
fn codex_registration_includes_authoritative_session_end() {
    assert_eq!(
        CODEX_HOOK_EVENTS
            .iter()
            .filter(|event| **event == "SessionEnd")
            .count(),
        1,
        "Codex 必须且只能注册一次 SessionEnd"
    );

    let entry = build_codex_hook_entry("miniterm-hook", "SessionEnd");
    let command = entry[0]["hooks"][0]["command"]
        .as_str()
        .expect("SessionEnd hook 应包含命令");
    assert!(command.contains("miniterm-hook"));
    assert!(command.ends_with(" SessionEnd"));
}

/// Codex CLI 0.152.1 起 feature 名改为 `hooks`。手动片段与一键注册必须
/// 使用同一现行键；继续展示旧键会让用户照抄后无法启动 hooks。
#[test]
fn codex_manual_config_uses_current_hooks_feature() {
    let snippet = get_hook_config_snippet().expect("应生成 hook 配置片段");
    let content = snippet["codex"]["files"][1]["content"]
        .as_str()
        .expect("Codex config.toml 片段应为字符串");

    assert!(content.contains("hooks = true"));
    assert!(
        !content.contains("codex_hooks"),
        "Codex CLI 已不再识别 codex_hooks feature"
    );
}

#[test]
fn codex_feature_config_migrates_legacy_key_and_preserves_user_content() {
    let updated = enable_codex_hooks_feature(
        "# 用户注释\nmodel = \"gpt-5\"\n\n[features]\ncodex_hooks = true\nweb_search = true\n",
    )
    .expect("应迁移 Codex feature");
    let doc = updated
        .parse::<toml_edit::DocumentMut>()
        .expect("迁移结果应是合法 TOML");

    assert_eq!(doc["features"]["hooks"].as_bool(), Some(true));
    assert!(doc["features"].get("codex_hooks").is_none());
    assert_eq!(doc["features"]["web_search"].as_bool(), Some(true));
    assert_eq!(doc["model"].as_str(), Some("gpt-5"));
    assert!(updated.contains("# 用户注释"));
}

/// 键已经是现行名字时必须原样返回——`ensure_codex_hooks_feature` 拿这个
/// 相等判断决定写不写盘，启动期自愈全靠它才不会每次启动重写用户配置。
#[test]
fn codex_feature_config_is_noop_when_already_current() {
    let current = "# 用户注释\n[features]\nhooks = true\nweb_search = true\n";
    assert_eq!(
        enable_codex_hooks_feature(current).expect("现行配置应可解析"),
        current
    );
}

// ---- 卸载摘除(设置页「卸载」与卸载器 `--unregister-hooks` 共用) ----

/// 每个用例一个独立临时目录,不碰真实 home。
fn unique_test_dir(label: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mt-hook-registry-test-{label}-{ts}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn foreign_entry(command: &str) -> Value {
    serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": command }] })
}

/// 摘除扫全部事件键(含已从事件表下线的老事件名),只动我们的条目;
/// 被我们摘空的键删掉,用户原本就空着的数组与其余键序原样保留。
#[test]
fn strip_removes_only_ours_across_all_events_and_keeps_key_order() {
    let ours = |event: &str| build_claude_hook_entry(r"C:\mt\miniterm-hook.exe", event);
    let mut config = serde_json::json!({
        "theme": "dark",
        "hooks": {
            "Stop": [foreign_entry("notify-send done"), ours("Stop")],
            "LegacyEvent": [ours("LegacyEvent")],
            "UserEmpty": [],
            "PreToolUse": [ours("PreToolUse")],
            "Notification": [foreign_entry("say hi")]
        }
    });

    let keys = |config: &Value| -> Vec<String> {
        config["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let before = keys(&config);

    assert_eq!(strip_miniterm_entries(&mut config), 3);
    assert_eq!(
        config,
        serde_json::json!({
            "theme": "dark",
            "hooks": {
                "Stop": [foreign_entry("notify-send done")],
                "UserEmpty": [],
                "Notification": [foreign_entry("say hi")]
            }
        })
    );
    // 剩下的键保持原有相对顺序(serde_json 开了 preserve_order 时,swap_remove
    // 会把末尾的键挪到被删的位置上;没开时 Map 本就按键名排序,这条自然成立)
    let after = keys(&config);
    let expected: Vec<String> = before.into_iter().filter(|k| after.contains(k)).collect();
    assert_eq!(after, expected, "其余键序不能被打乱");

    // 再摘一次:什么都不剩,也什么都不动
    let snapshot = config.clone();
    assert_eq!(strip_miniterm_entries(&mut config), 0);
    assert_eq!(config, snapshot);
}

/// 没有我们条目的文件一个字节都不写(紧凑排版原样保留,不被重排成 pretty);
/// 有的话摘掉我们的、别人的照留。
#[test]
fn removal_leaves_files_without_our_entries_byte_identical() {
    let dir = unique_test_dir("untouched");
    let claude = dir.join("settings.json");
    let codex = dir.join("hooks.json");
    let raw =
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool"}]}]},"model":"x"}"#;
    std::fs::write(&claude, raw).unwrap();
    std::fs::write(&codex, raw).unwrap();

    assert_eq!(remove_claude_entries_at(&claude).unwrap(), 0);
    assert_eq!(remove_codex_entries_at(&codex).unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&claude).unwrap(), raw);
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), raw);

    // 读不懂的文件同样不碰(不冒险改写用户配置)
    std::fs::write(&codex, "{broken").unwrap();
    assert_eq!(remove_codex_entries_at(&codex).unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), "{broken");

    // 文件不在:不建
    let missing = dir.join("absent.json");
    assert_eq!(remove_claude_entries_at(&missing).unwrap(), 0);
    assert!(!missing.exists());

    let mut with_ours: Value = serde_json::from_str(raw).unwrap();
    with_ours["hooks"]["Stop"]
        .as_array_mut()
        .unwrap()
        .push(build_claude_hook_entry(r"C:\mt\miniterm-hook.exe", "Stop"));
    std::fs::write(&claude, with_ours.to_string()).unwrap();
    assert_eq!(remove_claude_entries_at(&claude).unwrap(), 1);
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
    assert_eq!(after, serde_json::from_str::<Value>(raw).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// grok:文件里只剩我们的条目 → 连文件带二进制副本一起删;还有别人的 → 只摘我们的。
#[test]
fn grok_removal_deletes_file_and_binary_only_when_all_ours() {
    let dir = unique_test_dir("grok");
    let hooks = dir.join(GROK_HOOK_FILE);
    let bin = dir.join(hook_binary_name());
    let all_ours = serde_json::json!({
        "hooks": { "Stop": [build_grok_hook_entry()], "SessionEnd": [build_grok_hook_entry()] }
    });
    std::fs::write(&hooks, all_ours.to_string()).unwrap();
    std::fs::write(&bin, b"stub").unwrap();

    assert_eq!(remove_grok_entries_at(&hooks, Some(&bin)).unwrap(), 2);
    assert!(!hooks.exists() && !bin.exists());

    let mixed = serde_json::json!({
        "hooks": { "Stop": [build_grok_hook_entry(), foreign_entry("./mine.sh")] }
    });
    std::fs::write(&hooks, mixed.to_string()).unwrap();
    std::fs::write(&bin, b"stub").unwrap();
    assert_eq!(remove_grok_entries_at(&hooks, Some(&bin)).unwrap(), 1);
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(
        after,
        serde_json::json!({ "hooks": { "Stop": [foreign_entry("./mine.sh")] } })
    );
    assert!(bin.exists(), "文件还在用,二进制副本不能删");
    let _ = std::fs::remove_dir_all(&dir);
}

/// omp / pi:只删本家的扩展文件,用户自己的同名文件、共用目录里另一家的注册都不动。
#[test]
fn ts_extension_removal_only_deletes_owned_file() {
    let dir = unique_test_dir("ts-ext");
    let ext = dir.join(EXTENSION_FILE);
    let pi_owns = |shared: bool| move |s: &str| PI_EXTENSION.owns(s, shared);
    assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 0);

    std::fs::write(&ext, "export default function (pi) {}").unwrap();
    assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 0);
    assert!(ext.exists());

    for source in [OMP_EXTENSION_SOURCE, PI_EXTENSION_SOURCE] {
        std::fs::write(&ext, source).unwrap();
        assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 1);
        assert!(!ext.exists());
    }

    // 共用目录:卸载 pi 不能把 omp 的注册删掉
    std::fs::write(&ext, OMP_EXTENSION_SOURCE).unwrap();
    assert_eq!(remove_extension_at(&ext, pi_owns(true)).unwrap(), 0);
    assert!(ext.exists());
    assert_eq!(
        remove_extension_at(&ext, |s| OMP_EXTENSION.owns(s, true)).unwrap(),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Antigravity CLI(agy) ----

const AGY_HOOK: &str = r"C:\Program Files (x86)\mini term\miniterm-hook.exe";

/// agy 用户机器上真实存在的别家组(指向已删脚本),用来验「别家的组原样保留」。
fn orca_group() -> Value {
    serde_json::json!({
        "PreInvocation": [
            { "type": "command", "command": "node C:/gone/orca-status.js", "timeout": 10 }
        ]
    })
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn top_keys(config: &Value) -> Vec<String> {
    config.as_object().unwrap().keys().cloned().collect()
}

/// 注册集只有 PreInvocation 与 Stop:hook 同步阻塞 agent 循环,越少越好;PreToolUse
/// 绝不能注册 —— sidecar 对 agy 输出 `{}`,在 PreToolUse 上等于拒绝执行工具。
#[test]
fn agy_registration_is_minimal_and_never_gates_tools() {
    assert_eq!(AGY_HOOK_EVENTS, ["PreInvocation", "Stop"]);
    let mut seen = std::collections::HashSet::new();
    for event in AGY_HOOK_EVENTS {
        assert!(seen.insert(*event), "重复注册事件: {event}");
    }
    assert!(!AGY_HOOK_EVENTS.contains(&"PreToolUse"));
    let group = build_agy_group(AGY_HOOK);
    assert!(group.get("PreToolUse").is_none());
    assert_eq!(group.len(), AGY_HOOK_EVENTS.len());
}

/// 处理器形态:平铺、`type: command`、10s 超时、命令是「绝对路径 + 事件名」
/// (Windows 带引号,与 Claude 那条同一写法,实测带空格 / 括号的路径都能跑)。
#[test]
fn agy_handler_shape() {
    let handler = build_agy_handler(AGY_HOOK, "Stop");
    assert_eq!(handler["type"], "command");
    assert_eq!(handler["timeout"], AGY_HOOK_TIMEOUT_SECS);
    assert!(handler.get("hooks").is_none() && handler.get("matcher").is_none());
    let command = handler["command"].as_str().unwrap();
    assert!(command.contains(HOOK_MARKER));
    assert!(command.ends_with(" Stop"));
    if cfg!(windows) {
        assert_eq!(command, format!("\"{AGY_HOOK}\" Stop"));
    }
    assert!(agy_handler_is_miniterm(&handler));
}

/// 注册:别家的组原样、键序不动;我们的组追加在后;再注册一次已是最新,一个字节不写。
#[test]
fn agy_register_keeps_foreign_groups_and_is_idempotent() {
    let dir = unique_test_dir("agy-register");
    let path = dir.join("config").join("hooks.json");
    // 目录不存在也能建
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    assert_eq!(
        read_json(&path),
        serde_json::json!({ AGY_HOOK_GROUP: Value::Object(build_agy_group(AGY_HOOK)) })
    );

    let user = serde_json::json!({ "orca-status": orca_group(), "zz-last": { "enabled": false } });
    std::fs::write(&path, serde_json::to_string_pretty(&user).unwrap()).unwrap();
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    let after = read_json(&path);
    assert_eq!(after["orca-status"], orca_group(), "别家的组原样");
    assert_eq!(after["zz-last"], serde_json::json!({ "enabled": false }));
    let keys = top_keys(&after);
    let foreign: Vec<String> = keys
        .iter()
        .filter(|k| *k != AGY_HOOK_GROUP)
        .cloned()
        .collect();
    let expected: Vec<String> = top_keys(&user)
        .into_iter()
        .filter(|k| foreign.contains(k))
        .collect();
    assert_eq!(foreign, expected, "别家的键序不能被打乱");
    assert_eq!(agy_registered_events(&after).len(), AGY_HOOK_EVENTS.len());

    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !register_agy_hooks_at(&path, AGY_HOOK).unwrap(),
        "已是最新不写"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 我们的组已在(旧路径 / 旧事件集):原位替换,用户在组上设的 `enabled` 保留。
#[test]
fn agy_register_rewrites_our_group_in_place() {
    let mut config = serde_json::json!({
        "a-first": orca_group(),
        AGY_HOOK_GROUP: {
            "enabled": false,
            "SessionStart": [ build_agy_handler(r"D:\old\miniterm-hook.exe", "SessionStart") ]
        },
        "z-last": orca_group()
    });
    let before_keys = top_keys(&config);
    assert!(!agy_group_is_current(&config, AGY_HOOK));
    upsert_agy_group(&mut config, AGY_HOOK).unwrap();
    assert!(agy_group_is_current(&config, AGY_HOOK));
    assert_eq!(
        config[AGY_HOOK_GROUP]["enabled"], false,
        "用户的停用开关保留"
    );
    assert!(
        config[AGY_HOOK_GROUP].get("SessionStart").is_none(),
        "旧事件被换掉"
    );
    // preserve_order 下组原位替换;没开时 Map 按键名排序,两种情况相对顺序都不变
    assert_eq!(top_keys(&config), before_keys);
}

/// 读的时候剥 BOM;坏 JSON / 顶层不是对象报错,且**不覆盖**用户文件。
#[test]
fn agy_register_strips_bom_and_refuses_unreadable_files() {
    let dir = unique_test_dir("agy-bom");
    let path = dir.join("hooks.json");
    let with_bom = format!(
        "\u{feff}{}",
        serde_json::to_string_pretty(&serde_json::json!({ "orca-status": orca_group() })).unwrap()
    );
    std::fs::write(&path, &with_bom).unwrap();
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.starts_with('\u{feff}'), "写出去不带 BOM");
    assert_eq!(read_json(&path)["orca-status"], orca_group());

    // 带 BOM 的已注册文件也认得出(注册现状 / 自愈都靠它)
    std::fs::write(&path, format!("\u{feff}{written}")).unwrap();
    let config = read_agy_hooks(&path).unwrap().unwrap();
    assert_eq!(agy_registered_events(&config).len(), AGY_HOOK_EVENTS.len());

    for broken in ["{broken", "[1, 2]", "\"x\""] {
        std::fs::write(&path, broken).unwrap();
        assert!(register_agy_hooks_at(&path, AGY_HOOK).is_err(), "{broken}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "不覆盖");
    }
    // 空文件按 {} 处理
    std::fs::write(&path, "  \n").unwrap();
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 「已注册」只认我们组里带标识的事件:别家组里出现 miniterm-hook、我们组里只有别人的
/// 命令,都不算。分组形态(`{matcher, hooks}`)的条目也认。
#[test]
fn agy_registered_events_only_count_our_group() {
    let config = serde_json::json!({
        "other": { "Stop": [ build_agy_handler(AGY_HOOK, "Stop") ] },
        AGY_HOOK_GROUP: {
            "enabled": true,
            "PreInvocation": [ build_agy_handler(AGY_HOOK, "PreInvocation") ],
            "Stop": [ { "type": "command", "command": "notify-send done" } ],
            "PostToolUse": [
                { "matcher": "*", "hooks": [ build_agy_handler(AGY_HOOK, "PostToolUse") ] }
            ]
        }
    });
    let mut got: Vec<_> = agy_registered_events(&config).into_iter().collect();
    got.sort();
    assert_eq!(got, ["PostToolUse", "PreInvocation"]);
    assert!(agy_registered_events(&serde_json::json!({})).is_empty());
    assert!(agy_registered_events(&serde_json::json!({ "other": orca_group() })).is_empty());
}

/// 卸载:所有组里的 mini-term 处理器都摘(平铺 / 分组两种形态),被我们摘空的分组项 /
/// 事件 / 组一并删除;别家的条目、用户自己的空数组、键序原样;再卸一次字节不变。
#[test]
fn agy_removal_strips_ours_everywhere_and_keeps_the_rest() {
    let dir = unique_test_dir("agy-remove");
    let path = dir.join("hooks.json");
    let user_handler = serde_json::json!({ "type": "command", "command": "notify-send done" });
    let config = serde_json::json!({
        "orca-status": orca_group(),
        AGY_HOOK_GROUP: Value::Object(build_agy_group(AGY_HOOK)),
        "mixed": {
            "enabled": true,
            "Stop": [ user_handler.clone(), build_agy_handler(AGY_HOOK, "Stop") ],
            "PostToolUse": [
                { "matcher": "*", "hooks": [ build_agy_handler(AGY_HOOK, "PostToolUse") ] },
                { "matcher": "run_command", "hooks": [
                    user_handler.clone(), build_agy_handler(AGY_HOOK, "PostToolUse")
                ] }
            ],
            "UserEmpty": []
        },
        "only-ours": { "enabled": false, "Stop": [ build_agy_handler(AGY_HOOK, "Stop") ] }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    assert_eq!(remove_agy_entries_at(&path).unwrap(), 6);
    let after = read_json(&path);
    assert_eq!(
        after,
        serde_json::json!({
            "orca-status": orca_group(),
            "mixed": {
                "enabled": true,
                "Stop": [ user_handler.clone() ],
                "PostToolUse": [
                    { "matcher": "run_command", "hooks": [ user_handler.clone() ] }
                ],
                "UserEmpty": []
            }
        })
    );
    let expected: Vec<String> = top_keys(&config)
        .into_iter()
        .filter(|k| after.get(k).is_some())
        .collect();
    assert_eq!(top_keys(&after), expected, "其余键序不能被打乱");

    // 再卸一次:没有我们的条目,一个字节不写
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(remove_agy_entries_at(&path).unwrap(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 文件里只有我们的组:卸完留下 `{}`,文件本身不删(那是 agy 的配置文件)。没有我们
/// 条目的、读不懂的、不存在的文件一律不碰。
#[test]
fn agy_removal_leaves_other_files_untouched() {
    let dir = unique_test_dir("agy-remove-edge");
    let path = dir.join("hooks.json");
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    assert_eq!(remove_agy_entries_at(&path).unwrap(), AGY_HOOK_EVENTS.len());
    assert!(path.exists());
    assert_eq!(read_json(&path), serde_json::json!({}));

    let raw = r#"{"orca-status":{"PreInvocation":[{"type":"command","command":"x"}]}}"#;
    std::fs::write(&path, raw).unwrap();
    assert_eq!(remove_agy_entries_at(&path).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        raw,
        "紧凑排版原样保留"
    );

    std::fs::write(&path, "{broken").unwrap();
    assert_eq!(remove_agy_entries_at(&path).unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");

    let missing = dir.join("absent.json");
    assert_eq!(remove_agy_entries_at(&missing).unwrap(), 0);
    assert!(!missing.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 启动期自愈:没注册过 / 已是最新 → 不写;注册过但命令路径过期(装机目录变了)或
/// 缺事件 → 重写我们的组,别家的组不动。
#[test]
fn agy_sync_rewrites_only_stale_registrations() {
    let dir = unique_test_dir("agy-sync");
    let path = dir.join("hooks.json");

    // 文件不在 / 没注册过:不写
    assert!(!sync_agy_hooks_at(&path, AGY_HOOK).unwrap());
    assert!(!path.exists());
    let raw =
        serde_json::to_string_pretty(&serde_json::json!({ "orca-status": orca_group() })).unwrap();
    std::fs::write(&path, &raw).unwrap();
    assert!(!sync_agy_hooks_at(&path, AGY_HOOK).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);

    // 已是最新:不写
    assert!(register_agy_hooks_at(&path, AGY_HOOK).unwrap());
    let bytes = std::fs::read(&path).unwrap();
    assert!(!sync_agy_hooks_at(&path, AGY_HOOK).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // 装机目录变了(登记的旧二进制已经不在):旧路径的组要被刷成新路径
    let moved = r"E:\apps\mini-term\miniterm-hook.exe";
    assert!(!std::path::Path::new(AGY_HOOK).exists());
    assert!(sync_agy_hooks_at(&path, moved).unwrap());
    let after = read_json(&path);
    assert!(agy_group_is_current(&after, moved));
    assert_eq!(after["orca-status"], orca_group());

    // 登记的是另一份**还在**的 miniterm-hook(dev 实例与装机版并跑):不抢,不来回翻
    let live_peer = dir.join(hook_binary_name());
    std::fs::write(&live_peer, b"stub").unwrap();
    let live_peer = live_peer.to_string_lossy().to_string();
    assert!(register_agy_hooks_at(&path, &live_peer).unwrap());
    let bytes = std::fs::read(&path).unwrap();
    assert!(!sync_agy_hooks_at(&path, moved).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    // 但它缺事件时照样补齐(补成当前这份)
    let mut peer_stale = read_json(&path);
    peer_stale[AGY_HOOK_GROUP]
        .as_object_mut()
        .unwrap()
        .retain(|event, _| event == "Stop");
    std::fs::write(&path, serde_json::to_string_pretty(&peer_stale).unwrap()).unwrap();
    assert!(sync_agy_hooks_at(&path, moved).unwrap());
    assert!(agy_group_is_current(&read_json(&path), moved));

    // 老版本只注册了一个事件:补齐
    let mut stale = after.clone();
    stale[AGY_HOOK_GROUP]
        .as_object_mut()
        .unwrap()
        .retain(|event, _| event == "Stop");
    std::fs::write(&path, serde_json::to_string_pretty(&stale).unwrap()).unwrap();
    assert!(sync_agy_hooks_at(&path, moved).unwrap());
    assert_eq!(
        agy_registered_events(&read_json(&path)).len(),
        AGY_HOOK_EVENTS.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 从我们的组里抠出登记的二进制路径:两种命令写法都认,别家组 / 没有标识的不算。
#[test]
fn agy_registered_hook_path_parses_both_command_forms() {
    let config = serde_json::json!({ AGY_HOOK_GROUP: Value::Object(build_agy_group(AGY_HOOK)) });
    assert_eq!(agy_registered_hook_path(&config).as_deref(), Some(AGY_HOOK));
    let unix = serde_json::json!({ AGY_HOOK_GROUP: {
        "Stop": [ { "type": "command", "command": "/opt/mini term/miniterm-hook Stop" } ]
    } });
    assert_eq!(
        agy_registered_hook_path(&unix).as_deref(),
        Some("/opt/mini term/miniterm-hook")
    );
    let foreign = serde_json::json!({
        "other": { "Stop": [ build_agy_handler(AGY_HOOK, "Stop") ] },
        AGY_HOOK_GROUP: { "Stop": [ { "type": "command", "command": "notify-send x" } ] }
    });
    assert_eq!(agy_registered_hook_path(&foreign), None);
}

/// 手动片段与一键注册写的是同一份组内容,且没有 PreToolUse。
#[test]
fn agy_manual_snippet_matches_registration() {
    let snippet = get_hook_config_snippet().expect("应生成 hook 配置片段");
    let file = &snippet["agy"]["files"][0];
    assert_eq!(file["file"], "~/.gemini/config/hooks.json");
    let raw = file["content"].as_str().unwrap();
    let content: Value = serde_json::from_str(raw).unwrap();
    let hook_path = hook_binary_path().unwrap();
    assert!(agy_group_is_current(&content, &hook_path));
    assert_eq!(top_keys(&content), [AGY_HOOK_GROUP]);
    assert!(!raw.contains("PreToolUse"));
}

#[test]
fn codex_feature_config_accepts_inline_features_table() {
    let updated =
        enable_codex_hooks_feature("features = { codex_hooks = true, web_search = true }\n")
            .expect("应迁移内联 features 表");
    let doc = updated
        .parse::<toml_edit::DocumentMut>()
        .expect("迁移结果应是合法 TOML");

    assert_eq!(doc["features"]["hooks"].as_bool(), Some(true));
    assert!(doc["features"].get("codex_hooks").is_none());
    assert_eq!(doc["features"]["web_search"].as_bool(), Some(true));
}
