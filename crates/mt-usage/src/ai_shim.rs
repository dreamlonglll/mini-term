//! 会话记录读取的最小共用件 —— 从 `src-tauri/src/ai_sessions.rs` 与
//! `src-tauri/src/hook_registry.rs` **逐字复制**的六个纯函数。
//!
//! 为什么不直接依赖 `mt-ai`：这几个函数在旧仓里属于 `ai_sessions` / `hook_registry`，
//! 迁移后归 `mt-ai`。但账本对它们的用法只有「枚举文件 / 认 session_meta 行」这么浅，
//! 而 `mt-usage` 若为此挂上 `mt-ai` 依赖，就会被那边（hook server、状态判定、
//! 会话正文解析）的编译状态绑住。等 `mt-ai` 稳定后把本文件删掉、改成
//! `use mt_ai::{...}` 即可，函数体一字未改，替换是机械动作。
//!
//! ⚠️ 修改前先确认 `mt-ai`（或旧 `ai_sessions.rs`）侧的同名函数是否也要跟着改：
//! 两份实现必须保持一致，否则账本与 AI 历史面板会对同一批文件给出不同结论。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 路径统一化(小写 + 反斜杠,去尾部斜杠),用于 Windows 路径比较
pub(crate) fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// 加载 Codex session_index.jsonl → { id: thread_name }
/// 使用统计全局扫描时复用同一标题映射。
pub(crate) fn load_codex_thread_names(codex_dir: &Path) -> HashMap<String, String> {
    let index_path = codex_dir.join("session_index.jsonl");
    let mut map = HashMap::new();

    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };

    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
            if let (Some(id), Some(name)) = (
                obj.get("id").and_then(|v| v.as_str()),
                obj.get("thread_name").and_then(|v| v.as_str()),
            ) {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }

    map
}

/// 递归遍历 sessions/<year>/<month>/<day>/ 目录,仅收集文件路径。
/// 真正读取 JSONL 前先按路径日期排序和限量,避免历史记录增长后每次刷新都读全量内容。
pub(crate) fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_paths(&path, paths);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
}

/// Codex 会话文件头部 session_meta 行的关键字段。
pub(crate) struct CodexSessionMeta {
    pub(crate) id: String,
    #[allow(dead_code)] // 账本只用 id/cwd；保留字段以与 mt-ai 侧的结构一致
    pub(crate) timestamp: String,
    pub(crate) cwd: String,
}

/// 解析一行,若是 session_meta 则取出 id/timestamp/cwd。行级纯函数。
pub(crate) fn codex_meta_from_line(line: &str) -> Option<CodexSessionMeta> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    Some(CodexSessionMeta {
        id: obj
            .pointer("/payload/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        timestamp: obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cwd: obj
            .pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 从一行 response_item 里提取第一条真实用户输入作为标题候选
/// (跳过 `<...>` 系统注入与 `# AGENTS.md` 前缀)。行级纯函数,本地与远程共用。
pub(crate) fn codex_user_title_from_line(line: &str) -> Option<String> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }
    if obj.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let arr = obj.pointer("/payload/content").and_then(|v| v.as_array())?;
    for item in arr {
        if item.get("type").and_then(|t| t.as_str()) != Some("input_text") {
            continue;
        }
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let trimmed = text.trim_start();
        if !trimmed.is_empty()
            && !trimmed.starts_with('<')
            && !trimmed.starts_with("# AGENTS.md")
        {
            return Some(trimmed.chars().take(100).collect());
        }
    }
    None
}

/// grok 的用户级配置根目录：`$GROK_HOME` 优先，否则 `~/.grok`
/// （与 grok 自身 `grok_home()` 的口径一致）
pub(crate) fn grok_home() -> Option<PathBuf> {
    match std::env::var("GROK_HOME") {
        Ok(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => dirs::home_dir().map(|h| h.join(".grok")),
    }
}
