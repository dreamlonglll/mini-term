//! 项目级生成 skill 的公共底座:两个「把 skill 投进项目」的注册器
//! (`ssh_registry` 的 mini-term-ssh、`orchestrator_skill` 的 mini-term-orchestrator)
//! 共用的落盘路径、shell 引号、`.gitignore` 追加与 Codex 项目信任。
//!
//! 只放**与具体 skill 内容无关**的东西。SKILL.md 怎么渲染、什么时候写、什么
//! 时候删是各注册器自己的事:mini-term-ssh 按项目开关启停,
//! mini-term-orchestrator 跟着编排者 pane 的生死走(见各自的模块注释)。
//!
//! 全部是**同步阻塞**的文件 IO,线程口径由调用方定。

use std::path::{Path, PathBuf};

use mt_core::atomic_write;

/// 校验并规整项目目录路径。要求传入的是一个已存在的目录。
pub(crate) fn validate_project_dir(project_dir: &str) -> Result<PathBuf, String> {
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

// ─── shell 引号 ───

pub(crate) fn quote_posix_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn quote_powershell_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ─── 落盘路径 ───

/// 某个 skill 在项目里的两份落盘路径:Claude 版(`true` = 含 `allowed-tools`,
/// skill 激活期间免审批)与 Codex 版(`false`,规避未知 frontmatter 字段的
/// 容忍度风险)。
pub(crate) fn skill_paths(project_dir: &Path, skill_dir_name: &str) -> [(PathBuf, bool); 2] {
    [
        (
            project_dir
                .join(".claude")
                .join("skills")
                .join(skill_dir_name)
                .join("SKILL.md"),
            true,
        ),
        (
            project_dir
                .join(".codex")
                .join("skills")
                .join(skill_dir_name)
                .join("SKILL.md"),
            false,
        ),
    ]
}

/// 删掉一份 SKILL.md 之后收空壳:`<skill>/ → skills/ → .claude|.codex/`,
/// 逐级只删空目录(`remove_dir` 遇非空或不存在即失败而停,安全)。
pub(crate) fn prune_empty_skill_dirs(skill_md: &Path) {
    let mut dir = skill_md.parent();
    for _ in 0..3 {
        let Some(d) = dir else { break };
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

// ─── <project>/.gitignore ───

/// 计算追加条目后的 `.gitignore` 全文;若无需追加返回 `None`。抽出便于单测。
///
/// `header` 是追加段前那行注释,`entries` 是要保证在场的条目(逐行 trim 后
/// 比对,已有的不重复)。**跟着文件已有的换行风格走**:CRLF 的文件就追加 CRLF
/// 行 —— 混进 LF 会让 git 把整个文件判成「行尾全改」(2026-09-02 在本仓踩到)。
pub(crate) fn compute_gitignore_append(
    existing: &str,
    header: &str,
    entries: &[&str],
) -> Option<String> {
    let present: std::collections::HashSet<&str> = existing.lines().map(|l| l.trim()).collect();
    let missing: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|e| !present.contains(*e))
        .collect();
    if missing.is_empty() {
        return None;
    }

    let newline = if existing.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = existing.to_string();
    // 确保与已有内容之间有换行分隔
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(newline);
    }
    out.push_str(header);
    out.push_str(newline);
    for entry in missing {
        out.push_str(entry);
        out.push_str(newline);
    }
    Some(out)
}

/// 幂等地把若干条目追加进 `<project>/.gitignore`(生成物含机器相关绝对路径,
/// 不该进版本库)。
pub(crate) fn append_gitignore_entries(
    project_dir: &Path,
    header: &str,
    entries: &[&str],
) -> Result<(), String> {
    let gitignore_path = project_dir.join(".gitignore");
    let existing = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("读取 .gitignore 失败: {}", e))?
    } else {
        String::new()
    };

    let Some(appended) = compute_gitignore_append(&existing, header, entries) else {
        return Ok(());
    };

    atomic_write(&gitignore_path, appended.as_bytes())
        .map_err(|e| format!("写入 .gitignore 失败: {}", e))?;
    Ok(())
}

// ─── Codex 项目信任 ───

/// 获取 Codex 全局配置文件路径: `~/.codex/config.toml`
pub(crate) fn codex_global_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// 在 Codex 全局 `~/.codex/config.toml` 里把项目标为 `trust_level = "trusted"`。
///
/// Codex 要求项目目录被信任后,其 `<project>/.codex/` 内容才生效;未信任则
/// 项目级配置(含 skills)可能被静默忽略。
///
/// 幂等:若该项目路径已有 `[projects."..."]` 条目,保留其它字段、只确保
/// `trust_level` 为 `"trusted"`;**内容没变就不落盘**(编排者每次起 pane 都会
/// 走到这里,别每次都重写 home 下的文件)。停用时**不**移除信任(无法可靠
/// 判断是否本功能所加,且信任本身无害)。
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

    let next = doc.to_string();
    if next == content {
        return Ok(());
    }
    atomic_write(&config_path, next.as_bytes())
        .map_err(|e| format!("写入 Codex config.toml 失败: {}", e))?;
    Ok(())
}

/// 在 `toml_edit` 文档里确保 `[projects."<key>"] trust_level = "trusted"`。抽出便于单测。
pub(crate) fn apply_codex_project_trust(doc: &mut toml_edit::DocumentMut, project_key: &str) {
    if doc.get("projects").is_none() {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["projects"] = toml_edit::Item::Table(t);
    }
    doc["projects"][project_key]["trust_level"] = toml_edit::value("trusted");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-skill-files-test-{label}-{ts}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const HEADER: &str = "# test skill";
    const ENTRIES: &[&str] = &[".claude/skills/x/", ".codex/skills/x/"];

    // ─── 落盘路径 / 空壳清理 ───

    #[test]
    fn skill_paths_land_under_claude_and_codex() {
        let dir = PathBuf::from("/proj");
        let [(claude, claude_tools), (codex, codex_tools)] = skill_paths(&dir, "my-skill");
        assert!(claude.ends_with(Path::new(".claude/skills/my-skill/SKILL.md")));
        assert!(codex.ends_with(Path::new(".codex/skills/my-skill/SKILL.md")));
        // Claude 版带 allowed-tools,Codex 版不带
        assert!(claude_tools);
        assert!(!codex_tools);
    }

    #[test]
    fn prune_removes_only_empty_parents() {
        let dir = unique_test_dir("prune");
        let [(claude, _), _] = skill_paths(&dir, "s");
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&claude, b"x").unwrap();
        // 旁边放一个用户自己的文件:`.claude/` 不能被收走
        std::fs::write(dir.join(".claude").join("settings.local.json"), b"{}").unwrap();

        std::fs::remove_file(&claude).unwrap();
        prune_empty_skill_dirs(&claude);

        assert!(!dir.join(".claude").join("skills").exists(), "空的 skills/ 该收掉");
        assert!(dir.join(".claude").join("settings.local.json").exists(), "用户文件毫发无损");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── .gitignore 追加 ───

    #[test]
    fn gitignore_append_on_empty_adds_all_entries() {
        let result = compute_gitignore_append("", HEADER, ENTRIES).unwrap();
        assert!(result.starts_with(HEADER));
        assert!(result.contains(".claude/skills/x/"));
        assert!(result.contains(".codex/skills/x/"));
    }

    #[test]
    fn gitignore_append_skips_existing_entries() {
        let existing = "node_modules\n.claude/skills/x/\n.codex/skills/x/\n";
        assert!(compute_gitignore_append(existing, HEADER, ENTRIES).is_none());
    }

    #[test]
    fn gitignore_append_adds_only_missing_entry() {
        let existing = "node_modules\n.claude/skills/x/\n";
        let result = compute_gitignore_append(existing, HEADER, ENTRIES).unwrap();
        assert!(result.contains("node_modules"));
        assert!(result.contains(".codex/skills/x/"));
        // 已有条目不重复
        assert_eq!(result.matches(".claude/skills/x/").count(), 1);
    }

    #[test]
    fn gitignore_append_separates_from_unterminated_tail() {
        // 末尾没换行的 .gitignore:追加段不能粘在最后一行后面
        let result = compute_gitignore_append("dist", HEADER, ENTRIES).unwrap();
        assert!(result.starts_with("dist\n# test skill\n"), "got:\n{result}");
    }

    #[test]
    fn gitignore_append_file_round_trip_idempotent() {
        let dir = unique_test_dir("gitignore");
        std::fs::write(dir.join(".gitignore"), "node_modules\n").unwrap();

        append_gitignore_entries(&dir, HEADER, ENTRIES).unwrap();
        append_gitignore_entries(&dir, HEADER, ENTRIES).unwrap();

        let content = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(content.contains("node_modules"));
        assert_eq!(content.matches(".claude/skills/x/").count(), 1);
        assert_eq!(content.matches(".codex/skills/x/").count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CRLF 的 `.gitignore` 追加 CRLF 行,不许混进 LF(git 会把整个文件判成行尾全改)。
    #[test]
    fn gitignore_append_follows_crlf_style() {
        let result = compute_gitignore_append("dist\r\nnode_modules\r\n", HEADER, ENTRIES).unwrap();
        assert_eq!(
            result,
            "dist\r\nnode_modules\r\n# test skill\r\n.claude/skills/x/\r\n.codex/skills/x/\r\n"
        );
        // 末尾没换行的 CRLF 文件:补的分隔也是 CRLF
        let result = compute_gitignore_append("dist\r\nnode_modules", HEADER, ENTRIES).unwrap();
        assert!(result.starts_with("dist\r\nnode_modules\r\n# test skill\r\n"), "got:\n{result:?}");
        // 已经在场的条目照样认得出(`lines()` 会剥掉 \r)
        assert!(
            compute_gitignore_append(
                "# test skill\r\n.claude/skills/x/\r\n.codex/skills/x/\r\n",
                HEADER,
                ENTRIES
            )
            .is_none()
        );
    }

    // ─── validate_project_dir ───

    #[test]
    fn validate_project_dir_rejects_empty_and_missing() {
        assert!(validate_project_dir("").is_err());
        assert!(validate_project_dir("   ").is_err());
        assert!(validate_project_dir("/definitely/not/a/real/dir/xyz123").is_err());
    }

    #[test]
    fn validate_project_dir_accepts_existing_dir() {
        let tmp = std::env::temp_dir();
        assert!(validate_project_dir(&tmp.to_string_lossy()).is_ok());
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

    /// 已经信任过的项目再来一次,文档文本一字不变 —— `trust_project_in_codex`
    /// 靠这条判「不用落盘」。
    #[test]
    fn codex_project_trust_is_textually_idempotent() {
        let initial = "[projects.\"/home/u/proj\"]\ntrust_level = \"trusted\"\n";
        let mut doc: toml_edit::DocumentMut = initial.parse().unwrap();
        apply_codex_project_trust(&mut doc, "/home/u/proj");
        assert_eq!(doc.to_string(), initial);
    }
}
