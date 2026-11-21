use super::*;

// ─── Antigravity CLI(agy)hook 注册/卸载 ───
//
// 全局配置 `~/.gemini/config/hooks.json`(`ANTIGRAVITY_GEMINI_DIR` 实测不改它的位置)。
// 文件是两层:顶层键是**随便起的组名**,值是该组的事件表;事件下挂处理器数组 ——
// `SessionStart` / `PreInvocation` / `PostInvocation` / `Stop` 是平铺的处理器,
// `PreToolUse` / `PostToolUse` 是 `{matcher, hooks: [处理器…]}` 分组。组级
// `"enabled": false` 可整组停用。
//
// - 我们只写自己的组 [`AGY_HOOK_GROUP`];用户机器上常见别家的组(比如指向已删脚本的
//   `orca-status`),别家的组、键序一律原样保留(删键用 `retain`,不用 `Map::remove`
//   —— 后者在 preserve_order 下是 swap_remove,会打乱用户文件的键序)。
// - 命令写法实测(1.5.12):Windows 上 agy 交给 `cmd` 执行,`"<绝对路径>" <事件名>` 带空格、
//   带括号的路径都能跑,外面再包一层引号反而跑不起来 —— 与 Claude 那条同一写法。
//   hook 进程的工作目录是 hooks.json 所在目录,所以必须写绝对路径。
// - sidecar 对 agy 的每个事件都往 stdout 打 `{}`(agy 要求输出 JSON 对象)。这对注册
//   事件集是硬约束:见 [`AGY_HOOK_EVENTS`]。
// - agy 1.5.x 能读带 UTF-8 BOM 的文件,用户手改过的文件可能带 BOM:读的时候剥掉,
//   写的时候不带。

/// mini-term 在 agy `hooks.json` 里的组名
pub(super) const AGY_HOOK_GROUP: &str = "mini-term";

/// agy 注册的事件:**只要这两个**。
///
/// - hook 同步执行、阻塞 agent 循环(官方文档),每多注册一个事件,每次模型调用就多一次
///   `cmd` + 进程冷启动;
/// - `SessionStart` 与首个 `PreInvocation` 同时到(懒触发:发第一条消息才有),不带额外信息;
/// - `PostInvocation` / `PostToolUse` 只覆盖「工具执行完到下一次调模型」那一小段,紧跟着
///   就是下一个 `PreInvocation`,不值得多一次进程启动;
/// - ⚠️ **绝不能注册 `PreToolUse`**:sidecar 对 agy 一律输出 `{}`,而 `PreToolUse` 的
///   `{}`(没有 decision)= **拒绝执行工具**(实测 `tool call denied by pre-tool hook`)。
pub(super) const AGY_HOOK_EVENTS: &[&str] = &["PreInvocation", "Stop"];

/// agy hook 处理器的超时(秒)。sidecar POST 完就退,10s 绰绰有余;缺省是 30s,
/// 而 hook 同步阻塞 agent 循环,万一卡住要尽早放行。
pub(super) const AGY_HOOK_TIMEOUT_SECS: u64 = 10;

/// agy 的全局 hook 配置文件 `~/.gemini/config/hooks.json`
pub(super) fn agy_hooks_path() -> Option<PathBuf> {
    crate::sessions::agy_config_dir().map(|d| d.join("hooks.json"))
}

/// 一个 agy 处理器(平铺形态)。命令写法与 Claude 那条相同,见本段开头。
pub(super) fn build_agy_handler(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("\"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!({
        "type": "command",
        "command": command,
        "timeout": AGY_HOOK_TIMEOUT_SECS
    })
}

/// 我们的组应有的内容(不含 `enabled`):每个事件一个处理器。
pub(super) fn build_agy_group(hook_path: &str) -> serde_json::Map<String, Value> {
    AGY_HOOK_EVENTS
        .iter()
        .map(|event| {
            (
                event.to_string(),
                serde_json::json!([build_agy_handler(hook_path, event)]),
            )
        })
        .collect()
}

/// 解析 agy 的 hooks.json:剥 UTF-8 BOM;空文件按 `{}`;顶层不是对象报错。
pub(super) fn parse_agy_hooks(content: &str) -> Result<Value, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if content.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let config: Value =
        serde_json::from_str(content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?;
    if !config.is_object() {
        return Err("hooks.json 顶层不是对象".to_string());
    }
    Ok(config)
}

/// 读 agy 的 hooks.json;文件不在返回 `Ok(None)`,读不了 / 读不懂返回 `Err`。
pub(super) fn read_agy_hooks(path: &std::path::Path) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => parse_agy_hooks(&content).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取 hooks.json 失败: {}", e)),
    }
}

/// 处理器(平铺形态的一项,或分组里 `hooks` 的一项)的命令带 miniterm-hook 标识。
pub(super) fn agy_handler_is_miniterm(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains(HOOK_MARKER))
}

/// 已注册事件 = **我们的组里**命令带 miniterm-hook 标识的事件(两种形态都认)。
pub(super) fn agy_registered_events(config: &Value) -> std::collections::HashSet<String> {
    let Some(group) = config.get(AGY_HOOK_GROUP).and_then(|g| g.as_object()) else {
        return std::collections::HashSet::new();
    };
    group
        .iter()
        .filter(|(_, items)| {
            items.as_array().is_some_and(|arr| {
                arr.iter()
                    .any(|item| agy_handler_is_miniterm(item) || entry_is_miniterm(item))
            })
        })
        .map(|(event, _)| event.clone())
        .collect()
}

/// 我们的组是否已与当前应写的一致(`enabled` 是用户的开关,不参与比较)。装机目录变了
/// (命令里的绝对路径过期)、事件集变了都算不一致。
pub(super) fn agy_group_is_current(config: &Value, hook_path: &str) -> bool {
    let Some(group) = config.get(AGY_HOOK_GROUP).and_then(|g| g.as_object()) else {
        return false;
    };
    let current: serde_json::Map<String, Value> = group
        .iter()
        .filter(|(key, _)| key.as_str() != "enabled")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    current == build_agy_group(hook_path)
}

/// 把我们的组写成当前应有的内容。组已在 → 原位替换(键序不动),用户在组上设的
/// `enabled` 保留;不在 → 追加到末尾。别家的组一概不碰。
pub(super) fn upsert_agy_group(config: &mut Value, hook_path: &str) -> Result<(), String> {
    let root = config
        .as_object_mut()
        .ok_or_else(|| "hooks.json 顶层不是对象".to_string())?;
    let mut group = serde_json::Map::new();
    if let Some(enabled) = root.get(AGY_HOOK_GROUP).and_then(|g| g.get("enabled")) {
        group.insert("enabled".to_string(), enabled.clone());
    }
    group.extend(build_agy_group(hook_path));
    // 键已在时 insert 原位替换值(preserve_order 下是 IndexMap::insert,不挪位置)
    root.insert(AGY_HOOK_GROUP.to_string(), Value::Object(group));
    Ok(())
}

/// 摘掉**所有组**里的 mini-term 处理器(平铺的看自己的命令,分组的看组里 `hooks`),
/// 被我们摘空的分组项 / 事件 / 组一并删除,返回摘掉的处理器数。
///
/// 只删被**我们**摘空的:用户自己留着的空数组、空组不碰;组里只剩 `enabled` 也算空。
pub(super) fn strip_agy_entries(config: &mut Value) -> usize {
    let Some(root) = config.as_object_mut() else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied_groups: Vec<String> = Vec::new();
    for (group_name, group) in root.iter_mut() {
        let Some(group) = group.as_object_mut() else {
            continue;
        };
        let mut removed_in_group = 0;
        let mut emptied_events: Vec<String> = Vec::new();
        for (event, items) in group.iter_mut() {
            let Some(items) = items.as_array_mut() else {
                continue;
            };
            let mut removed_in_event = 0;
            items.retain_mut(|item| {
                if agy_handler_is_miniterm(item) {
                    removed_in_event += 1;
                    return false;
                }
                if let Some(hooks) = item.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    let before = hooks.len();
                    hooks.retain(|h| !agy_handler_is_miniterm(h));
                    let n = before - hooks.len();
                    removed_in_event += n;
                    if n > 0 && hooks.is_empty() {
                        return false;
                    }
                }
                true
            });
            if removed_in_event > 0 && items.is_empty() {
                emptied_events.push(event.clone());
            }
            removed_in_group += removed_in_event;
        }
        if !emptied_events.is_empty() {
            group.retain(|event, _| !emptied_events.contains(event));
        }
        if removed_in_group > 0 && group.keys().all(|k| k == "enabled") {
            emptied_groups.push(group_name.clone());
        }
        removed += removed_in_group;
    }
    if !emptied_groups.is_empty() {
        root.retain(|name, _| !emptied_groups.contains(name));
    }
    removed
}

pub(super) fn write_agy_hooks(path: &std::path::Path, config: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let json_str = serde_json::to_string_pretty(config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::util::atomic_write(path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))
}

/// 把我们的组写进 `path`,返回是否真的落盘(已是当前内容时一个字节不写)。
///
/// 文件读不懂(坏 JSON / 顶层不是对象)→ 报错,**不覆盖**用户文件。
pub(super) fn register_agy_hooks_at(
    path: &std::path::Path,
    hook_path: &str,
) -> Result<bool, String> {
    let mut config = read_agy_hooks(path)?.unwrap_or_else(|| serde_json::json!({}));
    if agy_group_is_current(&config, hook_path) {
        return Ok(false);
    }
    upsert_agy_group(&mut config, hook_path)?;
    write_agy_hooks(path, &config)?;
    Ok(true)
}

/// 注册 agy hooks 到 ~/.gemini/config/hooks.json
pub(super) fn register_agy_hooks(hook_path: &str) -> Result<String, String> {
    let path = agy_hooks_path().ok_or_else(|| "无法获取 home 目录".to_string())?;
    let label = AgentKind::Agy.spec().label;
    Ok(if register_agy_hooks_at(&path, hook_path)? {
        format!(
            "{}: hook 已写入组 {} (共 {} 个事件)",
            label,
            AGY_HOOK_GROUP,
            AGY_HOOK_EVENTS.len()
        )
    } else {
        format!(
            "{}: hook 已是最新 (共 {} 个事件)",
            label,
            AGY_HOOK_EVENTS.len()
        )
    })
}

/// 摘掉 `path` 里所有 mini-term 处理器,返回摘掉的条数。文件不在 / 读不懂 / 里面没有
/// 我们的条目时**一个字节不写**;摘完只剩 `{}` 也照样留着文件(那是 agy 的配置文件,
/// 不是我们建的专属文件)。
pub(super) fn remove_agy_entries_at(path: &std::path::Path) -> Result<usize, String> {
    let Ok(Some(mut config)) = read_agy_hooks(path) else {
        return Ok(0);
    };
    let removed = strip_agy_entries(&mut config);
    if removed > 0 {
        write_agy_hooks(path, &config)?;
    }
    Ok(removed)
}

/// 从 ~/.gemini/config/hooks.json 中卸载 miniterm hooks
pub(super) fn unregister_agy_hooks() -> Result<String, String> {
    let removed = match agy_hooks_path() {
        Some(path) => remove_agy_entries_at(&path)?,
        None => 0,
    };
    let label = AgentKind::Agy.spec().label;
    Ok(if removed == 0 {
        format!("{}: 未发现 mini-term 的 hook 条目，无需卸载", label)
    } else {
        format!("{}: 已移除 {} 个 hook 条目", label, removed)
    })
}

pub(super) fn registered_agy_events() -> std::collections::HashSet<String> {
    agy_hooks_path()
        .and_then(|path| read_agy_hooks(&path).ok().flatten())
        .map(|config| agy_registered_events(&config))
        .unwrap_or_default()
}

/// 我们的组里登记的 hook 二进制路径(从第一个带标识的处理器命令里抠:Windows 写法
/// `"<路径>" 事件名` 取引号里的,Unix 写法 `<路径> 事件名` 取最后一个空格前的)。
pub(super) fn agy_registered_hook_path(config: &Value) -> Option<String> {
    let group = config.get(AGY_HOOK_GROUP)?.as_object()?;
    let command = group
        .values()
        .filter_map(|items| items.as_array())
        .flatten()
        .filter(|item| agy_handler_is_miniterm(item))
        .find_map(|item| item.get("command").and_then(|c| c.as_str()))?;
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split_once('"')?.0,
        None => command.rsplit_once(' ')?.0,
    };
    Some(path.to_string())
}

/// 启动期自愈的文件层:注册过(我们的组里有 miniterm-hook 条目)且组内容与当前应写的
/// 不一致(缺事件 / 事件集变了 / 命令里的路径过期)时重写我们的组,返回是否落盘。
/// 没注册过、读不懂、已是最新 → 一个字节不写。
///
/// 「路径不一致」只在**登记的那个二进制已经不在了**时才改写:组里其余内容都对、只是指向
/// 另一份**还在**的 miniterm-hook(dev 实例与装机版并跑),说明是另一个 mini-term 注册
/// 的,两边每次启动互相改写只会来回翻 —— 不抢。装机目录变了(旧目录随卸载删掉)、
/// dev 的 target 被删,旧路径都不在了,照样改写。
pub(super) fn sync_agy_hooks_at(path: &std::path::Path, hook_path: &str) -> Result<bool, String> {
    let Ok(Some(config)) = read_agy_hooks(path) else {
        return Ok(false);
    };
    if agy_registered_events(&config).is_empty() || agy_group_is_current(&config, hook_path) {
        return Ok(false);
    }
    let held_by_live_peer = agy_registered_hook_path(&config).is_some_and(|other| {
        other != hook_path
            && std::path::Path::new(&other).is_file()
            && agy_group_is_current(&config, &other)
    });
    if held_by_live_peer {
        return Ok(false);
    }
    register_agy_hooks_at(path, hook_path)
}

/// 已注册用户的启动期自愈:补齐新增事件,以及**命令里的绝对路径过期**时改写 ——
/// agy 的命令写的是 hook 二进制的绝对路径,装机目录一变(换盘重装)或 dev 的 target
/// 被删,旧路径就指向不存在的文件,agy 每个事件都白跑一次失败的 `cmd`、状态全丢。
/// 指向另一份还在的 miniterm-hook 时不抢(口径见 `sync_agy_hooks_at`)。
pub fn sync_agy_hooks_if_registered() {
    let Some(path) = agy_hooks_path() else {
        return;
    };
    let hook_path = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "[hook-registry] agy 补注册跳过（拿不到 hook 二进制路径）: {}",
                e
            );
            return;
        }
    };
    match sync_agy_hooks_at(&path, &hook_path) {
        Ok(true) => tracing::info!(
            "[hook-registry] agy hooks.json 的 {} 组已刷新",
            AGY_HOOK_GROUP
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("[hook-registry] agy 补注册失败: {}", e),
    }
}
