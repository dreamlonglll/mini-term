use super::*;

// ─── Claude Code hook 注册/卸载 ───

/// 为 Claude Code 构建单个 hook 条目
///
/// Claude Code 格式要求: { "matcher": "", "hooks": [{ "type": "command", "command": "..." }] }
pub(super) fn build_claude_hook_entry(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("\"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": command
        }]
    })
}

/// 注册 Claude Code hooks 到 ~/.claude/settings.json
pub(super) fn register_claude_hooks(hook_path: &str) -> Result<String, String> {
    let settings_path =
        claude_settings::settings_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 读改写走共用助手:建 .claude 目录 / 读现有配置 / 原子写回 / 与 ssh_registry
    // 进程内串行,口径见 `claude_settings` 模块注释
    let (added, updated) = claude_settings::update(&settings_path, |settings| {
        // 确保 hooks 对象存在
        if settings.get("hooks").is_none() {
            settings["hooks"] = serde_json::json!({});
        }

        let hooks = settings["hooks"]
            .as_object_mut()
            .ok_or_else(|| "hooks 字段不是对象".to_string())?;

        let mut updated = 0;
        let mut added = 0;

        for event in CLAUDE_HOOK_EVENTS {
            let new_entry = build_claude_hook_entry(hook_path, event);

            if let Some(event_hooks) = hooks.get_mut(*event) {
                if let Some(arr) = event_hooks.as_array_mut() {
                    // 查找已有的 miniterm-hook 条目
                    // Claude Code 格式: [{ "matcher": "", "hooks": [{ "command": "..." }] }]
                    let existing_idx = arr.iter().position(entry_is_miniterm);

                    if let Some(idx) = existing_idx {
                        arr[idx] = new_entry;
                        updated += 1;
                    } else {
                        arr.push(new_entry);
                        added += 1;
                    }
                }
            } else {
                hooks.insert(event.to_string(), serde_json::json!([new_entry]));
                added += 1;
            }
        }
        Ok((added, updated))
    })?;

    Ok(format!(
        "Claude Code: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CLAUDE_HOOK_EVENTS.len()
    ))
}

/// 从同构的 `{ "hooks": { "<Event>": [entry, …] } }` 里摘掉全部 mini-term 条目,
/// 返回摘掉的条数。Claude / Codex / Grok 三家的文件在这一层同构,共用本函数。
///
/// 扫 `hooks` 下**每个**事件键,不只当前事件表:老版本注册过、后来从事件表里下线的
/// 事件也得摘干净(卸载后不该留下任何指向 miniterm-hook 的条目)。只删被**我们**摘空
/// 的事件键 —— 用户自己留着的空数组不碰;删键用 `retain` 保住其余键的顺序
/// (`Map::remove` 在 preserve_order 下是 swap_remove,会打乱用户文件的键序)。
pub(super) fn strip_miniterm_entries(config: &mut Value) -> usize {
    let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied: Vec<String> = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(arr) = entries.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|entry| !entry_is_miniterm(entry));
        let n = before - arr.len();
        if n > 0 && arr.is_empty() {
            emptied.push(event.clone());
        }
        removed += n;
    }
    if !emptied.is_empty() {
        hooks.retain(|event, _| !emptied.contains(event));
    }
    removed
}

/// 摘掉 Claude settings.json 里的 mini-term 条目,返回摘掉的条数。
///
/// 文件不在 / 读不懂 / 里面没有我们的条目时**一个字节不写**(返回 0):settings.json 是
/// 用户的全局配置,读改写一遍哪怕内容不变也会重排格式。
pub(super) fn remove_claude_entries_at(path: &std::path::Path) -> Result<usize, String> {
    if registered_events_in(Some(path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    claude_settings::update(path, |settings| Ok(strip_miniterm_entries(settings)))
}

/// 从 ~/.claude/settings.json 中卸载 miniterm hooks
pub(super) fn unregister_claude_hooks() -> Result<String, String> {
    let removed = match claude_settings::settings_path() {
        Some(path) => remove_claude_entries_at(&path)?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Claude Code: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Claude Code: 已移除 {} 个 hook 条目", removed)
    })
}

/// 某个 hook 配置文件里已写入 miniterm-hook 条目的事件名集合。
///
/// 三家的文件在这一层同构（`{ "hooks": { "<Event>": [entry, …] } }`），共用本函数。
/// 读不到 / 解析失败一律返回空集 —— 空集的语义是「没注册过」，调用方据此不动手，
/// 比冒险改写一个读不懂的配置文件安全。
pub(super) fn registered_events_in(path: Option<PathBuf>) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Some(path) = path else {
        return set;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return set;
    };
    let Ok(config) = serde_json::from_str::<Value>(&content) else {
        return set;
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_object()) else {
        return set;
    };
    for (event, entries) in hooks {
        let ours = entries
            .as_array()
            .is_some_and(|arr| arr.iter().any(entry_is_miniterm));
        if ours {
            set.insert(event.clone());
        }
    }
    set
}

pub(super) fn registered_claude_events() -> std::collections::HashSet<String> {
    registered_events_in(claude_settings::settings_path())
}

pub(super) fn registered_codex_events() -> std::collections::HashSet<String> {
    registered_events_in(codex_hooks_path())
}

/// 需要补注册的事件（`sync_claude_hooks_if_registered` 的纯判定部分，抽出来是
/// 为了可测：另一半要读用户 home 目录下的真实配置）。
///
/// `registered` 为空 = 从未注册过，返回空 —— 不给没开过这功能的用户写配置。
pub(super) fn missing_claude_events(
    registered: &std::collections::HashSet<String>,
) -> Vec<&'static str> {
    if registered.is_empty() {
        return Vec::new();
    }
    CLAUDE_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|e| !registered.contains(*e))
        .collect()
}

/// 给已注册的用户补上新版本新增的 hook 事件。
///
/// `CLAUDE_HOOK_EVENTS` 会随版本增长（v0.10.3 补了 StopFailure 等 5 个），而注册
/// 是设置面板里的一次性手动动作。不补的话，老用户升级后配置里永远是旧事件集，
/// 新增的状态判定对他们完全不生效——而他们没有任何理由知道要再点一次「注册」。
///
/// 只在**已经注册过**（配置里存在 miniterm-hook 条目）时补：从未注册的用户不碰，
/// 那是他们的选择。补齐直接复用幂等的 `register_claude_hooks`。
pub fn sync_claude_hooks_if_registered() {
    let missing = missing_claude_events(&registered_claude_events());
    if missing.is_empty() {
        return;
    }
    let hook_path = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "[hook-registry] 补注册跳过（拿不到 hook 二进制路径）: {}",
                e
            );
            return;
        }
    };
    match register_claude_hooks(&hook_path) {
        Ok(msg) => tracing::info!("[hook-registry] 补注册新增事件 {:?} -> {}", missing, msg),
        Err(e) => tracing::warn!("[hook-registry] 补注册失败: {}", e),
    }
}
