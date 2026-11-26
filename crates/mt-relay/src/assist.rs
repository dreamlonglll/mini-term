//! 移动端输入辅助（`@` 文件引用补全、`/` 技能补全）与终端应急按键编码。
//!
//! # 安全边界
//!
//! - **只读且有界**：`@` 文件扫描不跟随符号链接、绝不读取文件内容、遍历上限 [`MAX_WALK_ENTRIES`]、
//!   返回相对项目根的 POSIX 路径至多 [`MAX_FILE_RESULTS`] 条；
//! - **只摘名称与简述**：`/` 技能扫描只读每个 `SKILL.md` / 命令 `.md` 的前 [`MAX_SKILL_HEADER_BYTES`]
//!   字节提取 `name` 与一行 `description`（截断至 [`MAX_DESC_CHARS`] 字符），绝不下发技能正文；
//! - **按键白名单**：移动端无法发任意未审控制序列，所有应急按键收敛在 [`TerminalKey`] 闭集内。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use mt_ai::AgentKind;
use mt_relay_protocol::{AssistFailReason, MobileProjectFile, MobileSkillItem, TerminalKey};

/// 单次 `@` 扫描最多检查的文件数（防巨型仓库或未忽略的构建产物目录拖慢响应）。
const MAX_WALK_ENTRIES: usize = 4000;

/// 单次 `@` 补全最多返回的候选条数。
const MAX_FILE_RESULTS: usize = 30;

/// 单个技能文件最多读取的头部字节数（够覆盖 YAML frontmatter 与首段描述）。
const MAX_SKILL_HEADER_BYTES: usize = 4096;

/// 每个技能目录最多扫描的条目数。
const MAX_SKILLS_PER_DIR: usize = 200;

/// 技能描述字符数上限。
const MAX_DESC_CHARS: usize = 120;

/// 移动端应急特殊按键 → 写入 PTY 的精确字节串。
///
/// 方向键与 Home/End 跟随目标终端当前的 `DECCKM`（[`app_cursor`]）模式，
/// 确保全屏 TUI（Claude Code / Codex / Vim 等）里光标移动始终生效。
pub fn encode_terminal_key(key: TerminalKey, app_cursor: bool) -> &'static str {
    match key {
        TerminalKey::Escape => "\x1b",
        TerminalKey::CtrlC => "\x03",
        TerminalKey::CtrlD => "\x04",
        TerminalKey::CtrlU => "\x15",
        TerminalKey::Tab => "\t",
        TerminalKey::ShiftTab => "\x1b[Z",
        TerminalKey::Enter => "\r",
        TerminalKey::Backspace => "\x7f",
        TerminalKey::Space => " ",
        TerminalKey::Up => {
            if app_cursor {
                "\x1bOA"
            } else {
                "\x1b[A"
            }
        }
        TerminalKey::Down => {
            if app_cursor {
                "\x1bOB"
            } else {
                "\x1b[B"
            }
        }
        TerminalKey::Right => {
            if app_cursor {
                "\x1bOC"
            } else {
                "\x1b[C"
            }
        }
        TerminalKey::Left => {
            if app_cursor {
                "\x1bOD"
            } else {
                "\x1b[D"
            }
        }
        TerminalKey::Home => {
            if app_cursor {
                "\x1bOH"
            } else {
                "\x1b[H"
            }
        }
        TerminalKey::End => {
            if app_cursor {
                "\x1bOF"
            } else {
                "\x1b[F"
            }
        }
        TerminalKey::PageUp => "\x1b[5~",
        TerminalKey::PageDown => "\x1b[6~",
    }
}

/// 在本地项目根内按 `query` 检索相对文件路径（供 `@` 文件引用补全）。
///
/// 返回 `(匹配文件列表, 是否触发遍历上限截断)`。
pub fn search_project_files(
    project_root: &Path,
    query: &str,
) -> Result<(Vec<MobileProjectFile>, bool), AssistFailReason> {
    if !project_root.is_dir() {
        return Err(AssistFailReason::ReadFailed);
    }
    let query_norm = query.trim().replace('\\', "/").to_ascii_lowercase();
    let mut visited = 0usize;
    let mut truncated = false;
    let mut scored: Vec<(i32, String)> = Vec::new();

    let mut builder = WalkBuilder::new(project_root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let Some(name) = entry.file_name().to_str() else {
                return true;
            };
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return !matches!(
                    name,
                    ".git" | ".hg" | ".svn" | "node_modules" | "target" | ".next" | "dist"
                );
            }
            true
        });

    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        visited += 1;
        if visited > MAX_WALK_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(project_root) else {
            continue;
        };
        let rel_posix = to_posix_relative(rel);
        if rel_posix.is_empty() {
            continue;
        }
        if query_norm.is_empty() {
            scored.push((0, rel_posix));
        } else if let Some(score) = score_file_match(&rel_posix, &query_norm) {
            scored.push((score, rel_posix));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(MAX_FILE_RESULTS);
    let files = scored
        .into_iter()
        .map(|(_, path)| MobileProjectFile { path })
        .collect();
    Ok((files, truncated))
}

fn to_posix_relative(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 文件路径模糊打分（分值越高越靠前）：文件名精确前缀 > 文件名包含 > 路径前缀 > 路径包含 > 子序列。
fn score_file_match(path: &str, query_lower: &str) -> Option<i32> {
    let path_lower = path.to_ascii_lowercase();
    let file_name = path_lower.rsplit('/').next().unwrap_or(&path_lower);

    if !query_lower.contains('/') {
        if file_name == query_lower {
            return Some(1000);
        }
        if file_name.starts_with(query_lower) {
            return Some(850 - file_name.len().min(200) as i32);
        }
        if let Some(pos) = file_name.find(query_lower) {
            return Some(700 - pos.min(200) as i32);
        }
    }
    if path_lower.starts_with(query_lower) {
        return Some(600 - path_lower.len().min(200) as i32);
    }
    if let Some(pos) = path_lower.find(query_lower) {
        return Some(450 - pos.min(200) as i32);
    }
    // 顺序子序列兜底（如 `mv` 命中 `MirrorView.tsx`）
    let mut q_chars = query_lower.chars();
    let mut target = q_chars.next()?;
    let mut first_idx = None;
    for (idx, ch) in path_lower.chars().enumerate() {
        if ch == target {
            if first_idx.is_none() {
                first_idx = Some(idx as i32);
            }
            let matched_span = idx as i32 - first_idx.unwrap_or(0);
            match q_chars.next() {
                Some(next) => target = next,
                None => return Some(250 - matched_span.min(200)),
            }
        }
    }
    None
}

/// 扫描目标项目与当前 AI agent 可用的本地 Skills / Commands。
///
/// 返回 `(草稿插入前缀, 去重排序后的技能列表)`；Codex 返回 `"$"`，其余返回 `"/"`。
pub fn scan_project_skills(
    project_root: &Path,
    agent: Option<&str>,
) -> (String, Vec<MobileSkillItem>) {
    let kind = agent.and_then(AgentKind::parse);
    let prefix = if kind == Some(AgentKind::Codex) {
        "$"
    } else {
        "/"
    };
    let home = dirs::home_dir();
    let skills = scan_skills_with_home(project_root, home.as_deref(), kind);
    (prefix.to_string(), skills)
}

#[derive(Clone, Copy)]
enum SkillDirLayout {
    /// `<dir>/<skill-name>/SKILL.md`
    SkillDirs,
    /// `<dir>/<command-name>.md`（Claude Code 自定义命令）
    MarkdownCommands,
}

fn scan_skills_with_home(
    project_root: &Path,
    home: Option<&Path>,
    kind: Option<AgentKind>,
) -> Vec<MobileSkillItem> {
    let mut sources: Vec<(PathBuf, SkillDirLayout, &'static str)> = Vec::new();

    match kind {
        Some(AgentKind::Codex) => {
            sources.push((
                project_root.join(".agents").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            sources.push((
                project_root.join(".codex").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            if let Ok(codex_home) = std::env::var("CODEX_HOME")
                && !codex_home.trim().is_empty()
            {
                sources.push((
                    PathBuf::from(codex_home).join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
            }
            if let Some(h) = home {
                sources.push((
                    h.join(".codex").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
                sources.push((
                    h.join(".agents").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
            }
        }
        Some(AgentKind::Agy) => {
            sources.push((
                project_root.join(".agents").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            sources.push((
                project_root.join(".claude").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            if let Some(h) = home {
                let agy_root = h.join(".gemini").join("antigravity-cli");
                sources.push((agy_root.join("skills"), SkillDirLayout::SkillDirs, "user"));
                sources.push((
                    agy_root.join("builtin").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
                sources.push((
                    h.join(".agents").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
            }
        }
        Some(AgentKind::Pi) | Some(AgentKind::Omp) => {
            sources.push((
                project_root.join(".pi").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            sources.push((
                project_root.join(".omp").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            sources.push((
                project_root.join(".agents").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            if let Some(h) = home {
                sources.push((
                    h.join(".pi").join("agent").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
                sources.push((
                    h.join(".omp").join("agent").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
                sources.push((
                    h.join(".agents").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
            }
        }
        _ => {
            // Claude Code 及未明确识别的 agent：覆盖项目与用户级 skills / commands
            sources.push((
                project_root.join(".claude").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            sources.push((
                project_root.join(".claude").join("commands"),
                SkillDirLayout::MarkdownCommands,
                "project",
            ));
            sources.push((
                project_root.join(".agents").join("skills"),
                SkillDirLayout::SkillDirs,
                "project",
            ));
            if let Some(h) = home {
                sources.push((
                    h.join(".claude").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
                sources.push((
                    h.join(".claude").join("commands"),
                    SkillDirLayout::MarkdownCommands,
                    "user",
                ));
                sources.push((
                    h.join(".agents").join("skills"),
                    SkillDirLayout::SkillDirs,
                    "user",
                ));
            }
        }
    }

    // 项目级先扫描，同名去重时项目级优先覆盖用户级；目录也先去重防同一根重复扫
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut by_name: HashMap<String, MobileSkillItem> = HashMap::new();

    for (dir, layout, source_tag) in sources {
        if !dir.is_dir() {
            continue;
        }
        let key = mt_core::path::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen_dirs.insert(key) {
            continue;
        }
        collect_skills_from_dir(&dir, layout, source_tag, &mut by_name);
    }

    let mut list: Vec<MobileSkillItem> = by_name.into_values().collect();
    list.sort_by(|a, b| {
        let rank = |s: &str| if s == "project" { 0u8 } else { 1u8 };
        rank(&a.source).cmp(&rank(&b.source)).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });
    list
}

fn collect_skills_from_dir(
    dir: &Path,
    layout: SkillDirLayout,
    source_tag: &'static str,
    out: &mut HashMap<String, MobileSkillItem>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut scanned = 0usize;
    for entry in entries.flatten() {
        scanned += 1;
        if scanned > MAX_SKILLS_PER_DIR {
            break;
        }
        let path = entry.path();
        match layout {
            SkillDirLayout::SkillDirs => {
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                let fallback_name = entry.file_name().to_string_lossy().into_owned();
                if let Some(item) = parse_skill_file(&skill_md, &fallback_name, source_tag) {
                    out.entry(item.name.to_ascii_lowercase()).or_insert(item);
                }
            }
            SkillDirLayout::MarkdownCommands => {
                if !path.is_file() {
                    continue;
                }
                let ext_ok = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("md"));
                if !ext_ok {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Some(item) = parse_skill_file(&path, stem, source_tag) {
                    out.entry(item.name.to_ascii_lowercase()).or_insert(item);
                }
            }
        }
    }
}

fn parse_skill_file(
    path: &Path,
    fallback_name: &str,
    source_tag: &'static str,
) -> Option<MobileSkillItem> {
    let header = read_prefix_utf8(path, MAX_SKILL_HEADER_BYTES)?;
    let (fm_name, fm_desc, body) = parse_frontmatter(&header);
    let raw_name = fm_name.unwrap_or_else(|| fallback_name.to_string());
    let name = sanitize_skill_name(&raw_name)?;
    let desc_raw = fm_desc
        .or_else(|| first_non_empty_line(body))
        .unwrap_or_default();
    let description = truncate_single_line(&desc_raw, MAX_DESC_CHARS);
    Some(MobileSkillItem {
        name,
        description,
        source: source_tag.to_string(),
    })
}

fn read_prefix_utf8(path: &Path, limit: usize) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::with_capacity(limit.min(1024));
    file.take(limit as u64).read_to_end(&mut buf).ok()?;
    // 截断可能落在多字节 UTF-8 中间，容错转字符串
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>, &str) {
    let trimmed = text.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed
        .strip_prefix("---\r\n")
        .or_else(|| trimmed.strip_prefix("---\n"))
    else {
        return (None, None, trimmed);
    };

    let mut name: Option<String> = None;
    let mut desc: Option<String> = None;
    let mut body = "";
    let mut offset = 0usize;

    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let clean = line.trim();
        if clean == "---" || clean == "..." {
            body = &rest[offset..];
            break;
        }
        if let Some((k, v)) = clean.split_once(':') {
            let key = k.trim();
            let val = unquote_yaml_scalar(v.trim());
            if val.is_empty() {
                continue;
            }
            if key.eq_ignore_ascii_case("name") && name.is_none() {
                name = Some(val.to_string());
            } else if key.eq_ignore_ascii_case("description") && desc.is_none() {
                desc = Some(val.to_string());
            }
        }
    }
    (name, desc, body)
}

fn unquote_yaml_scalar(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        &raw[1..raw.len() - 1]
    } else if matches!(raw, "|" | ">" | "|-" | ">-") {
        ""
    } else {
        raw
    }
}

fn first_non_empty_line(body: &str) -> Option<String> {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn sanitize_skill_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .trim_start_matches(['/', '$'])
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .take(64)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn truncate_single_line(raw: &str, max_chars: usize) -> String {
    let compact: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::new();
    for (i, ch) in compact.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_key_encoding_respects_app_cursor_mode() {
        assert_eq!(encode_terminal_key(TerminalKey::Escape, false), "\x1b");
        assert_eq!(encode_terminal_key(TerminalKey::CtrlC, false), "\x03");
        assert_eq!(encode_terminal_key(TerminalKey::ShiftTab, false), "\x1b[Z");
        assert_eq!(encode_terminal_key(TerminalKey::Up, false), "\x1b[A");
        assert_eq!(encode_terminal_key(TerminalKey::Up, true), "\x1bOA");
        assert_eq!(encode_terminal_key(TerminalKey::Down, true), "\x1bOB");
        assert_eq!(encode_terminal_key(TerminalKey::Home, false), "\x1b[H");
        assert_eq!(encode_terminal_key(TerminalKey::Home, true), "\x1bOH");
    }

    #[test]
    fn search_project_files_filters_ignores_and_ranks_matches() {
        let dir = std::env::temp_dir().join(format!(
            "mt-relay-assist-files-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/App.tsx"), "export {}").unwrap();
        std::fs::write(dir.join("src/MirrorView.tsx"), "export {}").unwrap();
        std::fs::write(dir.join("README.md"), "# hi").unwrap();
        std::fs::write(dir.join("node_modules/pkg/App.tsx"), "ignored").unwrap();

        let (files, truncated) = search_project_files(&dir, "app").unwrap();
        assert!(!truncated);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/App.tsx");

        let (all, _) = search_project_files(&dir, "").unwrap();
        let paths: Vec<&str> = all.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"README.md"));
        assert!(paths.contains(&"src/MirrorView.tsx"));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_skills_extracts_frontmatter_and_prefers_project_over_user() {
        let base = std::env::temp_dir().join(format!(
            "mt-relay-assist-skills-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let proj = base.join("proj");
        let home = base.join("home");
        std::fs::create_dir_all(proj.join(".claude/skills/deploy")).unwrap();
        std::fs::create_dir_all(home.join(".claude/skills/deploy")).unwrap();
        std::fs::create_dir_all(home.join(".claude/commands")).unwrap();

        std::fs::write(
            proj.join(".claude/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: \"Project deploy flow\"\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: User deploy flow\n---\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/commands/review.md"),
            "# Quick PR review\nDetails...",
        )
        .unwrap();

        let list = scan_skills_with_home(&proj, Some(&home), Some(AgentKind::Claude));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "deploy");
        assert_eq!(list[0].source, "project");
        assert_eq!(list[0].description, "Project deploy flow");
        assert_eq!(list[1].name, "review");
        assert_eq!(list[1].source, "user");
        assert_eq!(list[1].description, "Quick PR review");

        let (prefix_codex, _) = scan_project_skills(&proj, Some("codex"));
        assert_eq!(prefix_codex, "$");
        let (prefix_claude, _) = scan_project_skills(&proj, Some("claude"));
        assert_eq!(prefix_claude, "/");

        let _ = std::fs::remove_dir_all(&base);
    }
}
