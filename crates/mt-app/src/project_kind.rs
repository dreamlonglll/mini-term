//! 目录技术栈探测(`src/utils/projectKind.ts` + `src/hooks/useProjectKinds.ts` 移植)。
//!
//! 项目列表的领位徽标与文件树里一级子工程的图标都靠它:扫目录**一层**的文件名,
//! 标记文件直判,`package.json` 再按依赖细分前端框架。
//!
//! # 三条纪律(与原版一致)
//!
//! 1. **只探本地项目**:远程(SSH)项目领位固定 SSH 图标,压根不探
//!    (GPUI 侧还没有远程项目,判据仍照写,mt-ssh 接上自动生效);
//! 2. **结果进缓存,认不出也进**(`Some(None)`)—— 否则每帧重探一次磁盘;
//! 3. **失效只有一条**:项目根目录里的标记文件(`Cargo.toml` / `package.json` /…)
//!    发生 `fs-change`。原版注释点明了理由:活跃项目的根目录正是唯一能在应用内
//!    被改到这些文件的地方。
//!
//! # 线程
//!
//! [`detect_local`] 会读目录、读 `package.json`,**阻塞**;调用方一律丢
//! background executor(见 [`crate::store::AppStore::ensure_dir_kinds`])。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mt_ui::icons::ProjectKind;

/// 出现在项目根目录即触发(重)探测的标记文件。逐字照抄
/// `projectKind.ts` 的 `PROJECT_MARKER_FILES`。
pub const PROJECT_MARKER_FILES: &[&str] = &[
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "pubspec.yaml",
    "composer.json",
    "package.json",
];

/// 文件名是不是「一出现就该重探」的标记文件。
pub fn is_marker_file(name: &str) -> bool {
    PROJECT_MARKER_FILES.contains(&name)
}

/// 路径规范化(`useProjectKinds.ts::normPath`):分隔符统一成 `/`、去掉尾部斜杠。
///
/// 只用于**失效比对**(fs-change 的父目录 vs 项目路径),缓存键仍是路径原文 ——
/// 与原版 `dirKinds` 用原始路径当 key 同口径。
pub fn norm_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut last_sep = false;
    for ch in p.chars() {
        if ch == '\\' || ch == '/' {
            if !last_sep {
                out.push('/');
            }
            last_sep = true;
        } else {
            out.push(ch);
            last_sep = false;
        }
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// 规则从具体到泛化:标记文件直判,`package.json` 再按依赖细分前端框架。
///
/// 逐条照抄 `classifyProject`,**顺序不能动** —— 一个仓库同时有 `Cargo.toml`
/// 和 `package.json` 时,原版给的是 Rust。
pub fn classify_project(
    files: &HashSet<String>,
    deps: Option<&HashMap<String, String>>,
) -> Option<ProjectKind> {
    let has = |n: &str| files.contains(n);
    if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        return Some(ProjectKind::Java);
    }
    if has("Cargo.toml") {
        return Some(ProjectKind::Rust);
    }
    if has("go.mod") {
        return Some(ProjectKind::Go);
    }
    if has("pyproject.toml") || has("requirements.txt") {
        return Some(ProjectKind::Python);
    }
    if has("pubspec.yaml") {
        return Some(ProjectKind::Flutter);
    }
    if has("composer.json") {
        return Some(ProjectKind::Php);
    }
    if has("package.json") {
        if let Some(deps) = deps {
            if deps.contains_key("vue") {
                return Some(ProjectKind::Vue);
            }
            if deps.contains_key("next") {
                return Some(ProjectKind::Next);
            }
            if deps.contains_key("react") {
                return Some(ProjectKind::React);
            }
            if deps.contains_key("svelte") {
                return Some(ProjectKind::Svelte);
            }
            if deps.contains_key("vite") {
                return Some(ProjectKind::Vite);
            }
        }
        return Some(ProjectKind::Node);
    }
    None
}

/// `package.json` 文本 → `dependencies` + `devDependencies` 合并表。
/// 解析失败返回 `None`(按「没有 deps」处理,仍能给出 Node.js 级别的判定)。
pub fn parse_package_deps(json_text: &str) -> Option<HashMap<String, String>> {
    let value: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let obj = value.as_object()?;
    let mut out = HashMap::new();
    // 顺序与原版展开式 `{...dependencies, ...devDependencies}` 一致:
    // 同名时 devDependencies 覆盖(值不参与判定,写法对齐而已)
    for field in ["dependencies", "devDependencies"] {
        if let Some(map) = obj.get(field).and_then(|v| v.as_object()) {
            for (k, v) in map {
                out.insert(
                    k.clone(),
                    v.as_str().map(str::to_string).unwrap_or_default(),
                );
            }
        }
    }
    Some(out)
}

/// 探一个本地目录。**阻塞**(读目录 + 可能读 `package.json`)。
///
/// 与原版 `detectLocal` 同构:`list_directory(path, path)` 列自己一层,
/// 再按需读 `package.json`;读不了就按无 deps 处理。
pub fn detect_local(dir: &Path) -> Option<ProjectKind> {
    let entries = mt_project::fs::list_directory(dir, dir).ok()?;
    let files: HashSet<String> = entries
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name)
        .collect();
    let deps = if files.contains("package.json") {
        mt_project::fs::read_file_content(dir, &dir.join("package.json"))
            .ok()
            .filter(|r| !r.is_binary && !r.too_large)
            .and_then(|r| parse_package_deps(&r.content))
    } else {
        None
    };
    classify_project(&files, deps.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn deps(names: &[&str]) -> HashMap<String, String> {
        names
            .iter()
            .map(|s| (s.to_string(), "1.0.0".to_string()))
            .collect()
    }

    #[test]
    fn 标记文件直判() {
        assert_eq!(
            classify_project(&files(&["pom.xml"]), None),
            Some(ProjectKind::Java)
        );
        assert_eq!(
            classify_project(&files(&["build.gradle.kts"]), None),
            Some(ProjectKind::Java)
        );
        assert_eq!(
            classify_project(&files(&["Cargo.toml"]), None),
            Some(ProjectKind::Rust)
        );
        assert_eq!(
            classify_project(&files(&["go.mod"]), None),
            Some(ProjectKind::Go)
        );
        assert_eq!(
            classify_project(&files(&["requirements.txt"]), None),
            Some(ProjectKind::Python)
        );
        assert_eq!(
            classify_project(&files(&["pubspec.yaml"]), None),
            Some(ProjectKind::Flutter)
        );
        assert_eq!(
            classify_project(&files(&["composer.json"]), None),
            Some(ProjectKind::Php)
        );
    }

    /// 顺序即优先级:Cargo.toml 与 package.json 同在时是 Rust,不是 Node。
    #[test]
    fn 具体规则压过泛化规则() {
        assert_eq!(
            classify_project(&files(&["Cargo.toml", "package.json"]), Some(&deps(&["react"]))),
            Some(ProjectKind::Rust)
        );
        assert_eq!(
            classify_project(&files(&["pom.xml", "Cargo.toml"]), None),
            Some(ProjectKind::Java)
        );
    }

    /// package.json 的依赖细分顺序:vue > next > react > svelte > vite > nodejs。
    #[test]
    fn 前端框架按依赖细分且有序() {
        let pkg = files(&["package.json"]);
        assert_eq!(
            classify_project(&pkg, Some(&deps(&["vue", "vite"]))),
            Some(ProjectKind::Vue)
        );
        assert_eq!(
            classify_project(&pkg, Some(&deps(&["next", "react"]))),
            Some(ProjectKind::Next),
            "Next 排在 React 之前 —— Next 项目必然也带 react"
        );
        assert_eq!(
            classify_project(&pkg, Some(&deps(&["react"]))),
            Some(ProjectKind::React)
        );
        assert_eq!(
            classify_project(&pkg, Some(&deps(&["svelte", "vite"]))),
            Some(ProjectKind::Svelte)
        );
        assert_eq!(
            classify_project(&pkg, Some(&deps(&["vite"]))),
            Some(ProjectKind::Vite)
        );
        assert_eq!(classify_project(&pkg, Some(&deps(&[]))), Some(ProjectKind::Node));
        assert_eq!(classify_project(&pkg, None), Some(ProjectKind::Node));
    }

    #[test]
    fn 认不出就是_none() {
        assert_eq!(classify_project(&files(&["README.md"]), None), None);
        assert_eq!(classify_project(&files(&[]), None), None);
    }

    /// 目录不参与判定 —— `detect_local` 只把 `is_dir == false` 的名字塞进集合,
    /// 这条钉住「有个叫 Cargo.toml 的**目录**不该判成 Rust」的口径来源。
    #[test]
    fn 标记文件集合就是全部输入() {
        // classify 本身只看集合,目录过滤在 detect_local 那一层
        assert_eq!(classify_project(&files(&["Cargo.toml"]), None), Some(ProjectKind::Rust));
    }

    #[test]
    fn deps_合并两张表() {
        let parsed = parse_package_deps(
            r#"{"dependencies":{"react":"^18"},"devDependencies":{"vite":"^5"}}"#,
        )
        .unwrap();
        assert!(parsed.contains_key("react"));
        assert!(parsed.contains_key("vite"));
    }

    #[test]
    fn deps_解析失败与非对象都退_none() {
        assert!(parse_package_deps("not json").is_none());
        assert!(parse_package_deps("[1,2,3]").is_none());
        assert!(parse_package_deps("null").is_none());
        // 合法对象但没有两张表:返回空表(不是 None)—— 与原版展开式一致
        assert_eq!(parse_package_deps("{}").unwrap().len(), 0);
    }

    #[test]
    fn 标记文件表判定() {
        assert!(is_marker_file("Cargo.toml"));
        assert!(is_marker_file("package.json"));
        assert!(!is_marker_file("Cargo.lock"));
        assert!(!is_marker_file("index.ts"));
        // 大小写敏感,与前端的 Set 一致
        assert!(!is_marker_file("cargo.toml"));
    }

    #[test]
    fn 路径规范化统一分隔符并去尾() {
        assert_eq!(norm_path(r"D:\Git\demo\"), "D:/Git/demo");
        assert_eq!(norm_path("D:/Git/demo"), "D:/Git/demo");
        assert_eq!(norm_path(r"D:\\Git\\demo"), "D:/Git/demo");
        assert_eq!(norm_path("/"), "/");
        assert_eq!(norm_path(""), "");
    }
}
