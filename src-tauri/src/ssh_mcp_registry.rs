//! SSH MCP 注册的**读侧清理**模块（存量迁移兜底）
//!
//! 注册职责已由 `ssh_skill_registry`（SKILL.md 生成）接替（ssh-cli-skill spec
//! §5）：启用/停用 SSH 工具不再写任何 MCP 配置,本模块只保留「摘除历史 MCP
//! 注册」的清理函数,供 skill 注册器在启用/停用时自动迁移存量项目调用。
//! `mt-ssh-mcp` sidecar 二进制过渡期继续构建发布(存量项目的 `.mcp.json`
//! 仍指向它);待存量消化后随 spec §10 PR4 一并下线本模块。
//!
//! 清理原则与写入时代一致:只动本 server 用 marker 写入的条目,用户/团队的
//! 其它 server 与配置字段毫发无损;但**不**移除 Codex 的项目信任(无法可靠
//! 区分是否本功能所加,且信任无害),也不动 `.gitignore`。

use serde_json::Value;
use std::path::{Path, PathBuf};

/// SSH MCP server 在各配置文件里的固定名字 —— 同时充当幂等 marker。
const MCP_SERVER_NAME: &str = "mini-term-ssh";

/// 获取 Codex 全局配置文件路径: `~/.codex/config.toml`
fn codex_global_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// 获取 Claude Code 全局配置文件路径: `~/.claude/settings.json`
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

// ─── Claude Code: <project>/.mcp.json ───

/// 从 `<project>/.mcp.json` 幂等移除 `mini-term-ssh` server。
///
/// 只删本 server 的 key;若删完后 `mcpServers` 与文件都为空,则删掉整个
/// `.mcp.json`(避免留下空壳文件)。文件不存在视为已移除。
pub(crate) fn remove_project_mcp_json(project_dir: &Path) -> Result<(), String> {
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

/// 从 `<project>/.codex/config.toml` 幂等移除 `[mcp_servers.mini-term-ssh]`。
///
/// 只删本 server 子表;文件其它内容保留。文件不存在视为已移除。
pub(crate) fn remove_project_codex_config(project_dir: &Path) -> Result<(), String> {
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
/// Codex 要求项目目录被信任后,其 `<project>/.codex/` 内容才生效;未信任则
/// 项目级配置(含 skills)可能被静默忽略,skill 注册器启用时继续写入。
///
/// 幂等:若该项目路径已有 `[projects."..."]` 条目,保留其它字段、只确保
/// `trust_level` 为 `"trusted"`。停用时**不**移除此信任(无法可靠判断是否
/// 本功能所加,且信任本身无害)。
pub(crate) fn trust_project_in_codex(project_dir: &Path) -> Result<(), String> {
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

// ─── Claude Code: ~/.claude/settings.json 免审批白名单 ───

/// 在 Claude `~/.claude/settings.json` 的 `enabledMcpjsonServers` 数组里
/// 增/删本 server 名。skill 时代只用 `enable = false` 路径清理历史白名单;
/// enable 路径保留给单测与潜在回滚。
///
/// 幂等:已在/不在数组里都不重复操作。
pub(crate) fn set_claude_mcp_approval(enable: bool) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ─── 文件级 round-trip ───

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
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
    fn remove_project_mcp_json_deletes_file_when_only_our_server() {
        let dir = unique_test_dir("delete-empty");
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"mini-term-ssh":{"command":"x"}}}"#,
        )
        .unwrap();
        remove_project_mcp_json(&dir).unwrap();
        assert!(!dir.join(".mcp.json").exists(), "只剩本 server 时应删除整个文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_project_mcp_json_preserves_user_server() {
        let dir = unique_test_dir("preserve");
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"mini-term-ssh":{"command":"x"},"team-server":{"command":"team"}}}"#,
        )
        .unwrap();
        remove_project_mcp_json(&dir).unwrap();
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["team-server"]["command"], "team");
        assert!(mcp["mcpServers"].get(MCP_SERVER_NAME).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_project_mcp_json_noop_when_absent() {
        let dir = unique_test_dir("absent");
        remove_project_mcp_json(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_project_codex_config_strips_only_our_table() {
        let dir = unique_test_dir("codex");
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(
            dir.join(".codex").join("config.toml"),
            "[mcp_servers.context7]\ncommand = \"npx\"\n\n[mcp_servers.mini-term-ssh]\ncommand = \"/bin/mt-ssh-mcp\"\n",
        )
        .unwrap();
        remove_project_codex_config(&dir).unwrap();
        let doc: toml_edit::DocumentMut =
            std::fs::read_to_string(dir.join(".codex").join("config.toml"))
                .unwrap()
                .parse()
                .unwrap();
        assert!(doc["mcp_servers"].get("mini-term-ssh").is_none());
        assert_eq!(doc["mcp_servers"]["context7"]["command"].as_str(), Some("npx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── Codex config.toml 纯逻辑 ───

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
        apply_claude_mcp_approval(&mut settings, false);
        // 不相关的 key 不受影响
        assert_eq!(settings["theme"], "dark");
        assert!(settings["hooks"].is_object());
    }
}
