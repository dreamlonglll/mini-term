use super::*;

// ─── Grok Build hook 注册/卸载 ───

/// 为 Grok 构建单个 hook 条目。
///
/// 与另外两家最大的不同：**命令是不带参数的相对文件名**。grok 的 runner 只在
/// 命令文本含空格/管道/`&`/`$` 等元字符时才交给 shell，而 Windows 上它挑的 shell
/// 由环境决定（git-bash / pwsh / powershell / cmd 依次探测），四家的引号与调用
/// 语义互斥——`"C:\path\x.exe" Event` 在 PowerShell 里只是个字符串字面量，
/// `& "…"` 在 bash/cmd 里又是语法错误，写不出一份通用文本。
/// 不含空格的相对路径（相对 hook JSON 所在目录）走的是直接 spawn 分支，
/// 完全绕开 shell；事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递
/// （sidecar 的 `resolve_event_name` 负责 snake_case → PascalCase 还原）。
///
/// 不写 `matcher`：grok 对 `Stop` / `UserPromptSubmit` 上的 matcher 会打警告，
/// 而空 matcher 本就等价于「匹配全部」，省掉即可。
pub(super) fn build_grok_hook_entry() -> Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": hook_binary_name(),
            "timeout": GROK_HOOK_TIMEOUT_SECS
        }]
    })
}

/// 把 hook 二进制复制进 grok hooks 目录。
///
/// 复制失败但旧副本还在 → 视为成功（Windows 上覆盖正在运行的 exe 会失败，
/// 而 hook 进程虽短命也可能恰好在跑；此时留着旧副本远好过让整次注册失败）。
pub(super) fn install_grok_hook_binary(src: &str) -> Result<(), String> {
    let dest = grok_hook_binary_path().ok_or_else(|| "无法获取 grok 目录".to_string())?;
    let src_path = std::path::Path::new(src);
    if !src_path.is_file() {
        return Err(format!("hook 二进制不存在: {}", src));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 hooks 目录失败: {}", e))?;
    }
    match std::fs::copy(src_path, &dest) {
        Ok(_) => Ok(()),
        Err(e) if dest.is_file() => {
            tracing::warn!(
                "[hook-registry] grok hook 二进制覆盖失败(沿用旧副本): {}",
                e
            );
            Ok(())
        }
        Err(e) => Err(format!("复制 hook 二进制失败: {}", e)),
    }
}

/// 注册 Grok hooks 到 {grok_home}/hooks/miniterm.json
pub(super) fn register_grok_hooks(hook_path: &str) -> Result<String, String> {
    let hooks_path = grok_hooks_path().ok_or_else(|| "无法获取 grok 目录".to_string())?;

    install_grok_hook_binary(hook_path)?;

    let mut config: Value = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)
            .map_err(|e| format!("读取 {} 失败: {}", GROK_HOOK_FILE, e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("解析 {} 失败: {}", GROK_HOOK_FILE, e))?
    } else {
        serde_json::json!({})
    };

    if config.get("hooks").is_none() {
        config["hooks"] = serde_json::json!({});
    }
    let hooks = config["hooks"]
        .as_object_mut()
        .ok_or_else(|| "hooks 字段不是对象".to_string())?;

    let mut updated = 0;
    let mut added = 0;

    for event in GROK_HOOK_EVENTS {
        let new_entry = build_grok_hook_entry();
        if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
            match arr.iter().position(entry_is_miniterm) {
                Some(idx) => {
                    arr[idx] = new_entry;
                    updated += 1;
                }
                None => {
                    arr.push(new_entry);
                    added += 1;
                }
            }
        } else {
            hooks.insert(event.to_string(), serde_json::json!([new_entry]));
            added += 1;
        }
    }

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 {} 失败: {}", GROK_HOOK_FILE, e))?;
    crate::util::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;

    Ok(format!(
        "Grok: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        GROK_HOOK_EVENTS.len()
    ))
}

/// 摘掉 {grok_home}/hooks/miniterm.json 里的 mini-term 条目,返回摘掉的条数;
/// 条目清空后连同复制进去的二进制(`bin`)一并删除(那份副本只为本文件服务)。
/// 没有我们的条目时一个字节不动(同 [`remove_claude_entries_at`])。
pub(super) fn remove_grok_entries_at(
    hooks_path: &std::path::Path,
    bin: Option<&std::path::Path>,
) -> Result<usize, String> {
    if registered_events_in(Some(hooks_path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(hooks_path)
        .map_err(|e| format!("读取 {} 失败: {}", GROK_HOOK_FILE, e))?;
    let mut config: Value = serde_json::from_str(&content)
        .map_err(|e| format!("解析 {} 失败: {}", GROK_HOOK_FILE, e))?;
    let removed = strip_miniterm_entries(&mut config);

    let file_now_empty = config
        .get("hooks")
        .and_then(|h| h.as_object())
        .is_none_or(|h| h.is_empty());

    if file_now_empty {
        // 整个文件都是我们的：直接删掉，别在用户的 hooks 目录留下空壳
        // （sidecar 按该文件是否存在决定要不要丢弃 Claude 兼容层的重复投递）
        std::fs::remove_file(hooks_path)
            .map_err(|e| format!("删除 {} 失败: {}", GROK_HOOK_FILE, e))?;
        if let Some(bin) = bin.filter(|b| b.is_file())
            && let Err(e) = std::fs::remove_file(bin)
        {
            tracing::warn!("[hook-registry] 删除 grok hook 二进制副本失败: {}", e);
        }
    } else {
        let json_str = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("序列化 {} 失败: {}", GROK_HOOK_FILE, e))?;
        crate::util::atomic_write(hooks_path, json_str.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;
    }
    Ok(removed)
}

/// 从 {grok_home}/hooks/miniterm.json 中卸载 miniterm hooks
pub(super) fn unregister_grok_hooks() -> Result<String, String> {
    let removed = match grok_hooks_path() {
        Some(path) => remove_grok_entries_at(&path, grok_hook_binary_path().as_deref())?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Grok: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Grok: 已移除 {} 个 hook 条目", removed)
    })
}

pub(super) fn registered_grok_events() -> std::collections::HashSet<String> {
    registered_events_in(grok_hooks_path())
}

pub(super) fn missing_grok_events(
    registered: &std::collections::HashSet<String>,
) -> Vec<&'static str> {
    if registered.is_empty() {
        return Vec::new();
    }
    GROK_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|e| !registered.contains(*e))
        .collect()
}

/// 已注册用户的启动期自愈，两件事：补齐新增事件，以及**刷新二进制副本**。
///
/// 副本是 grok 路线特有的负担：mini-term 升级后应用目录里的 hook 二进制换了新的，
/// 而 `~/.grok/hooks/` 里那份还是旧的。只要注册过就无条件重跑一次幂等的注册，
/// 顺带把副本盖成当前版本（覆盖失败会沿用旧副本，不会让启动流程报错）。
pub fn sync_grok_hooks_if_registered() {
    let registered = registered_grok_events();
    if registered.is_empty() {
        return;
    }
    let hook_path = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "[hook-registry] grok 补注册跳过（拿不到 hook 二进制路径）: {}",
                e
            );
            return;
        }
    };
    let missing = missing_grok_events(&registered);
    match register_grok_hooks(&hook_path) {
        Ok(msg) => tracing::info!(
            "[hook-registry] grok 补注册(缺失事件 {:?}) -> {}",
            missing,
            msg
        ),
        Err(e) => tracing::warn!("[hook-registry] grok 补注册失败: {}", e),
    }
}
