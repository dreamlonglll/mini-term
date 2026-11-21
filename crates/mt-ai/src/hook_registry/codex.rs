use super::*;

// ─── Codex hook 注册/卸载 ───

/// 获取 Codex 事件的超时时间
pub(super) fn codex_event_timeout(event: &str) -> u64 {
    if event == "PermissionRequest" {
        600
    } else {
        30
    }
}

/// 为 Codex 构建单个 hook 条目
///
/// Codex 在 Windows 上使用 PowerShell 执行 hook 命令，
/// 需要用 call operator (`& "path"`) 格式。
pub(super) fn build_codex_hook_entry(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("& \"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!([{
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": codex_event_timeout(event)
        }]
    }])
}

/// 将 Codex config.toml 更新为当前 hooks feature，并迁移旧版键。
///
/// 抽成纯函数，避免测试改写开发者真实的 `~/.codex/config.toml`。
pub(super) fn enable_codex_hooks_feature(content: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析 config.toml 失败: {}", e))?;

    if doc.get("features").is_none() {
        doc["features"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let features = doc["features"]
        .as_table_like_mut()
        .ok_or_else(|| "config.toml 的 features 字段不是表".to_string())?;

    // Codex CLI 0.152.1 起改用 `hooks`；移除旧键，避免新版本报告未知 feature。
    features.remove("codex_hooks");
    features.insert("hooks", toml_edit::value(true));
    Ok(doc.to_string())
}

/// 确保 Codex config.toml 中启用了当前 hooks feature flag。
///
/// 返回是否真的落盘：键已经是现行名字时原样返回，一个字节都不写。启动期自愈
/// （`sync_codex_hooks_feature_if_registered`）走的就是这条路，不能每次启动都
/// 重写一遍用户的 config.toml。
pub(super) fn ensure_codex_hooks_feature() -> Result<bool, String> {
    let config_path = codex_config_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 config.toml 失败: {}", e))?
    } else {
        String::new()
    };
    let updated = enable_codex_hooks_feature(&content)?;
    if updated == content {
        return Ok(false);
    }

    crate::util::atomic_write(&config_path, updated.as_bytes())
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    Ok(true)
}

/// 已注册用户的启动期自愈：把 config.toml 里的 feature 键迁到当前名字。
///
/// config.toml 只有点「注册」时才写，而面板判「已注册」只看 hooks.json——存量
/// 用户升级 mini-term 后面板照旧显示已注册，config.toml 里却还留着废弃的
/// `codex_hooks`，每次开 codex 都吃一条弃用告警（issue #72），且界面上没有任何
/// 线索提示他该去重点一次注册。故只要 hooks.json 里有我们的条目就迁一次。
pub fn sync_codex_hooks_feature_if_registered() {
    if registered_codex_events().is_empty() {
        return;
    }
    match ensure_codex_hooks_feature() {
        Ok(true) => tracing::info!("[hook-registry] codex config.toml 已迁至 features.hooks"),
        Ok(false) => {}
        Err(e) => tracing::warn!("[hook-registry] codex feature 迁移失败: {}", e),
    }
}

/// 注册 Codex hooks 到 ~/.codex/hooks.json
pub(super) fn register_codex_hooks(hook_path: &str) -> Result<String, String> {
    let hooks_path = codex_hooks_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 确保 .codex 目录存在
    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    // 启用 feature flag
    ensure_codex_hooks_feature()?;

    // 读取现有配置
    let mut config: Value = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)
            .map_err(|e| format!("读取 hooks.json 失败: {}", e))?;
        serde_json::from_str(&content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?
    } else {
        serde_json::json!({})
    };

    // 确保 hooks 对象存在
    if config.get("hooks").is_none() {
        config["hooks"] = serde_json::json!({});
    }

    let hooks = config["hooks"]
        .as_object_mut()
        .ok_or_else(|| "hooks 字段不是对象".to_string())?;

    let mut updated = 0;
    let mut added = 0;

    for event in CODEX_HOOK_EVENTS {
        let new_entries = build_codex_hook_entry(hook_path, event);

        if let Some(event_hooks) = hooks.get_mut(*event) {
            if let Some(arr) = event_hooks.as_array_mut() {
                // 查找已有的 miniterm-hook 条目
                // Codex 格式: [ { "hooks": [{ "type": "command", "command": "..." }] } ]
                let existing_idx = arr.iter().position(entry_is_miniterm);

                if let Some(idx) = existing_idx {
                    // 更新：替换整个条目
                    if let Some(new_entry) = new_entries.as_array().and_then(|a| a.first()) {
                        arr[idx] = new_entry.clone();
                        updated += 1;
                    }
                } else {
                    // 追加
                    if let Some(new_arr) = new_entries.as_array() {
                        for entry in new_arr {
                            arr.push(entry.clone());
                        }
                    }
                    added += 1;
                }
            }
        } else {
            // 创建新的事件条目
            hooks.insert(event.to_string(), new_entries);
            added += 1;
        }
    }

    // 写回配置文件
    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::util::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;

    Ok(format!(
        "Codex: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CODEX_HOOK_EVENTS.len()
    ))
}

/// 摘掉 Codex hooks.json 里的 mini-term 条目,返回摘掉的条数。没有我们的条目时
/// 一个字节不写(同 [`remove_claude_entries_at`])。
///
/// `config.toml` 里注册时打开的 `features.hooks` 不动:那是 Codex 的总开关,用户自己
/// 的 hook 也靠它,开着而没有条目是无害的。
pub(super) fn remove_codex_entries_at(path: &std::path::Path) -> Result<usize, String> {
    if registered_events_in(Some(path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("读取 hooks.json 失败: {}", e))?;
    let mut config: Value =
        serde_json::from_str(&content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?;
    let removed = strip_miniterm_entries(&mut config);

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::util::atomic_write(path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;
    Ok(removed)
}

/// 从 ~/.codex/hooks.json 中卸载 miniterm hooks
pub(super) fn unregister_codex_hooks() -> Result<String, String> {
    let removed = match codex_hooks_path() {
        Some(path) => remove_codex_entries_at(&path)?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Codex: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Codex: 已移除 {} 个 hook 条目", removed)
    })
}
