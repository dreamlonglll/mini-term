//! 文件类型判定、路径比对与扩展名 → 语言名的映射(纯逻辑,可测)。
//!
//! 扩展名口径逐条对照原版 `FileViewerModal.tsx` / `CodeEditor.tsx`,见各函数注释。
//! [`language_for`] 经 `file_viewer` 再导出,对外路径不变。

/// `FileViewerModal.tsx:27-29` 的 `isMarkdownFile`。
pub(super) fn is_markdown_file(path: &str) -> bool {
    has_ext(path, &["md", "markdown", "mkd", "mdx"])
}

/// `FileViewerModal.tsx:31-33` 的 `isImageFile`。
pub(super) fn is_image_file(path: &str) -> bool {
    has_ext(
        path,
        &[
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "avif", "tif", "tiff",
        ],
    )
}

/// `FileViewerModal.tsx:35-37` 的 `isHtmlFile`。
pub(super) fn is_html_file(path: &str) -> bool {
    has_ext(path, &["html", "htm"])
}

/// PDF 交给系统 WebView 内置的阅读器(见 [`super::webview`]),原版没有这一支。
pub(super) fn is_pdf_file(path: &str) -> bool {
    has_ext(path, &["pdf"])
}

/// 散文类文件折行,代码不折(`CodeEditor.tsx:203-206` 的 `shouldWrap`)。
pub(super) fn should_wrap(path: &str) -> bool {
    has_ext(path, &["md", "markdown", "mkd", "mdx", "txt"])
}

/// 扩展名(小写)属于给定集合。`.tar.gz` 这类只看最后一段,与 JS 正则同口径。
fn has_ext(path: &str, exts: &[&str]) -> bool {
    let name = file_name_of(path);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    exts.contains(&ext.as_str())
}

/// 路径的最后一段(两种分隔符都认 —— 远程/WSL 路径是 POSIX 的)。
pub(super) fn file_name_of(path: &str) -> &str {
    let cut = path.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    &path[cut..]
}

/// 两个路径指的是不是同一个文件。
///
/// **反斜杠归一 + 小写**(原版 `FileViewerModal.tsx:277` 的 `norm`,现走
/// [`mt_core::path_key::windows_eq_key`],多去一截尾随分隔符,文件路径不受影响)——
/// Windows 上 notify 回来的路径大小写与盘符分隔符都可能与用户点的那一个不一致,
/// 直接比 `PathBuf` 会漏掉外部修改事件。
pub(super) fn same_path(a: &str, b: &str) -> bool {
    use mt_core::path_key::windows_eq_key;
    windows_eq_key(a) == windows_eq_key(b)
}

/// 文件名 → 语言注册表里的名字(组件库内建的 30 种 + [`crate::syntax_languages`]
/// 补充的那些)。
///
/// 对照原版 `LanguageDescription.matchFilename(languages, fileName)`
/// (`CodeEditor.tsx:300`)覆盖的常见类型。认不出返回 `"text"`,落到 `Language::Plain`
/// —— 与原版「匹配不到就是纯文本」同义。
///
/// 特殊文件名(无扩展名的 `Makefile` / `Dockerfile` 之流)先于扩展名判定,
/// 与 [`mt_ui::icons::FileIcon`] 的「特殊文件名压扩展名」同一条规矩。
///
/// 没有专属语法包的类型退到**近似语言**(Dockerfile → bash,scss → css,vue / razor →
/// html,`.bzl` → python):语法树会带错误节点,但关键字/字符串/注释这些大头照样上色,
/// 比整篇纯文本强。哪些类型为什么没有专属包见 `syntax_languages.rs` 模块注释。
pub fn language_for(file_name: &str) -> &'static str {
    let name = file_name_of(file_name).to_ascii_lowercase();
    // 特殊文件名先判(有的根本没有扩展名,有的扩展名会指向错的语言:
    // `CMakeLists.txt` 的 `.txt` 什么都不是)
    match name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "dockerfile" | "containerfile" => return "bash",
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".zshrc" | ".zshenv"
        | ".zprofile" | ".profile" | ".envrc" | "pkgbuild" => return "bash",
        // 锁文件与 Python 打包文件是 TOML 语法
        "cargo.lock" | "pipfile" | "poetry.lock" | "uv.lock" | "pdm.lock" => return "toml",
        // Ruby DSL
        "gemfile" | "rakefile" | "vagrantfile" | "podfile" | "fastfile" | "brewfile"
        | "guardfile" | "capfile" => return "ruby",
        "jenkinsfile" => return "groovy",
        // Bazel / Buck 的 Starlark 是 Python 子集
        "build" | "build.bazel" | "workspace" | "workspace.bazel" | "buck" => return "python",
        ".editorconfig" | ".gitconfig" | ".gitmodules" | ".npmrc" | ".yarnrc" => return "ini",
        ".babelrc" | ".eslintrc" | ".prettierrc" | ".swcrc" | ".jshintrc" | ".stylelintrc" => {
            return "json";
        }
        ".clang-format" | ".clang-tidy" | ".clangd" => return "yaml",
        _ => {}
    }
    // `.env` / `.env.local` / `.env.production`:KEY=VALUE,bash 语法照单全收
    if name == ".env" || name.starts_with(".env.") {
        return "bash";
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return "text";
    };
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" | "jsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "json" | "jsonc" | "jsonl" | "ndjson" | "json5" | "webmanifest" | "geojson" | "har"
        | "avsc" | "code-workspace" | "code-snippets" | "ipynb" => "json",
        "py" | "pyi" | "pyw" | "pyx" | "pxd" | "bzl" => "python",
        "go" => "go",
        "rb" | "rake" | "gemspec" | "ru" | "rbw" => "ruby",
        "java" | "aidl" => "java",
        "cs" | "csx" => "csharp",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ino" | "cu" | "cuh" | "mm" => "cpp",
        "css" | "scss" | "less" | "pcss" | "postcss" => "css",
        // vue / razor / 各种 HTML 模板都退到 html:标签与 `<script>` / `<style>` 照常上色
        "html" | "htm" | "xhtml" | "vue" | "cshtml" | "razor" | "astro" | "hbs" | "handlebars"
        | "mustache" | "gohtml" | "jsp" | "twig" | "liquid" | "heex" | "eex" => "html",
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "dockerfile" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" | "mkd" | "mdx" => "markdown",
        "sql" | "psql" | "pgsql" | "mysql" | "ddl" | "dml" => "sql",
        "swift" => "swift",
        "zig" | "zon" => "zig",
        "ex" | "exs" => "elixir",
        "scala" | "sbt" | "sc" => "scala",
        "proto" => "proto",
        "graphql" | "gql" | "graphqls" => "graphql",
        "diff" | "patch" => "diff",
        "cmake" => "cmake",
        "ejs" => "ejs",
        "erb" => "erb",
        "mk" => "make",
        // ---- 以下由 syntax_languages 补充 ----
        "php" | "phtml" | "php5" | "phps" => "php",
        "kt" | "kts" => "kotlin",
        "lua" | "luau" => "lua",
        "ps1" | "psm1" | "psd1" => "powershell",
        // .NET / Java / Apple 工程里那一堆 XML 方言(csproj / xaml / plist / storyboard …)
        "xml" | "xsd" | "xsl" | "xslt" | "wsdl" | "svg" | "plist" | "xaml" | "axaml" | "resx"
        | "nuspec" | "csproj" | "fsproj" | "vbproj" | "vcxproj" | "filters" | "props"
        | "targets" | "config" | "manifest" | "ps1xml" | "pom" | "iml" | "storyboard" | "xib"
        | "ui" | "qrc" | "wxs" | "opml" | "rss" | "atom" | "fxml" | "xcscheme"
        | "xcworkspacedata" | "xcprivacy" | "entitlements" => "xml",
        "dart" => "dart",
        "groovy" | "gradle" | "gvy" | "gy" | "gsh" => "groovy",
        // key=value 一族:ini 语法允许节前裸键,.properties / systemd unit / .desktop 都吃得下
        "ini" | "cfg" | "conf" | "reg" | "properties" | "service" | "socket" | "timer"
        | "desktop" | "flake8" | "pylintrc" | "gitconfig" => "ini",
        "bat" | "cmd" => "batch",
        // 模板类没有专属语法,退到 html:标签与 <script> / <style> 照常上色
        "svelte" | "j2" | "jinja" | "jinja2" => "html",
        _ => "text",
    }
}

#[cfg(test)]
#[path = "file_kind_tests.rs"]
mod tests;
