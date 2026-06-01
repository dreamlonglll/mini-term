//! SSH MCP 按项目注册/卸载模块
//!
//! 提供 Tauri commands 用于为单个项目「启用 / 停用 SSH MCP」。启用时把
//! `mt-ssh-mcp` sidecar 注册成该项目的 MCP server,让运行在该项目终端里的
//! AI agent(Claude Code / Codex)能调用 SSH 工具。
//!
//! 本模块高度对标 `hook_registry.rs`:同样是「读现有配置 → marker 幂等合并 →
//! 写回」,绝不整文件覆盖,以免破坏用户/团队既有的 `.mcp.json` /
//! `.codex/config.toml` 内容。
//!
//! 启用一个项目会写入(全部幂等):
//! - `<project>/.mcp.json` —— Claude Code 的项目级 MCP 注册(`mcpServers` 对象)
//! - `<project>/.codex/config.toml` —— Codex 的项目级 MCP 注册(`[mcp_servers.*]`)
//! - `~/.codex/config.toml` —— 把该项目标为 `trust_level = "trusted"`(否则 Codex
//!   会静默忽略项目级配置);已有 trust 项则保留不动
//! - `~/.claude/settings.json` —— `enabledMcpjsonServers` 加入本 server 名,
//!   让 Claude 免去对该 `.mcp.json` 的一次性审批弹窗
//! - `<project>/.gitignore` —— 追加 `.mcp.json` 与 `.codex/`(含机器相关绝对
//!   路径,不应进版本库)
//!
//! 停用时移除本模块用 marker 写入的 MCP server 条目;但**不**移除 Codex 的
//! 项目信任(无法可靠区分是否本功能所加,且信任无害),也不动 `.gitignore`。

use serde_json::Value;
use std::path::{Path, PathBuf};

/// SSH MCP server 在各配置文件里的固定名字 —— 同时充当幂等 marker。
const MCP_SERVER_NAME: &str = "mini-term-ssh";

// ─── 二进制 / 路径解析 ───

/// 获取 `mt-ssh-mcp` sidecar 二进制的绝对路径(与主程序同目录的兄弟 bin)。
///
/// 与 `hook_registry::get_hook_binary_path` 同套逻辑,二进制名换成 `mt-ssh-mcp`。
fn get_ssh_mcp_binary_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取当前程序路径: {}", e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法获取程序所在目录".to_string())?;

    let bin_name = if cfg!(windows) {
        "mt-ssh-mcp.exe"
    } else {
        "mt-ssh-mcp"
    };

    let bin_path = dir.join(bin_name);
    Ok(bin_path.to_string_lossy().to_string())
}

/// 获取 Codex 全局配置文件路径: `~/.codex/config.toml`
fn codex_global_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// 获取 Claude Code 全局配置文件路径: `~/.claude/settings.json`
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// 校验并规整项目目录路径。
///
/// 要求传入的是一个已存在的目录;返回 `PathBuf`。
fn validate_project_dir(project_dir: &str) -> Result<PathBuf, String> {
    let trimmed = project_dir.trim();
    if trimmed.is_empty() {
        return Err("项目目录路径为空".to_string());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_dir() {
        return Err(format!("项目目录不存在或不是文件夹: {}", trimmed));
    }
    Ok(path)
}

// ─── Claude Code: <project>/.mcp.json ───

/// 构建 `.mcp.json` 里的 stdio MCP server 条目。
///
/// 严格按 Claude Code `.mcp.json` schema:stdio server 用 `type` / `command` /
/// `args` / `env` 字段。`command` 用 `mt-ssh-mcp` 二进制绝对路径(机器相关,
/// 因此也才需要把 `.mcp.json` 加进 `.gitignore`)。
///
/// `args` 携带 `--project-id <id>`,让 sidecar 知道自己属于哪个项目,
/// 从而按该项目的「关联 SSH」范围过滤可见连接。
fn build_mcp_server_entry(binary_path: &str, project_id: &str) -> Value {
    serde_json::json!({
        "type": "stdio",
        "command": binary_path,
        "args": ["--project-id", project_id],
        "env": {}
    })
}

/// 把 `mini-term-ssh` server 幂等写入 `<project>/.mcp.json`。
///
/// 读现有 `.mcp.json`(没有则新建),只在 `mcpServers` 对象里增/改本 server
/// 这一个 key,其它 server 与字段原样保留。
fn write_project_mcp_json(
    project_dir: &Path,
    binary_path: &str,
    project_id: &str,
) -> Result<(), String> {
    let mcp_path = project_dir.join(".mcp.json");

    let mut root: Value = if mcp_path.exists() {
        let content = std::fs::read_to_string(&mcp_path)
            .map_err(|e| format!("读取 .mcp.json 失败: {}", e))?;
        // 空文件视为空对象,避免解析报错
        if content.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&content).map_err(|e| format!("解析 .mcp.json 失败: {}", e))?
        }
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        return Err(".mcp.json 顶层不是 JSON 对象".to_string());
    }
    if root.get("mcpServers").is_none() {
        root["mcpServers"] = serde_json::json!({});
    }
    let servers = root["mcpServers"]
        .as_object_mut()
        .ok_or_else(|| ".mcp.json 的 mcpServers 字段不是对象".to_string())?;

    servers.insert(
        MCP_SERVER_NAME.to_string(),
        build_mcp_server_entry(binary_path, project_id),
    );

    let json_str =
        serde_json::to_string_pretty(&root).map_err(|e| format!("序列化 .mcp.json 失败: {}", e))?;
    crate::fs::atomic_write(&mcp_path, json_str.as_bytes())
        .map_err(|e| format!("写入 .mcp.json 失败: {}", e))?;
    Ok(())
}

/// 从 `<project>/.mcp.json` 幂等移除 `mini-term-ssh` server。
///
/// 只删本 server 的 key;若删完后 `mcpServers` 与文件都为空,则删掉整个
/// `.mcp.json`(避免留下空壳文件)。文件不存在视为已移除。
fn remove_project_mcp_json(project_dir: &Path) -> Result<(), String> {
    let mcp_path = project_dir.join(".mcp.json");
    if !mcp_path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(&mcp_path).map_err(|e| format!("读取 .mcp.json 失败: {}", e))?;
    if content.trim().is_empty() {
        // 空文件直接删
        let _ = std::fs::remove_file(&mcp_path);
        return Ok(());
    }
    let mut root: Value =
        serde_json::from_str(&content).map_err(|e| format!("解析 .mcp.json 失败: {}", e))?;

    if let Some(servers) = root.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
        servers.remove(MCP_SERVER_NAME);
    }

    // 判断文件是否变成「只剩空 mcpServers / 空对象」——是则删文件。
    let is_now_empty = root
        .as_object()
        .map(|obj| {
            obj.iter().all(|(k, v)| {
                k == "mcpServers" && v.as_object().map(|m| m.is_empty()).unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if is_now_empty {
        std::fs::remove_file(&mcp_path).map_err(|e| format!("删除空的 .mcp.json 失败: {}", e))?;
    } else {
        let json_str = serde_json::to_string_pretty(&root)
            .map_err(|e| format!("序列化 .mcp.json 失败: {}", e))?;
        crate::fs::atomic_write(&mcp_path, json_str.as_bytes())
            .map_err(|e| format!("写入 .mcp.json 失败: {}", e))?;
    }
    Ok(())
}

// ─── Codex: <project>/.codex/config.toml ───

/// 把 `[mcp_servers.mini-term-ssh]` 幂等写入 `<project>/.codex/config.toml`。
///
/// 用 `toml_edit` 解析+改写,只动本 server 子表,其它内容原样保留。
/// 写 `command` 与 `args`(`args` 携带 `--project-id`,见 `apply_codex_mcp_server`)。
fn write_project_codex_config(
    project_dir: &Path,
    binary_path: &str,
    project_id: &str,
) -> Result<(), String> {
    let codex_dir = project_dir.join(".codex");
    std::fs::create_dir_all(&codex_dir).map_err(|e| format!("创建项目 .codex 目录失败: {}", e))?;
    let config_path = codex_dir.join("config.toml");

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取项目 config.toml 失败: {}", e))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析项目 config.toml 失败: {}", e))?;

    apply_codex_mcp_server(&mut doc, binary_path, project_id);

    crate::fs::atomic_write(&config_path, doc.to_string().as_bytes())
        .map_err(|e| format!("写入项目 config.toml 失败: {}", e))?;
    Ok(())
}

/// 在 `toml_edit` 文档里设置 `[mcp_servers.mini-term-ssh]` 子表。抽出便于单测。
///
/// `toml_edit::value(path)` 会自动正确转义 Windows 反斜杠路径,无需手动处理。
///
/// 实现要点:`mcp_servers` 设为 implicit table(不写出 `[mcp_servers]` 这一空表
/// 头),`mini-term-ssh` 作为**显式** `Table`(非 inline)写入,这样才会序列化成
/// 研究文件指定的 `[mcp_servers.mini-term-ssh]` 点路径表头形式,而不是 inline
/// table `mini-term-ssh = { command = "..." }`。
///
/// `args` 写成 `["--project-id", "<id>"]`,让 sidecar 知道自己属于哪个项目。
fn apply_codex_mcp_server(doc: &mut toml_edit::DocumentMut, binary_path: &str, project_id: &str) {
    if doc.get("mcp_servers").is_none() {
        let mut parent = toml_edit::Table::new();
        parent.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(parent);
    }

    // 若本 server 子表不存在(或不是标准 table),建一个显式 Table。
    let needs_new_table = doc["mcp_servers"]
        .get(MCP_SERVER_NAME)
        .and_then(|i| i.as_table())
        .is_none();
    if needs_new_table {
        doc["mcp_servers"][MCP_SERVER_NAME] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["mcp_servers"][MCP_SERVER_NAME]["command"] = toml_edit::value(binary_path);

    let mut args = toml_edit::Array::new();
    args.push("--project-id");
    args.push(project_id);
    doc["mcp_servers"][MCP_SERVER_NAME]["args"] = toml_edit::value(args);
}

/// 从 `<project>/.codex/config.toml` 幂等移除 `[mcp_servers.mini-term-ssh]`。
///
/// 只删本 server 子表;文件其它内容保留。文件不存在视为已移除。
fn remove_project_codex_config(project_dir: &Path) -> Result<(), String> {
    let config_path = project_dir.join(".codex").join("config.toml");
    if !config_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("读取项目 config.toml 失败: {}", e))?;
    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析项目 config.toml 失败: {}", e))?;

    strip_codex_mcp_server(&mut doc);

    crate::fs::atomic_write(&config_path, doc.to_string().as_bytes())
        .map_err(|e| format!("写入项目 config.toml 失败: {}", e))?;
    Ok(())
}

/// 在 `toml_edit` 文档里删掉 `[mcp_servers.mini-term-ssh]` 子表。抽出便于单测。
///
/// 若删完后 `mcp_servers` 表为空,连 `[mcp_servers]` 一并移除,不留空表。
fn strip_codex_mcp_server(doc: &mut toml_edit::DocumentMut) {
    if let Some(servers) = doc.get_mut("mcp_servers").and_then(|i| i.as_table_mut()) {
        servers.remove(MCP_SERVER_NAME);
        if servers.is_empty() {
            doc.remove("mcp_servers");
        }
    }
}

// ─── Codex: ~/.codex/config.toml 项目信任 ───

/// 在 Codex 全局 `~/.codex/config.toml` 里把项目标为 `trust_level = "trusted"`。
///
/// Codex 要求项目目录被信任后,其 `<project>/.codex/config.toml` 才生效;
/// 未信任则项目级配置(含 mcp_servers)被静默忽略。
///
/// 幂等:若该项目路径已有 `[projects."..."]` 条目,保留其它字段、只确保
/// `trust_level` 为 `"trusted"`。停用时**不**移除此信任(无法可靠判断是否
/// 本功能所加,且信任本身无害)。
fn trust_project_in_codex(project_dir: &Path) -> Result<(), String> {
    let config_path = codex_global_config_path().ok_or_else(|| "无法获取 home 目录".to_string())?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 Codex config.toml 失败: {}", e))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析 Codex config.toml 失败: {}", e))?;

    // Codex 用项目绝对路径作为 [projects."<path>"] 的 key。
    let key = project_dir.to_string_lossy().to_string();
    apply_codex_project_trust(&mut doc, &key);

    crate::fs::atomic_write(&config_path, doc.to_string().as_bytes())
        .map_err(|e| format!("写入 Codex config.toml 失败: {}", e))?;
    Ok(())
}

/// 在 `toml_edit` 文档里确保 `[projects."<key>"] trust_level = "trusted"`。抽出便于单测。
fn apply_codex_project_trust(doc: &mut toml_edit::DocumentMut, project_key: &str) {
    if doc.get("projects").is_none() {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["projects"] = toml_edit::Item::Table(t);
    }
    doc["projects"][project_key]["trust_level"] = toml_edit::value("trusted");
}

// ─── Claude Code: ~/.claude/settings.json 免审批 ───

/// 在 Claude `~/.claude/settings.json` 的 `enabledMcpjsonServers` 数组里加入
/// 本 server 名,免去对该项目 `.mcp.json` 的一次性审批弹窗。
///
/// 用针对性的 `enabledMcpjsonServers`(只白名单本 server)而非全局
/// `enableAllProjectMcpServers`,避免一刀切信任所有项目的 `.mcp.json`。
///
/// 幂等:已在数组里则不重复添加。停用单个项目时也从数组移除本 server 名
/// (`enabledMcpjsonServers` 是按 server 名而非按项目的白名单)。
fn set_claude_mcp_approval(enable: bool) -> Result<(), String> {
    let settings_path = claude_settings_path().ok_or_else(|| "无法获取 home 目录".to_string())?;
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .claude 目录失败: {}", e))?;
    }

    // 停用且文件不存在 → 无需处理
    if !enable && !settings_path.exists() {
        return Ok(());
    }

    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("读取 Claude settings.json 失败: {}", e))?;
        if content.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&content)
                .map_err(|e| format!("解析 Claude settings.json 失败: {}", e))?
        }
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        return Err("Claude settings.json 顶层不是 JSON 对象".to_string());
    }

    apply_claude_mcp_approval(&mut settings, enable);

    let json_str = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("序列化 Claude settings.json 失败: {}", e))?;
    crate::fs::atomic_write(&settings_path, json_str.as_bytes())
        .map_err(|e| format!("写入 Claude settings.json 失败: {}", e))?;
    Ok(())
}

/// 在 settings JSON 里增/删 `enabledMcpjsonServers` 中的本 server 名。抽出便于单测。
fn apply_claude_mcp_approval(settings: &mut Value, enable: bool) {
    if enable {
        if settings.get("enabledMcpjsonServers").is_none() {
            settings["enabledMcpjsonServers"] = serde_json::json!([]);
        }
        if let Some(arr) = settings["enabledMcpjsonServers"].as_array_mut() {
            let already = arr.iter().any(|v| v.as_str() == Some(MCP_SERVER_NAME));
            if !already {
                arr.push(Value::String(MCP_SERVER_NAME.to_string()));
            }
        }
    } else if let Some(arr) = settings
        .get_mut("enabledMcpjsonServers")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| v.as_str() != Some(MCP_SERVER_NAME));
    }
}

// ─── <project>/.gitignore ───

/// `.gitignore` 里要追加的条目(机器相关绝对路径不该进版本库)。
const GITIGNORE_ENTRIES: &[&str] = &[".mcp.json", ".codex/"];

/// 幂等地把 `.mcp.json` 与 `.codex/` 追加进 `<project>/.gitignore`。
///
/// 逐行检查是否已存在(忽略首尾空白),不存在才追加;文件不存在则新建。
/// 失败不应让整个启用流程失败 —— 由调用方决定如何处理本函数的 Err。
fn append_gitignore_entries(project_dir: &Path) -> Result<(), String> {
    let gitignore_path = project_dir.join(".gitignore");
    let existing = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("读取 .gitignore 失败: {}", e))?
    } else {
        String::new()
    };

    let Some(appended) = compute_gitignore_append(&existing) else {
        // 两个条目都已存在,无需写
        return Ok(());
    };

    crate::fs::atomic_write(&gitignore_path, appended.as_bytes())
        .map_err(|e| format!("写入 .gitignore 失败: {}", e))?;
    Ok(())
}

/// 计算追加条目后的 `.gitignore` 全文;若无需追加返回 `None`。抽出便于单测。
fn compute_gitignore_append(existing: &str) -> Option<String> {
    let present: std::collections::HashSet<&str> = existing.lines().map(|l| l.trim()).collect();
    let missing: Vec<&str> = GITIGNORE_ENTRIES
        .iter()
        .copied()
        .filter(|e| !present.contains(*e))
        .collect();
    if missing.is_empty() {
        return None;
    }

    let mut out = existing.to_string();
    // 确保与已有内容之间有换行分隔
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# mini-term SSH MCP（本机相关配置，勿提交）\n");
    for entry in missing {
        out.push_str(entry);
        out.push('\n');
    }
    Some(out)
}

// ─── Tauri Commands ───

/// 为指定项目启用 SSH MCP。
///
/// `project_dir` 为项目目录绝对路径;`project_id` 为该项目的稳定 id,会写进
/// MCP 注册的 `args`,让 sidecar 按该项目的「关联 SSH」范围过滤可见连接。
/// 成功后该项目的终端里新启动的 Claude Code / Codex 会话即可调用 SSH 工具
/// (已运行的会话需重启)。
#[tauri::command]
pub fn enable_ssh_mcp(project_dir: String, project_id: String) -> Result<String, String> {
    let dir = validate_project_dir(&project_dir)?;
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err("项目 id 为空,无法启用 SSH MCP".to_string());
    }
    let binary_path = get_ssh_mcp_binary_path()?;

    // 核心写入:任何一步失败都直接返回错误(可读中文)。
    write_project_mcp_json(&dir, &binary_path, pid)?;
    write_project_codex_config(&dir, &binary_path, pid)?;
    trust_project_in_codex(&dir)?;
    set_claude_mcp_approval(true)?;

    // .gitignore 追加属于附带优化,失败不致命:记 stderr 并在结果里提示。
    let gitignore_note = match append_gitignore_entries(&dir) {
        Ok(()) => String::new(),
        Err(e) => {
            eprintln!("[ssh_mcp] 追加 .gitignore 失败(不影响启用): {}", e);
            "（.gitignore 未能自动更新，建议手动忽略 .mcp.json 与 .codex/）".to_string()
        }
    };

    Ok(format!(
        "已为该项目启用 SSH MCP：已写入 .mcp.json 与 .codex/config.toml。{}",
        gitignore_note
    ))
}

/// 为指定项目停用 SSH MCP。
///
/// 移除本功能写入的 MCP server 条目;Codex 项目信任与 `.gitignore` 条目保留
/// (无害,且无法可靠区分是否本功能所加)。
#[tauri::command]
pub fn disable_ssh_mcp(project_dir: String) -> Result<String, String> {
    let dir = validate_project_dir(&project_dir)?;

    remove_project_mcp_json(&dir)?;
    remove_project_codex_config(&dir)?;
    set_claude_mcp_approval(false)?;

    Ok("已为该项目停用 SSH MCP：已从 .mcp.json 与 .codex/config.toml 移除相关条目。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── .mcp.json server 条目 ───

    #[test]
    fn build_mcp_server_entry_has_stdio_fields() {
        let entry = build_mcp_server_entry(r"C:\apps\mt-ssh-mcp.exe", "proj-1");
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], r"C:\apps\mt-ssh-mcp.exe");
        assert!(entry["env"].is_object());
        // args 携带 --project-id <id>,让 sidecar 知道自己属于哪个项目
        assert_eq!(entry["args"], serde_json::json!(["--project-id", "proj-1"]));
    }

    // ─── .mcp.json 幂等合并 ───

    fn mcp_json_after_write(initial: &str, binary: &str) -> Value {
        let mut root: Value = if initial.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(initial).unwrap()
        };
        if root.get("mcpServers").is_none() {
            root["mcpServers"] = serde_json::json!({});
        }
        root["mcpServers"].as_object_mut().unwrap().insert(
            MCP_SERVER_NAME.to_string(),
            build_mcp_server_entry(binary, "test-pid"),
        );
        root
    }

    #[test]
    fn mcp_json_write_preserves_existing_servers() {
        let initial = r#"{"mcpServers":{"other":{"command":"x"}}}"#;
        let result = mcp_json_after_write(initial, "/bin/mt-ssh-mcp");
        // 既有 server 保留
        assert_eq!(result["mcpServers"]["other"]["command"], "x");
        // 本 server 加入
        assert_eq!(
            result["mcpServers"][MCP_SERVER_NAME]["command"],
            "/bin/mt-ssh-mcp"
        );
    }

    #[test]
    fn mcp_json_write_on_empty_creates_structure() {
        let result = mcp_json_after_write("", "/bin/mt-ssh-mcp");
        assert!(result["mcpServers"][MCP_SERVER_NAME].is_object());
    }

    #[test]
    fn mcp_json_write_is_idempotent() {
        let once = mcp_json_after_write("", "/bin/mt-ssh-mcp");
        let twice = mcp_json_after_write(&once.to_string(), "/bin/mt-ssh-mcp");
        assert_eq!(once, twice);
        // 只应有一个 server 条目
        assert_eq!(twice["mcpServers"].as_object().unwrap().len(), 1);
    }

    // ─── .mcp.json 移除逻辑 ───

    /// 镜像 remove_project_mcp_json 的纯逻辑:返回 (剩余内容, 是否应删文件)。
    fn mcp_json_after_remove(initial: &str) -> (Value, bool) {
        let mut root: Value = serde_json::from_str(initial).unwrap();
        if let Some(servers) = root.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
            servers.remove(MCP_SERVER_NAME);
        }
        let is_now_empty = root
            .as_object()
            .map(|obj| {
                obj.iter().all(|(k, v)| {
                    k == "mcpServers" && v.as_object().map(|m| m.is_empty()).unwrap_or(false)
                })
            })
            .unwrap_or(false);
        (root, is_now_empty)
    }

    #[test]
    fn mcp_json_remove_keeps_other_servers() {
        let initial = r#"{"mcpServers":{"mini-term-ssh":{"command":"x"},"other":{"command":"y"}}}"#;
        let (root, should_delete) = mcp_json_after_remove(initial);
        assert!(!should_delete);
        assert!(root["mcpServers"].get("mini-term-ssh").is_none());
        assert_eq!(root["mcpServers"]["other"]["command"], "y");
    }

    #[test]
    fn mcp_json_remove_signals_delete_when_only_our_server() {
        let initial = r#"{"mcpServers":{"mini-term-ssh":{"command":"x"}}}"#;
        let (_root, should_delete) = mcp_json_after_remove(initial);
        // 删完后只剩空 mcpServers → 应删整个文件
        assert!(should_delete);
    }

    #[test]
    fn mcp_json_remove_keeps_file_when_other_top_level_keys() {
        let initial = r#"{"mcpServers":{"mini-term-ssh":{"command":"x"}},"someOtherKey":1}"#;
        let (_root, should_delete) = mcp_json_after_remove(initial);
        // 还有别的顶层 key → 不删文件
        assert!(!should_delete);
    }

    // ─── Codex config.toml [mcp_servers] ───

    #[test]
    fn codex_mcp_server_written_under_correct_table() {
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        apply_codex_mcp_server(&mut doc, r"C:\apps\mt-ssh-mcp.exe", "proj-1");
        let text = doc.to_string();
        // toml_edit 对带连字符的 key 会写成 [mcp_servers."mini-term-ssh"];
        // 引号与否都是合法 TOML、Codex 都能解析,这里只校验是 dotted-table 表头
        // (而非 inline table)。
        assert!(text.contains("[mcp_servers."), "got:\n{text}");
        assert!(text.contains("mini-term-ssh"), "got:\n{text}");
        assert!(text.contains("command ="), "got:\n{text}");
        // 关键:reparse 后能读回 command / args,且反斜杠被正确转义、未被破坏。
        let reparsed: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            reparsed["mcp_servers"]["mini-term-ssh"]["command"].as_str(),
            Some(r"C:\apps\mt-ssh-mcp.exe")
        );
        let args: Vec<&str> = reparsed["mcp_servers"]["mini-term-ssh"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(args, ["--project-id", "proj-1"]);
    }

    #[test]
    fn codex_mcp_server_preserves_existing_content() {
        let initial =
            "[mcp_servers.context7]\ncommand = \"npx\"\nargs = [\"@upstash/context7-mcp\"]\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        apply_codex_mcp_server(&mut doc, "/bin/mt-ssh-mcp", "p1");
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        // 既有 server 保留
        assert_eq!(
            reparsed["mcp_servers"]["context7"]["command"].as_str(),
            Some("npx")
        );
        // 本 server 加入
        assert_eq!(
            reparsed["mcp_servers"]["mini-term-ssh"]["command"].as_str(),
            Some("/bin/mt-ssh-mcp")
        );
    }

    #[test]
    fn codex_mcp_server_write_is_idempotent() {
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        apply_codex_mcp_server(&mut doc, "/bin/mt-ssh-mcp", "p1");
        let once = doc.to_string();
        let mut doc2: toml_edit::DocumentMut = once.parse().unwrap();
        apply_codex_mcp_server(&mut doc2, "/bin/mt-ssh-mcp", "p1");
        assert_eq!(once, doc2.to_string());
    }

    #[test]
    fn codex_mcp_server_strip_removes_only_our_server() {
        let initial = "[mcp_servers.context7]\ncommand = \"npx\"\n\n[mcp_servers.mini-term-ssh]\ncommand = \"/bin/mt-ssh-mcp\"\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        strip_codex_mcp_server(&mut doc);
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        assert!(reparsed["mcp_servers"].get("mini-term-ssh").is_none());
        // 别的 server 保留
        assert_eq!(
            reparsed["mcp_servers"]["context7"]["command"].as_str(),
            Some("npx")
        );
    }

    #[test]
    fn codex_mcp_server_strip_removes_empty_table() {
        let initial = "[mcp_servers.mini-term-ssh]\ncommand = \"/bin/mt-ssh-mcp\"\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        strip_codex_mcp_server(&mut doc);
        // 删完后整个 mcp_servers 表应消失
        assert!(doc.get("mcp_servers").is_none());
    }

    #[test]
    fn codex_mcp_server_strip_noop_when_absent() {
        let initial = "[mcp_servers.context7]\ncommand = \"npx\"\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        strip_codex_mcp_server(&mut doc);
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        // 别人的 server 不受影响
        assert_eq!(
            reparsed["mcp_servers"]["context7"]["command"].as_str(),
            Some("npx")
        );
    }

    // ─── Codex 项目信任 ───

    #[test]
    fn codex_project_trust_written_correctly() {
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        apply_codex_project_trust(&mut doc, r"D:\Git\proj");
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(
            reparsed["projects"][r"D:\Git\proj"]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn codex_project_trust_preserves_other_projects() {
        let initial = "[projects.\"/home/u/other\"]\ntrust_level = \"trusted\"\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        apply_codex_project_trust(&mut doc, "/home/u/new");
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        // 旧项目信任保留
        assert_eq!(
            reparsed["projects"]["/home/u/other"]["trust_level"].as_str(),
            Some("trusted")
        );
        // 新项目信任加入
        assert_eq!(
            reparsed["projects"]["/home/u/new"]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn codex_project_trust_preserves_sibling_fields() {
        // 已有项目条目带其它字段时,只动 trust_level,其它字段保留
        let initial = "[projects.\"/home/u/proj\"]\ntrust_level = \"unknown\"\nsome_field = 42\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        apply_codex_project_trust(&mut doc, "/home/u/proj");
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(
            reparsed["projects"]["/home/u/proj"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            reparsed["projects"]["/home/u/proj"]["some_field"].as_integer(),
            Some(42)
        );
    }

    // ─── Claude enabledMcpjsonServers ───

    #[test]
    fn claude_approval_enable_adds_server_name() {
        let mut settings = serde_json::json!({});
        apply_claude_mcp_approval(&mut settings, true);
        let arr = settings["enabledMcpjsonServers"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], MCP_SERVER_NAME);
    }

    #[test]
    fn claude_approval_enable_is_idempotent() {
        let mut settings = serde_json::json!({});
        apply_claude_mcp_approval(&mut settings, true);
        apply_claude_mcp_approval(&mut settings, true);
        assert_eq!(
            settings["enabledMcpjsonServers"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn claude_approval_enable_preserves_other_servers() {
        let mut settings = serde_json::json!({ "enabledMcpjsonServers": ["memory", "github"] });
        apply_claude_mcp_approval(&mut settings, true);
        let arr = settings["enabledMcpjsonServers"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(arr.iter().any(|v| v == "memory"));
        assert!(arr.iter().any(|v| v == "github"));
        assert!(arr.iter().any(|v| v == MCP_SERVER_NAME));
    }

    #[test]
    fn claude_approval_disable_removes_only_our_server() {
        let mut settings =
            serde_json::json!({ "enabledMcpjsonServers": ["memory", MCP_SERVER_NAME, "github"] });
        apply_claude_mcp_approval(&mut settings, false);
        let arr = settings["enabledMcpjsonServers"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(!arr.iter().any(|v| v == MCP_SERVER_NAME));
    }

    #[test]
    fn claude_approval_disable_noop_when_absent() {
        let mut settings = serde_json::json!({ "enabledMcpjsonServers": ["memory"] });
        apply_claude_mcp_approval(&mut settings, false);
        assert_eq!(
            settings["enabledMcpjsonServers"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn claude_approval_preserves_unrelated_settings() {
        let mut settings = serde_json::json!({ "theme": "dark", "hooks": {} });
        apply_claude_mcp_approval(&mut settings, true);
        // 不相关的 key 不受影响
        assert_eq!(settings["theme"], "dark");
        assert!(settings["hooks"].is_object());
    }

    // ─── .gitignore 追加 ───

    #[test]
    fn gitignore_append_on_empty_adds_both_entries() {
        let result = compute_gitignore_append("").unwrap();
        assert!(result.contains(".mcp.json"));
        assert!(result.contains(".codex/"));
    }

    #[test]
    fn gitignore_append_skips_existing_entries() {
        // 两个条目都已存在 → 无需追加
        let existing = "node_modules\n.mcp.json\n.codex/\n";
        assert!(compute_gitignore_append(existing).is_none());
    }

    #[test]
    fn gitignore_append_adds_only_missing_entry() {
        let existing = "node_modules\n.mcp.json\n";
        let result = compute_gitignore_append(existing).unwrap();
        // .codex/ 缺失,应被追加;且原内容保留
        assert!(result.contains("node_modules"));
        assert!(result.contains(".codex/"));
        // .mcp.json 已存在,不应重复(只出现一次)
        assert_eq!(result.matches(".mcp.json").count(), 1);
    }

    #[test]
    fn gitignore_append_inserts_newline_when_missing() {
        // 原文件结尾无换行 → 追加前补一个换行,避免与已有行黏连
        let existing = "node_modules";
        let result = compute_gitignore_append(existing).unwrap();
        assert!(result.starts_with("node_modules\n"));
    }

    #[test]
    fn gitignore_append_handles_whitespace_around_entries() {
        // 已有条目带前后空白也应识别为已存在
        let existing = "  .mcp.json  \n  .codex/  \n";
        assert!(compute_gitignore_append(existing).is_none());
    }

    // ─── validate_project_dir ───

    #[test]
    fn validate_project_dir_rejects_empty() {
        assert!(validate_project_dir("").is_err());
        assert!(validate_project_dir("   ").is_err());
    }

    #[test]
    fn validate_project_dir_rejects_nonexistent() {
        assert!(validate_project_dir("/definitely/not/a/real/dir/xyz123").is_err());
    }

    #[test]
    fn validate_project_dir_accepts_existing_dir() {
        let tmp = std::env::temp_dir();
        assert!(validate_project_dir(&tmp.to_string_lossy()).is_ok());
    }

    // ─── 端到端:启用 → 停用 写真实临时目录 ───

    fn unique_test_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-ssh-mcp-test-{label}-{ts}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn project_files_enable_then_disable_round_trip() {
        let dir = unique_test_dir("roundtrip");
        let binary = "/bin/mt-ssh-mcp";

        // 启用:写 .mcp.json + .codex/config.toml
        write_project_mcp_json(&dir, binary, "p1").unwrap();
        write_project_codex_config(&dir, binary, "p1").unwrap();

        let mcp_path = dir.join(".mcp.json");
        let codex_path = dir.join(".codex").join("config.toml");
        assert!(mcp_path.exists());
        assert!(codex_path.exists());

        // .mcp.json 内容正确,且 args 携带 --project-id
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"][MCP_SERVER_NAME]["command"], binary);
        assert_eq!(
            mcp["mcpServers"][MCP_SERVER_NAME]["args"],
            serde_json::json!(["--project-id", "p1"])
        );

        // .codex/config.toml 内容正确
        let codex: toml_edit::DocumentMut = std::fs::read_to_string(&codex_path)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            codex["mcp_servers"][MCP_SERVER_NAME]["command"].as_str(),
            Some(binary)
        );

        // 停用:移除条目。本功能是唯一 server → .mcp.json 应被删,config.toml 留空表头消失
        remove_project_mcp_json(&dir).unwrap();
        remove_project_codex_config(&dir).unwrap();
        assert!(!mcp_path.exists(), ".mcp.json 应在只剩本 server 时被删除");
        let codex_after: toml_edit::DocumentMut = std::fs::read_to_string(&codex_path)
            .unwrap()
            .parse()
            .unwrap();
        assert!(codex_after.get("mcp_servers").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_mcp_json_disable_preserves_user_server() {
        let dir = unique_test_dir("preserve");
        // 用户已有一个团队共享的 server
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"team-server":{"command":"team"}}}"#,
        )
        .unwrap();

        // 启用再停用,团队 server 必须毫发无损
        write_project_mcp_json(&dir, "/bin/mt-ssh-mcp", "p1").unwrap();
        remove_project_mcp_json(&dir).unwrap();

        let mcp_path = dir.join(".mcp.json");
        assert!(mcp_path.exists(), "文件含别的 server,不应被删");
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["team-server"]["command"], "team");
        assert!(mcp["mcpServers"].get(MCP_SERVER_NAME).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_gitignore_appended_on_enable() {
        let dir = unique_test_dir("gitignore");
        std::fs::write(dir.join(".gitignore"), "node_modules\n").unwrap();

        append_gitignore_entries(&dir).unwrap();

        let content = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(content.contains("node_modules"));
        assert!(content.contains(".mcp.json"));
        assert!(content.contains(".codex/"));

        // 再次调用应幂等(不重复追加)
        append_gitignore_entries(&dir).unwrap();
        let content2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(content2.matches(".mcp.json").count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
