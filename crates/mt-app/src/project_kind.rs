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

/// 出现在项目根目录即触发(重)探测的标记文件。
///
/// 原版只有前十条(`projectKind.ts` 的 `PROJECT_MARKER_FILES`);种类从 12 扩到 51 后,
/// 每种能自动认出来的类型都得把自己的标记文件登记在这里,否则「新建 mix.exs 之后
/// 徽标不变」——得重启才刷新。按扩展名认的那几类(`*.csproj` / `*.tf` / `*.cabal`)
/// 走 [`is_marker_file`] 里的后缀判断,不在这张表里。
pub const PROJECT_MARKER_FILES: &[&str] = &[
    // 原版十条
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
    // 语言
    "setup.py",
    "Pipfile",
    "Gemfile",
    "Rakefile",
    "mix.exs",
    "build.sbt",
    "stack.yaml",
    "build.zig",
    "Package.swift",
    "Podfile",
    "CMakeLists.txt",
    "meson.build",
    "conanfile.txt",
    "conanfile.py",
    "global.json",
    "cpanfile",
    "Makefile.PL",
    "deno.json",
    "deno.jsonc",
    "bun.lockb",
    "bunfig.toml",
    // 框架
    "manage.py",
    "artisan",
    "config.ru",
    "project.godot",
    "angular.json",
    // 基础设施
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "Chart.yaml",
    "kustomization.yaml",
    "skaffold.yaml",
    "ansible.cfg",
    "playbook.yml",
];

/// 按扩展名认类型的那几类 —— 这些也得触发重探。
const MARKER_SUFFIXES: &[&str] = &[
    ".csproj", ".sln", ".vcxproj", ".xcodeproj", ".xcworkspace",
    ".gemspec", ".cabal", ".rockspec", ".tf",
];

/// 文件名是不是「一出现就该重探」的标记文件。
pub fn is_marker_file(name: &str) -> bool {
    PROJECT_MARKER_FILES.contains(&name)
        || MARKER_SUFFIXES
            .iter()
            .any(|s| name.len() > s.len() && name.to_ascii_lowercase().ends_with(s))
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

/// 探一个目录得到的原料。纯数据,[`classify_project`] 是纯函数,单测直接打在它上面。
#[derive(Debug, Default, Clone)]
pub struct ProjectProbe {
    /// 根目录下的文件名(原样大小写)。
    pub files: HashSet<String>,
    /// 根目录下的子目录名(原样大小写)。Unity 靠 `Assets` + `ProjectSettings`、
    /// Tauri 靠 `src-tauri` 认出来 —— 只看文件是认不出的。
    pub dirs: HashSet<String>,
    /// `package.json` 的 dependencies + devDependencies。
    pub deps: HashMap<String, String>,
    /// 少数几个标记文件的正文(已小写)。判 Spring / Django 之外的 Python 框架、
    /// 以及 `pubspec.yaml` 到底是 Flutter 还是纯 Dart 都得看正文。
    pub texts: HashMap<String, String>,
}

impl ProjectProbe {
    fn has(&self, n: &str) -> bool {
        self.files.contains(n)
    }

    fn has_dir(&self, n: &str) -> bool {
        self.dirs.contains(n)
    }

    /// 有没有这个扩展名的文件(大小写不敏感)。
    fn has_ext(&self, suffix: &str) -> bool {
        self.files
            .iter()
            .any(|f| f.len() > suffix.len() && f.to_ascii_lowercase().ends_with(suffix))
    }

    /// 某个标记文件的正文里有没有这个词。读不到正文一律算没有。
    fn text_has(&self, file: &str, needle: &str) -> bool {
        self.texts.get(file).is_some_and(|t| t.contains(needle))
    }

    fn dep(&self, n: &str) -> bool {
        self.deps.contains_key(n)
    }

    /// 有没有以这个前缀打头的依赖(`@angular/core` 这类 scope 包)。
    fn dep_scope(&self, prefix: &str) -> bool {
        self.deps.keys().any(|k| k.starts_with(prefix))
    }
}

/// 目录原料 → 技术栈。**顺序即优先级**,从具体到泛化。
///
/// 四段,段内也有先后:
///
/// 1. **框架先于语言**:Tauri 项目是 Rust + 前端,报 Tauri 比报 Rust 有信息量;
///    Django 项目当然也是 Python,但用户想看到的是 Django;
/// 2. **语言标记文件**:原版那六条在这一段,相对顺序原样保留 ——
///    一个仓库同时有 `Cargo.toml` 和 `package.json` 时给的仍是 Rust;
/// 3. **`package.json` 依赖细分**:更具体的框架在前(nuxt 含 vue、next 含 react,
///    反过来排就永远出不来 Nuxt / Next);
/// 4. **纯基础设施仓库兜底**:放最后,因为半数项目都躺着一个 Dockerfile ——
///    只有在完全没有语言标记时,才认为这是个「Docker 仓库」。
pub fn classify_project(probe: &ProjectProbe) -> Option<ProjectKind> {
    // ── 1. 框架先于语言 ──
    if probe.has_dir("Assets") && probe.has_dir("ProjectSettings") {
        return Some(ProjectKind::Unity);
    }
    if probe.has("project.godot") {
        return Some(ProjectKind::Godot);
    }
    if probe.has_dir("src-tauri") || probe.dep_scope("@tauri-apps/") {
        return Some(ProjectKind::Tauri);
    }
    if probe.dep("electron") || probe.dep("electron-builder") {
        return Some(ProjectKind::Electron);
    }
    if probe.has("artisan") && probe.has("composer.json") {
        return Some(ProjectKind::Laravel);
    }
    // Rails 的判据是 Gemfile + config.ru(Rack 入口)——只看 Gemfile 会把所有 Ruby 项目吞掉
    if probe.has("Gemfile") && (probe.has("config.ru") || probe.has_dir("app") && probe.has_dir("config")) {
        return Some(ProjectKind::Rails);
    }
    if probe.has("manage.py") {
        return Some(ProjectKind::Django);
    }
    // Spring 藏在 pom.xml / build.gradle 正文里,没有专属标记文件
    if probe.text_has("pom.xml", "spring-boot")
        || probe.text_has("pom.xml", "springframework")
        || probe.text_has("build.gradle", "org.springframework")
        || probe.text_has("build.gradle.kts", "org.springframework")
    {
        return Some(ProjectKind::Spring);
    }
    // FastAPI 在 Flask 之前:两者都可能出现在同一份 requirements 里(测试依赖),
    // 但一个项目自称 FastAPI 的信息量更大
    if PY_DEP_FILES.iter().any(|f| probe.text_has(f, "fastapi")) {
        return Some(ProjectKind::FastApi);
    }
    if PY_DEP_FILES.iter().any(|f| probe.text_has(f, "flask")) {
        return Some(ProjectKind::Flask);
    }

    // ── 2. 语言标记文件(前六条与原版同序) ──
    if probe.has("pom.xml") || probe.has("build.gradle") || probe.has("build.gradle.kts") {
        // build.gradle.kts 刻意仍判 Java:Kotlin DSL 早已是 Java 项目的常规写法,
        // 拿它认 Kotlin 会把一大批 Java 项目误标。Kotlin 走手动指定
        return Some(ProjectKind::Java);
    }
    if probe.has("Cargo.toml") {
        return Some(ProjectKind::Rust);
    }
    if probe.has("go.mod") {
        return Some(ProjectKind::Go);
    }
    if probe.has("pyproject.toml") || probe.has("requirements.txt") {
        return Some(ProjectKind::Python);
    }
    if probe.has("pubspec.yaml") {
        // 正文读得到且不含 flutter 才是纯 Dart;读不到一律按 Flutter
        // (pubspec.yaml 的项目绝大多数是 Flutter,这也保住了原版行为)
        return Some(if probe.texts.contains_key("pubspec.yaml") && !probe.text_has("pubspec.yaml", "flutter") {
            ProjectKind::Dart
        } else {
            ProjectKind::Flutter
        });
    }
    if probe.has("composer.json") {
        return Some(ProjectKind::Php);
    }
    // 以下是本次新增的语言,排在原版六条之后,不影响既有判定
    if probe.has("setup.py") || probe.has("Pipfile") {
        return Some(ProjectKind::Python);
    }
    if probe.has("Gemfile") || probe.has("Rakefile") || probe.has_ext(".gemspec") {
        return Some(ProjectKind::Ruby);
    }
    if probe.has("mix.exs") {
        return Some(ProjectKind::Elixir);
    }
    if probe.has("build.sbt") {
        return Some(ProjectKind::Scala);
    }
    if probe.has("stack.yaml") || probe.has_ext(".cabal") {
        return Some(ProjectKind::Haskell);
    }
    if probe.has("build.zig") {
        return Some(ProjectKind::Zig);
    }
    if probe.has_ext(".rockspec") {
        return Some(ProjectKind::Lua);
    }
    if probe.has("cpanfile") || probe.has("Makefile.PL") {
        return Some(ProjectKind::Perl);
    }
    if probe.has("Package.swift") {
        return Some(ProjectKind::Swift);
    }
    // Podfile / xcodeproj 是「苹果平台工程」,不一定是 Swift(可能是 ObjC)
    if probe.has("Podfile") || probe.has_ext(".xcodeproj") || probe.has_ext(".xcworkspace") {
        return Some(ProjectKind::Apple);
    }
    if probe.has("global.json") || probe.has_ext(".csproj") || probe.has_ext(".sln") {
        return Some(ProjectKind::CSharp);
    }
    // C 与 C++ 共用 CMake / meson,分不开 —— 取更常见的 C++,纯 C 项目走手动指定
    if probe.has("CMakeLists.txt")
        || probe.has("meson.build")
        || probe.has("conanfile.txt")
        || probe.has("conanfile.py")
        || probe.has_ext(".vcxproj")
    {
        return Some(ProjectKind::Cpp);
    }
    if probe.has("deno.json") || probe.has("deno.jsonc") {
        return Some(ProjectKind::Deno);
    }
    if probe.has("bun.lockb") || probe.has("bunfig.toml") {
        return Some(ProjectKind::Bun);
    }

    // ── 3. package.json 依赖细分 ──
    if probe.has("package.json") {
        // 具体的框架必须排在它依赖的那个框架之前
        if probe.dep("nuxt") {
            return Some(ProjectKind::Nuxt);
        }
        if probe.dep("next") {
            return Some(ProjectKind::Next);
        }
        if probe.dep_scope("@remix-run/") {
            return Some(ProjectKind::Remix);
        }
        if probe.dep("astro") {
            return Some(ProjectKind::Astro);
        }
        if probe.dep_scope("@angular/") {
            return Some(ProjectKind::Angular);
        }
        if probe.dep_scope("@nestjs/") {
            return Some(ProjectKind::Nest);
        }
        if probe.dep("solid-js") {
            return Some(ProjectKind::Solid);
        }
        if probe.dep("vue") {
            return Some(ProjectKind::Vue);
        }
        if probe.dep("react") {
            return Some(ProjectKind::React);
        }
        if probe.dep("svelte") {
            return Some(ProjectKind::Svelte);
        }
        if probe.dep("express") {
            return Some(ProjectKind::Express);
        }
        if probe.dep("vite") {
            return Some(ProjectKind::Vite);
        }
        return Some(ProjectKind::Node);
    }

    // ── 4. 纯基础设施仓库 ──
    if probe.has_ext(".tf") {
        return Some(ProjectKind::Terraform);
    }
    if probe.has("Chart.yaml") || probe.has("kustomization.yaml") || probe.has("skaffold.yaml") {
        return Some(ProjectKind::Kubernetes);
    }
    if probe.has("ansible.cfg") || probe.has("playbook.yml") || probe.has("site.yml") {
        return Some(ProjectKind::Ansible);
    }
    if probe.has("Dockerfile") || probe.has("docker-compose.yml") || probe.has("docker-compose.yaml") {
        return Some(ProjectKind::Docker);
    }
    None
}

/// 要读正文才认得出框架的 Python 依赖清单。
const PY_DEP_FILES: &[&str] = &["requirements.txt", "pyproject.toml", "Pipfile"];

/// 需要读正文的标记文件(判 Spring / Flask / FastAPI / Flutter-vs-Dart)。
///
/// 都是几 KB 的清单文件;`read_file_content` 自带二进制与超大文件的闸,
/// 读失败一律按「没有正文」处理,退化成只看文件名的判定。
const TEXT_MARKERS: &[&str] = &[
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "requirements.txt",
    "pyproject.toml",
    "Pipfile",
    "pubspec.yaml",
];

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

/// 探一个本地目录。**阻塞**(读目录 + 读少数几个标记文件)。
///
/// 与原版 `detectLocal` 同构:`list_directory(path, path)` 列自己一层,再按需读文件。
/// 读文件这步只对**在场的**标记文件做,一个都不在场时就是一次纯列目录 ——
/// 半数项目落在这一档。读失败一律按「没有正文」退化成只看文件名的判定。
pub fn detect_local(dir: &Path) -> Option<ProjectKind> {
    let entries = mt_project::fs::list_directory(dir, dir).ok()?;
    let mut probe = ProjectProbe::default();
    for e in entries {
        if e.is_dir {
            probe.dirs.insert(e.name);
        } else {
            probe.files.insert(e.name);
        }
    }

    let read = |name: &str| -> Option<String> {
        mt_project::fs::read_file_content(dir, &dir.join(name))
            .ok()
            .filter(|r| !r.is_binary && !r.too_large)
            .map(|r| r.content)
    };
    if probe.files.contains("package.json")
        && let Some(text) = read("package.json")
        && let Some(deps) = parse_package_deps(&text)
    {
        probe.deps = deps;
    }
    for marker in TEXT_MARKERS {
        if probe.files.contains(*marker)
            && let Some(text) = read(marker)
        {
            // 正文只用来做大小写不敏感的关键词匹配,存之前先小写化
            probe.texts.insert((*marker).to_string(), text.to_ascii_lowercase());
        }
    }
    classify_project(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有文件名的探测原料 —— 绝大多数用例够用。
    fn probe(names: &[&str]) -> ProjectProbe {
        ProjectProbe {
            files: names.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn with_deps(names: &[&str], dep_names: &[&str]) -> ProjectProbe {
        ProjectProbe {
            files: names.iter().map(|s| s.to_string()).collect(),
            deps: dep_names
                .iter()
                .map(|s| ((*s).to_string(), "1.0.0".to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn with_text(names: &[&str], file: &str, text: &str) -> ProjectProbe {
        let mut p = probe(names);
        p.texts.insert(file.to_string(), text.to_ascii_lowercase());
        p
    }

    fn kind(names: &[&str]) -> Option<ProjectKind> {
        classify_project(&probe(names))
    }

    #[test]
    fn 标记文件直判() {
        assert_eq!(kind(&["pom.xml"]), Some(ProjectKind::Java));
        assert_eq!(kind(&["build.gradle.kts"]), Some(ProjectKind::Java));
        assert_eq!(kind(&["Cargo.toml"]), Some(ProjectKind::Rust));
        assert_eq!(kind(&["go.mod"]), Some(ProjectKind::Go));
        assert_eq!(kind(&["requirements.txt"]), Some(ProjectKind::Python));
        assert_eq!(kind(&["pubspec.yaml"]), Some(ProjectKind::Flutter));
        assert_eq!(kind(&["composer.json"]), Some(ProjectKind::Php));
    }

    /// 顺序即优先级:Cargo.toml 与 package.json 同在时是 Rust,不是 Node。
    #[test]
    fn 具体规则压过泛化规则() {
        assert_eq!(
            classify_project(&with_deps(&["Cargo.toml", "package.json"], &["react"])),
            Some(ProjectKind::Rust)
        );
        assert_eq!(kind(&["pom.xml", "Cargo.toml"]), Some(ProjectKind::Java));
    }

    /// package.json 的依赖细分:更具体的框架必须排在它所依赖的那个之前。
    #[test]
    fn 前端框架按依赖细分且有序() {
        let pkg = ["package.json"];
        let of = |d: &[&str]| classify_project(&with_deps(&pkg, d));
        assert_eq!(of(&["vue", "vite"]), Some(ProjectKind::Vue));
        assert_eq!(
            of(&["next", "react"]),
            Some(ProjectKind::Next),
            "Next 排在 React 之前 —— Next 项目必然也带 react"
        );
        assert_eq!(
            of(&["nuxt", "vue"]),
            Some(ProjectKind::Nuxt),
            "Nuxt 排在 Vue 之前 —— Nuxt 项目必然也带 vue"
        );
        assert_eq!(of(&["@remix-run/react", "react"]), Some(ProjectKind::Remix));
        assert_eq!(of(&["astro"]), Some(ProjectKind::Astro));
        assert_eq!(of(&["@angular/core"]), Some(ProjectKind::Angular));
        assert_eq!(of(&["@nestjs/core", "express"]), Some(ProjectKind::Nest));
        assert_eq!(of(&["solid-js"]), Some(ProjectKind::Solid));
        assert_eq!(of(&["react"]), Some(ProjectKind::React));
        assert_eq!(of(&["svelte", "vite"]), Some(ProjectKind::Svelte));
        assert_eq!(of(&["express"]), Some(ProjectKind::Express));
        assert_eq!(of(&["vite"]), Some(ProjectKind::Vite));
        assert_eq!(of(&[]), Some(ProjectKind::Node));
        assert_eq!(kind(&["package.json"]), Some(ProjectKind::Node));
    }

    /// 跨端壳压过里面装的前端框架 —— 用户想看到的是「这是个桌面应用」。
    #[test]
    fn 跨端壳压过前端框架() {
        assert_eq!(
            classify_project(&with_deps(&["package.json"], &["electron", "react"])),
            Some(ProjectKind::Electron)
        );
        assert_eq!(
            classify_project(&with_deps(&["package.json"], &["@tauri-apps/api", "vue"])),
            Some(ProjectKind::Tauri)
        );
        // src-tauri 目录同样算数,且压过 Cargo.toml 的 Rust 判定
        let mut p = probe(&["Cargo.toml", "package.json"]);
        p.dirs.insert("src-tauri".to_string());
        assert_eq!(classify_project(&p), Some(ProjectKind::Tauri));
    }

    #[test]
    fn 后端框架压过语言() {
        // Django 有专属入口文件
        assert_eq!(
            kind(&["manage.py", "requirements.txt"]),
            Some(ProjectKind::Django)
        );
        // Laravel 认 artisan,但必须同时是个 PHP 项目
        assert_eq!(kind(&["artisan", "composer.json"]), Some(ProjectKind::Laravel));
        assert_eq!(kind(&["artisan"]), None, "光有 artisan 不算 Laravel");
        // Rails 认 Gemfile + Rack 入口;只有 Gemfile 的是普通 Ruby 项目
        assert_eq!(kind(&["Gemfile", "config.ru"]), Some(ProjectKind::Rails));
        assert_eq!(kind(&["Gemfile"]), Some(ProjectKind::Ruby));
    }

    #[test]
    fn 要读正文才认得出的几种() {
        // Spring 藏在 pom.xml 里
        assert_eq!(
            classify_project(&with_text(
                &["pom.xml"],
                "pom.xml",
                "<artifactId>spring-boot-starter</artifactId>"
            )),
            Some(ProjectKind::Spring)
        );
        // 读不到正文就退回 Java
        assert_eq!(kind(&["pom.xml"]), Some(ProjectKind::Java));

        // Flask / FastAPI 藏在依赖清单里
        assert_eq!(
            classify_project(&with_text(
                &["requirements.txt"],
                "requirements.txt",
                "Flask==3.0"
            )),
            Some(ProjectKind::Flask)
        );
        assert_eq!(
            classify_project(&with_text(
                &["pyproject.toml"],
                "pyproject.toml",
                "fastapi = \"^0.110\""
            )),
            Some(ProjectKind::FastApi)
        );

        // pubspec.yaml:正文明确不含 flutter 才是纯 Dart,读不到一律按 Flutter
        assert_eq!(
            classify_project(&with_text(
                &["pubspec.yaml"],
                "pubspec.yaml",
                "name: mylib\nenvironment:\n  sdk: ^3.0.0"
            )),
            Some(ProjectKind::Dart)
        );
        assert_eq!(
            classify_project(&with_text(
                &["pubspec.yaml"],
                "pubspec.yaml",
                "dependencies:\n  flutter:\n    sdk: flutter"
            )),
            Some(ProjectKind::Flutter)
        );
    }

    #[test]
    fn 按扩展名认的几种() {
        assert_eq!(kind(&["MyApp.csproj"]), Some(ProjectKind::CSharp));
        assert_eq!(kind(&["MyApp.sln"]), Some(ProjectKind::CSharp));
        assert_eq!(kind(&["mylib.gemspec"]), Some(ProjectKind::Ruby));
        assert_eq!(kind(&["project.cabal"]), Some(ProjectKind::Haskell));
        assert_eq!(kind(&["rock-1.0.rockspec"]), Some(ProjectKind::Lua));
        assert_eq!(kind(&["main.tf"]), Some(ProjectKind::Terraform));
        assert_eq!(kind(&["App.xcodeproj"]), Some(ProjectKind::Apple));
        // 光是扩展名一致但没有名字主体的不算(".tf" 本身是个隐藏文件)
        assert_eq!(kind(&[".tf"]), None);
    }

    #[test]
    fn 新增语言各认各的标记文件() {
        for (marker, expect) in [
            ("mix.exs", ProjectKind::Elixir),
            ("build.sbt", ProjectKind::Scala),
            ("stack.yaml", ProjectKind::Haskell),
            ("build.zig", ProjectKind::Zig),
            ("Package.swift", ProjectKind::Swift),
            ("Podfile", ProjectKind::Apple),
            ("CMakeLists.txt", ProjectKind::Cpp),
            ("meson.build", ProjectKind::Cpp),
            ("global.json", ProjectKind::CSharp),
            ("cpanfile", ProjectKind::Perl),
            ("deno.json", ProjectKind::Deno),
            ("bunfig.toml", ProjectKind::Bun),
            ("setup.py", ProjectKind::Python),
            ("Pipfile", ProjectKind::Python),
            ("project.godot", ProjectKind::Godot),
        ] {
            assert_eq!(kind(&[marker]), Some(expect), "{marker}");
        }
    }

    #[test]
    fn 游戏引擎靠目录结构() {
        let mut p = ProjectProbe::default();
        p.dirs.insert("Assets".to_string());
        p.dirs.insert("ProjectSettings".to_string());
        assert_eq!(classify_project(&p), Some(ProjectKind::Unity));
        // 只有一半不算
        let mut half = ProjectProbe::default();
        half.dirs.insert("Assets".to_string());
        assert_eq!(classify_project(&half), None);
    }

    /// 基础设施排在最末:半数项目都躺着一个 Dockerfile,它不该压过语言判定。
    #[test]
    fn 基础设施只在没有语言标记时兜底() {
        assert_eq!(kind(&["Dockerfile"]), Some(ProjectKind::Docker));
        assert_eq!(
            kind(&["Dockerfile", "Cargo.toml"]),
            Some(ProjectKind::Rust),
            "有 Dockerfile 的 Rust 项目仍是 Rust"
        );
        assert_eq!(
            classify_project(&with_deps(
                &["docker-compose.yml", "package.json"],
                &["react"]
            )),
            Some(ProjectKind::React)
        );
        assert_eq!(kind(&["Chart.yaml"]), Some(ProjectKind::Kubernetes));
        assert_eq!(kind(&["ansible.cfg"]), Some(ProjectKind::Ansible));
    }

    #[test]
    fn 认不出就是_none() {
        assert_eq!(kind(&["README.md"]), None);
        assert_eq!(kind(&[]), None);
    }

    /// 目录与文件是两个集合 —— 有个叫 `Cargo.toml` 的**目录**不该判成 Rust。
    #[test]
    fn 目录名不当文件名用() {
        let mut p = ProjectProbe::default();
        p.dirs.insert("Cargo.toml".to_string());
        assert_eq!(classify_project(&p), None);
        assert_eq!(kind(&["Cargo.toml"]), Some(ProjectKind::Rust));
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
        // 按扩展名认的那几类也要能触发重探,否则新建 .csproj 后徽标不刷新
        assert!(is_marker_file("MyApp.csproj"));
        assert!(is_marker_file("main.tf"));
        assert!(!is_marker_file(".tf"), "光一个扩展名不是标记文件");
    }

    /// 每种能自动认出来的类型,它的标记文件都得在重探表里 —— 否则新建那个文件
    /// 之后徽标不刷新,得重启才生效。这条是最容易漏的一步。
    #[test]
    fn 会被判定的标记文件都登记了重探() {
        for marker in [
            "mix.exs",
            "build.sbt",
            "stack.yaml",
            "build.zig",
            "Package.swift",
            "Podfile",
            "CMakeLists.txt",
            "meson.build",
            "global.json",
            "cpanfile",
            "deno.json",
            "bunfig.toml",
            "setup.py",
            "Pipfile",
            "project.godot",
            "manage.py",
            "artisan",
            "config.ru",
            "Gemfile",
            "Rakefile",
            "Dockerfile",
            "Chart.yaml",
            "ansible.cfg",
        ] {
            assert!(is_marker_file(marker), "{marker} 没登记进重探表");
        }
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
