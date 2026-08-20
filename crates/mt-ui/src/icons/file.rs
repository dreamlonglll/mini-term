//! 文件树图标(对照 `src/utils/fileIcon.ts` + `src/components/FileTree.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版调 `@baybreezy/file-extension-icon`(Material Icon Theme 的全量 SVG,
//! gzip 约 1.2MB,懒加载切独立 chunk),按文件名/扩展名返回一枚**专属图标**。
//! 那张映射表在 npm 包里,仓库中没有源文件,而且它是「一个类型一张图」的量级
//! (数百枚),照搬进 Rust 既不现实也没必要。
//!
//! GPUI 侧按任务书允许的降级做法:**统一的文件/目录轮廓 + 按类别换的内嵌记号 +
//! 逐语言的品牌色**。覆盖面向原版看齐 —— 常见语言、前端栈、配置、数据、媒体、
//! 归档、二进制、证书、锁文件、git 元数据都在表里,并且**特殊文件名优先于扩展名**
//! (`Cargo.lock` 是锁文件不是 toml,`Dockerfile` 没有扩展名),这条是原版
//! Material Icon Theme 的核心语义。
//!
//! # 已知偏差
//!
//! - 同类别的不同扩展名共用一枚记号(如 `.rs` 与 `.go` 都是 `<>`),靠**颜色**区分;
//!   原版是一类型一图案。色觉障碍下这比原版弱,但比「所有文件一个灰图标」强得多;
//! - 目录只有开/合两态,没有原版按目录名换图(`src` / `node_modules` / `.github` …)。
//!
//! # 宿主接线(mt-app 的文件树)
//!
//! ```ignore
//! use mt_ui::icons::FileIcon;
//! // 目录:展开态自己传;文件:后两个参数给 false
//! row = row.child(FileIcon::new(&entry.name, entry.is_dir, expanded).size(px(14.0)));
//! ```
//!
//! 项目根节点仍应优先用技术栈徽标(原版 `FileTree.tsx:349` 就是这么做的):
//!
//! ```ignore
//! match dir_kind {
//!     Some(kind) => TechIcon::new(kind).size(px(14.0)).into_any_element(),
//!     None => FileIcon::new(&entry.name, true, expanded).into_any_element(),
//! }
//! ```

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::vector::{Geom, Ink, Shape, VectorIcon};
use crate::terminal::rgb8;

/// 文件/目录的图标类别。颜色逐条对齐 Material Icon Theme 的语言色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Directory,
    DirectoryOpen,
    Rust,
    Go,
    Python,
    Java,
    Kotlin,
    Swift,
    CFamily,
    CSharp,
    Ruby,
    Php,
    Lua,
    Dart,
    Scala,
    Haskell,
    Elixir,
    Zig,
    JavaScript,
    TypeScript,
    ReactScript,
    Vue,
    Svelte,
    Html,
    Xml,
    Css,
    Sass,
    Json,
    Yaml,
    Toml,
    Ini,
    Csv,
    Env,
    Markdown,
    Text,
    Pdf,
    Image,
    Video,
    Audio,
    Archive,
    Binary,
    Font,
    Database,
    Sql,
    Shell,
    PowerShell,
    Lock,
    Git,
    Config,
    Docker,
    Certificate,
    Log,
    Unknown,
}

impl FileKind {
    /// 文件名 → 类别。**顺序即语义**:特殊文件名 → 点开头的裸文件 → 扩展名 → 兜底。
    ///
    /// `is_open` 只对目录有意义。
    pub fn of(name: &str, is_dir: bool, is_open: bool) -> Self {
        if is_dir {
            return if is_open {
                Self::DirectoryOpen
            } else {
                Self::Directory
            };
        }
        let lower = name.to_ascii_lowercase();

        // 1. 整名命中(Cargo.lock 不是 toml,Dockerfile 没有扩展名)
        if let Some(kind) = lookup(EXACT_NAMES, &lower) {
            return kind;
        }
        // 2. 前缀命中(docker-compose.override.yml / .env.production / Dockerfile.dev)
        for (prefix, kind) in NAME_PREFIXES {
            if lower.starts_with(prefix) {
                return *kind;
            }
        }
        // 3. 扩展名。`.gitignore` 这类点开头的裸文件没有扩展名(整名已在第 1 步兜住)
        if let Some(ext) = extension_of(&lower)
            && let Some(kind) = lookup(EXTENSIONS, ext)
        {
            return kind;
        }
        Self::Unknown
    }

    /// 图标主色。
    pub fn color(self) -> Hsla {
        let (r, g, b) = match self {
            Self::Directory | Self::DirectoryOpen => (0xd4, 0xc8, 0xa0), // --color-folder
            Self::Rust => (0xde, 0xa5, 0x84),
            Self::Go => (0x00, 0xad, 0xd8),
            Self::Python => (0x37, 0x76, 0xab),
            Self::Java => (0xea, 0x2d, 0x2e),
            Self::Kotlin => (0xa9, 0x7b, 0xff),
            Self::Swift => (0xf0, 0x51, 0x38),
            Self::CFamily => (0x65, 0x9a, 0xd2),
            Self::CSharp => (0x9b, 0x4f, 0x96),
            Self::Ruby => (0xcc, 0x34, 0x2d),
            Self::Php => (0x77, 0x7b, 0xb4),
            Self::Lua => (0x5c, 0x7c, 0xfa),
            Self::Dart => (0x01, 0x75, 0xc2),
            Self::Scala => (0xdc, 0x32, 0x2f),
            Self::Haskell => (0x8f, 0x6f, 0xbd),
            Self::Elixir => (0x9b, 0x7c, 0xc4),
            Self::Zig => (0xf7, 0xa4, 0x1d),
            Self::JavaScript => (0xf1, 0xe0, 0x5a),
            Self::TypeScript => (0x31, 0x78, 0xc6),
            Self::ReactScript => (0x61, 0xda, 0xfb),
            Self::Vue => (0x41, 0xb8, 0x83),
            Self::Svelte => (0xff, 0x3e, 0x00),
            Self::Html => (0xe3, 0x4c, 0x26),
            Self::Xml => (0xf1, 0x66, 0x2a),
            Self::Css => (0x56, 0x8a, 0xd8),
            Self::Sass => (0xc6, 0x53, 0x8c),
            Self::Json => (0xcb, 0xcb, 0x41),
            Self::Yaml => (0xcb, 0x64, 0x5e),
            Self::Toml => (0xb0, 0x6c, 0x42),
            Self::Ini | Self::Config => (0x8a, 0x93, 0x9c),
            Self::Csv => (0x89, 0xe0, 0x51),
            Self::Env => (0xec, 0xd5, 0x3f),
            Self::Markdown => (0x9a, 0xa7, 0xb0),
            Self::Text => (0xb0, 0xbe, 0xc5),
            Self::Pdf => (0xe5, 0x39, 0x35),
            Self::Image => (0xa0, 0x74, 0xc4),
            Self::Video => (0xfd, 0x97, 0x1f),
            Self::Audio => (0xc7, 0x92, 0xea),
            Self::Archive => (0xec, 0xa5, 0x17),
            Self::Binary => (0x78, 0x90, 0x9c),
            Self::Font => (0xf0, 0x62, 0x92),
            Self::Database => (0xff, 0x70, 0x43),
            Self::Sql => (0xe3, 0x8c, 0x3c),
            Self::Shell => (0x89, 0xe0, 0x51),
            Self::PowerShell => (0x53, 0x91, 0xfe),
            Self::Lock => (0xf9, 0xa8, 0x25),
            Self::Git => (0xf1, 0x4e, 0x32),
            Self::Docker => (0x2b, 0x9d, 0xe5),
            Self::Certificate => (0xff, 0xb3, 0x00),
            Self::Log => (0xaf, 0xb4, 0x2b),
            Self::Unknown => (0x90, 0xa4, 0xae),
        };
        rgb8(r, g, b)
    }

    /// 轮廓(文件/目录两种)。
    fn outline(self) -> &'static [Shape] {
        match self {
            Self::Directory => FOLDER_CLOSED,
            Self::DirectoryOpen => FOLDER_OPEN,
            _ => FILE_SHEET,
        }
    }

    /// 轮廓里的记号。目录没有记号。
    fn mark(self) -> &'static [Shape] {
        match self {
            Self::Directory | Self::DirectoryOpen => &[],
            // 源码:尖括号
            Self::Rust
            | Self::Go
            | Self::Java
            | Self::Kotlin
            | Self::Swift
            | Self::CFamily
            | Self::CSharp
            | Self::Ruby
            | Self::Php
            | Self::Lua
            | Self::Dart
            | Self::Scala
            | Self::Haskell
            | Self::Elixir
            | Self::Zig
            | Self::Python
            | Self::JavaScript
            | Self::TypeScript
            | Self::ReactScript
            | Self::Vue
            | Self::Svelte
            | Self::Html
            | Self::Xml => MARK_ANGLES,
            // 结构化数据:花括号
            Self::Json | Self::Yaml | Self::Toml | Self::Ini | Self::Env => MARK_BRACES,
            Self::Css | Self::Sass => MARK_DROP,
            Self::Csv => MARK_GRID,
            Self::Markdown | Self::Text | Self::Pdf | Self::Log => MARK_LINES,
            Self::Image => MARK_IMAGE,
            Self::Video => MARK_PLAY,
            Self::Audio => MARK_NOTE,
            Self::Archive => MARK_ZIP,
            Self::Binary => MARK_CHIP,
            Self::Font => MARK_TYPE,
            Self::Database | Self::Sql => MARK_DISCS,
            Self::Shell | Self::PowerShell | Self::Docker => MARK_PROMPT,
            Self::Lock => MARK_LOCK,
            Self::Git => MARK_BRANCH,
            Self::Config => MARK_GEAR,
            Self::Certificate => MARK_KEY,
            Self::Unknown => &[],
        }
    }
}

/// 扩展名(小写、不含点)。没有扩展名或以点开头的裸文件返回 `None`。
fn extension_of(lower: &str) -> Option<&str> {
    let idx = lower.rfind('.')?;
    // `.gitignore` 的点在 0 位:那是「隐藏文件」不是「扩展名」
    if idx == 0 || idx + 1 >= lower.len() {
        return None;
    }
    Some(&lower[idx + 1..])
}

fn lookup(table: &[(&str, FileKind)], key: &str) -> Option<FileKind> {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, kind)| *kind)
}

/// 整名命中(全小写比对)。**必须在扩展名之前查** —— 这是 Material Icon Theme
/// 的核心语义:`Cargo.lock` 是锁文件不是 toml,`go.sum` 不是 sum 扩展名。
const EXACT_NAMES: &[(&str, FileKind)] = &[
    // 锁文件
    ("package-lock.json", FileKind::Lock),
    ("yarn.lock", FileKind::Lock),
    ("pnpm-lock.yaml", FileKind::Lock),
    ("bun.lockb", FileKind::Lock),
    ("cargo.lock", FileKind::Lock),
    ("poetry.lock", FileKind::Lock),
    ("composer.lock", FileKind::Lock),
    ("gemfile.lock", FileKind::Lock),
    ("go.sum", FileKind::Lock),
    ("uv.lock", FileKind::Lock),
    // 构建/包清单
    ("cargo.toml", FileKind::Toml),
    ("go.mod", FileKind::Go),
    ("package.json", FileKind::Json),
    ("composer.json", FileKind::Php),
    ("gemfile", FileKind::Ruby),
    ("rakefile", FileKind::Ruby),
    ("pubspec.yaml", FileKind::Dart),
    ("pyproject.toml", FileKind::Python),
    ("requirements.txt", FileKind::Python),
    ("setup.py", FileKind::Python),
    ("pom.xml", FileKind::Java),
    ("build.gradle", FileKind::Java),
    ("build.gradle.kts", FileKind::Kotlin),
    ("settings.gradle", FileKind::Java),
    ("makefile", FileKind::Config),
    ("gnumakefile", FileKind::Config),
    ("cmakelists.txt", FileKind::Config),
    ("justfile", FileKind::Config),
    ("tsconfig.json", FileKind::Config),
    ("jsconfig.json", FileKind::Config),
    // git 元数据
    (".gitignore", FileKind::Git),
    (".gitattributes", FileKind::Git),
    (".gitmodules", FileKind::Git),
    (".gitkeep", FileKind::Git),
    (".mailmap", FileKind::Git),
    // 容器
    ("dockerfile", FileKind::Docker),
    ("containerfile", FileKind::Docker),
    (".dockerignore", FileKind::Docker),
    // 各家 rc / 配置裸文件
    (".editorconfig", FileKind::Config),
    (".npmrc", FileKind::Config),
    (".nvmrc", FileKind::Config),
    (".babelrc", FileKind::Config),
    (".prettierrc", FileKind::Config),
    (".eslintrc", FileKind::Config),
    (".browserslistrc", FileKind::Config),
    (".gitlab-ci.yml", FileKind::Config),
    (".travis.yml", FileKind::Config),
    // 文档
    ("readme", FileKind::Markdown),
    ("readme.md", FileKind::Markdown),
    ("changelog.md", FileKind::Markdown),
    ("license", FileKind::Text),
    ("licence", FileKind::Text),
    ("copying", FileKind::Text),
    ("notice", FileKind::Text),
];

/// 前缀命中(整名没中时按前缀兜)。同样在扩展名之前。
const NAME_PREFIXES: &[(&str, FileKind)] = &[
    (".env", FileKind::Env),
    ("dockerfile.", FileKind::Docker),
    ("docker-compose", FileKind::Docker),
    (".eslintrc.", FileKind::Config),
    (".prettierrc.", FileKind::Config),
    ("license.", FileKind::Text),
];

/// 扩展名 → 类别。
const EXTENSIONS: &[(&str, FileKind)] = &[
    // 系统级语言
    ("rs", FileKind::Rust),
    ("go", FileKind::Go),
    ("zig", FileKind::Zig),
    ("c", FileKind::CFamily),
    ("h", FileKind::CFamily),
    ("cc", FileKind::CFamily),
    ("cpp", FileKind::CFamily),
    ("cxx", FileKind::CFamily),
    ("hpp", FileKind::CFamily),
    ("hh", FileKind::CFamily),
    ("hxx", FileKind::CFamily),
    ("m", FileKind::CFamily),
    ("mm", FileKind::CFamily),
    // JVM / .NET / 移动端
    ("java", FileKind::Java),
    ("kt", FileKind::Kotlin),
    ("kts", FileKind::Kotlin),
    ("scala", FileKind::Scala),
    ("sbt", FileKind::Scala),
    ("groovy", FileKind::Java),
    ("cs", FileKind::CSharp),
    ("csx", FileKind::CSharp),
    ("csproj", FileKind::CSharp),
    ("sln", FileKind::CSharp),
    ("fs", FileKind::CSharp),
    ("swift", FileKind::Swift),
    ("dart", FileKind::Dart),
    // 脚本语言
    ("py", FileKind::Python),
    ("pyi", FileKind::Python),
    ("pyw", FileKind::Python),
    ("ipynb", FileKind::Python),
    ("rb", FileKind::Ruby),
    ("erb", FileKind::Ruby),
    ("gemspec", FileKind::Ruby),
    ("php", FileKind::Php),
    ("phtml", FileKind::Php),
    ("lua", FileKind::Lua),
    ("pl", FileKind::Ruby),
    ("hs", FileKind::Haskell),
    ("lhs", FileKind::Haskell),
    ("ex", FileKind::Elixir),
    ("exs", FileKind::Elixir),
    ("erl", FileKind::Elixir),
    ("hrl", FileKind::Elixir),
    ("nim", FileKind::Config),
    ("r", FileKind::Config),
    ("jl", FileKind::Config),
    ("sol", FileKind::Config),
    ("vim", FileKind::Config),
    // 前端
    ("js", FileKind::JavaScript),
    ("mjs", FileKind::JavaScript),
    ("cjs", FileKind::JavaScript),
    ("ts", FileKind::TypeScript),
    ("mts", FileKind::TypeScript),
    ("cts", FileKind::TypeScript),
    ("jsx", FileKind::ReactScript),
    ("tsx", FileKind::ReactScript),
    ("vue", FileKind::Vue),
    ("svelte", FileKind::Svelte),
    ("astro", FileKind::Svelte),
    ("html", FileKind::Html),
    ("htm", FileKind::Html),
    ("xhtml", FileKind::Html),
    ("ejs", FileKind::Html),
    ("hbs", FileKind::Html),
    ("css", FileKind::Css),
    ("scss", FileKind::Sass),
    ("sass", FileKind::Sass),
    ("less", FileKind::Sass),
    ("styl", FileKind::Sass),
    // 标记 / 数据
    ("xml", FileKind::Xml),
    ("xsl", FileKind::Xml),
    ("xsd", FileKind::Xml),
    ("plist", FileKind::Xml),
    ("json", FileKind::Json),
    ("json5", FileKind::Json),
    ("jsonc", FileKind::Json),
    ("ndjson", FileKind::Json),
    ("jsonl", FileKind::Json),
    ("yaml", FileKind::Yaml),
    ("yml", FileKind::Yaml),
    ("toml", FileKind::Toml),
    ("ini", FileKind::Ini),
    ("cfg", FileKind::Ini),
    ("conf", FileKind::Ini),
    ("properties", FileKind::Ini),
    ("csv", FileKind::Csv),
    ("tsv", FileKind::Csv),
    ("env", FileKind::Env),
    ("graphql", FileKind::Config),
    ("gql", FileKind::Config),
    ("proto", FileKind::Config),
    ("tf", FileKind::Config),
    ("tfvars", FileKind::Config),
    ("hcl", FileKind::Config),
    ("gradle", FileKind::Config),
    ("cmake", FileKind::Config),
    ("mk", FileKind::Config),
    ("ninja", FileKind::Config),
    // 文档
    ("md", FileKind::Markdown),
    ("mdx", FileKind::Markdown),
    ("markdown", FileKind::Markdown),
    ("rst", FileKind::Markdown),
    ("adoc", FileKind::Markdown),
    ("txt", FileKind::Text),
    ("text", FileKind::Text),
    ("rtf", FileKind::Text),
    ("doc", FileKind::Text),
    ("docx", FileKind::Text),
    ("odt", FileKind::Text),
    ("pdf", FileKind::Pdf),
    ("log", FileKind::Log),
    // 媒体
    ("png", FileKind::Image),
    ("jpg", FileKind::Image),
    ("jpeg", FileKind::Image),
    ("gif", FileKind::Image),
    ("bmp", FileKind::Image),
    ("webp", FileKind::Image),
    ("ico", FileKind::Image),
    ("icns", FileKind::Image),
    ("tif", FileKind::Image),
    ("tiff", FileKind::Image),
    ("avif", FileKind::Image),
    ("heic", FileKind::Image),
    ("psd", FileKind::Image),
    ("svg", FileKind::Image),
    ("mp4", FileKind::Video),
    ("mkv", FileKind::Video),
    ("mov", FileKind::Video),
    ("avi", FileKind::Video),
    ("webm", FileKind::Video),
    ("flv", FileKind::Video),
    ("wmv", FileKind::Video),
    ("m4v", FileKind::Video),
    ("mp3", FileKind::Audio),
    ("wav", FileKind::Audio),
    ("flac", FileKind::Audio),
    ("ogg", FileKind::Audio),
    ("aac", FileKind::Audio),
    ("m4a", FileKind::Audio),
    ("opus", FileKind::Audio),
    ("wma", FileKind::Audio),
    // 归档 / 二进制
    ("zip", FileKind::Archive),
    ("tar", FileKind::Archive),
    ("gz", FileKind::Archive),
    ("tgz", FileKind::Archive),
    ("bz2", FileKind::Archive),
    ("xz", FileKind::Archive),
    ("7z", FileKind::Archive),
    ("rar", FileKind::Archive),
    ("zst", FileKind::Archive),
    ("lz4", FileKind::Archive),
    ("jar", FileKind::Archive),
    ("war", FileKind::Archive),
    ("ear", FileKind::Archive),
    ("exe", FileKind::Binary),
    ("dll", FileKind::Binary),
    ("so", FileKind::Binary),
    ("dylib", FileKind::Binary),
    ("bin", FileKind::Binary),
    ("wasm", FileKind::Binary),
    ("o", FileKind::Binary),
    ("a", FileKind::Binary),
    ("lib", FileKind::Binary),
    ("obj", FileKind::Binary),
    ("pdb", FileKind::Binary),
    ("class", FileKind::Binary),
    ("pyc", FileKind::Binary),
    ("msi", FileKind::Binary),
    ("dmg", FileKind::Binary),
    ("apk", FileKind::Binary),
    ("deb", FileKind::Binary),
    ("rpm", FileKind::Binary),
    // 字体 / 数据库 / 壳 / 证书
    ("ttf", FileKind::Font),
    ("otf", FileKind::Font),
    ("woff", FileKind::Font),
    ("woff2", FileKind::Font),
    ("eot", FileKind::Font),
    ("db", FileKind::Database),
    ("sqlite", FileKind::Database),
    ("sqlite3", FileKind::Database),
    ("mdb", FileKind::Database),
    ("accdb", FileKind::Database),
    ("realm", FileKind::Database),
    ("sql", FileKind::Sql),
    ("ddl", FileKind::Sql),
    ("sh", FileKind::Shell),
    ("bash", FileKind::Shell),
    ("zsh", FileKind::Shell),
    ("fish", FileKind::Shell),
    ("ksh", FileKind::Shell),
    ("bat", FileKind::Shell),
    ("cmd", FileKind::Shell),
    ("ps1", FileKind::PowerShell),
    ("psm1", FileKind::PowerShell),
    ("psd1", FileKind::PowerShell),
    ("lock", FileKind::Lock),
    ("pem", FileKind::Certificate),
    ("key", FileKind::Certificate),
    ("crt", FileKind::Certificate),
    ("cer", FileKind::Certificate),
    ("pfx", FileKind::Certificate),
    ("p12", FileKind::Certificate),
    ("pub", FileKind::Certificate),
    ("asc", FileKind::Certificate),
    ("gpg", FileKind::Certificate),
    ("patch", FileKind::Git),
    ("diff", FileKind::Git),
];

// ───────────────────────── 形状表 ─────────────────────────

/// 文件轮廓:一张纸 + 折角。
const FILE_SHEET: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.075,
        Geom::Polygon(&[
            (0.16, 0.06),
            (0.60, 0.06),
            (0.84, 0.30),
            (0.84, 0.94),
            (0.16, 0.94),
        ]),
    ),
    Shape::line(
        Ink::Current,
        0.075,
        Geom::Polyline(&[(0.60, 0.06), (0.60, 0.30), (0.84, 0.30)]),
    ),
];

/// 目录(合):经典的带页签文件夹。
const FOLDER_CLOSED: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Polygon(&[
        (0.05, 0.20),
        (0.40, 0.20),
        (0.49, 0.32),
        (0.95, 0.32),
        (0.95, 0.84),
        (0.05, 0.84),
    ]),
)];

/// 目录(开):后板 + 向右倾的前板。
const FOLDER_OPEN: &[Shape] = &[
    Shape::fill(
        Ink::CurrentAlpha(0.55),
        Geom::Polygon(&[
            (0.05, 0.18),
            (0.40, 0.18),
            (0.49, 0.30),
            (0.88, 0.30),
            (0.88, 0.50),
            (0.05, 0.50),
        ]),
    ),
    Shape::fill(
        Ink::Current,
        Geom::Polygon(&[(0.05, 0.42), (0.98, 0.42), (0.84, 0.86), (0.05, 0.86)]),
    ),
];

/// 源码:`<` `>`。
const MARK_ANGLES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.065,
        Geom::Polyline(&[(0.40, 0.52), (0.31, 0.64), (0.40, 0.76)]),
    ),
    Shape::line(
        Ink::Current,
        0.065,
        Geom::Polyline(&[(0.60, 0.52), (0.69, 0.64), (0.60, 0.76)]),
    ),
];

/// 结构化数据:`{` `}`。
const MARK_BRACES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Polyline(&[(0.42, 0.50), (0.34, 0.56), (0.34, 0.62), (0.28, 0.65), (0.34, 0.68), (0.34, 0.74), (0.42, 0.80)]),
    ),
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Polyline(&[(0.58, 0.50), (0.66, 0.56), (0.66, 0.62), (0.72, 0.65), (0.66, 0.68), (0.66, 0.74), (0.58, 0.80)]),
    ),
];

/// 文本:三条横线。
const MARK_LINES: &[Shape] = &[
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.30, 0.54), (0.70, 0.54)])),
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.30, 0.66), (0.70, 0.66)])),
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.30, 0.78), (0.56, 0.78)])),
];

/// 表格。
const MARK_GRID: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.055,
        Geom::Rect {
            x: 0.28,
            y: 0.52,
            w: 0.44,
            h: 0.30,
            round: 0.0,
        },
    ),
    Shape::line(Ink::Current, 0.055, Geom::Polyline(&[(0.50, 0.52), (0.50, 0.82)])),
    Shape::line(Ink::Current, 0.055, Geom::Polyline(&[(0.28, 0.67), (0.72, 0.67)])),
];

/// 样式:水滴。
const MARK_DROP: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Polygon(&[(0.50, 0.48), (0.68, 0.70), (0.60, 0.83), (0.40, 0.83), (0.32, 0.70)]),
)];

/// 图片:山 + 日。
const MARK_IMAGE: &[Shape] = &[
    Shape::fill(Ink::Current, Geom::Circle { c: (0.38, 0.58), r: 0.055 }),
    Shape::fill(
        Ink::Current,
        Geom::Polygon(&[(0.28, 0.82), (0.46, 0.62), (0.58, 0.74), (0.66, 0.66), (0.76, 0.82)]),
    ),
];

/// 视频:播放三角。
const MARK_PLAY: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Polygon(&[(0.40, 0.52), (0.72, 0.68), (0.40, 0.84)]),
)];

/// 音频:音符。
const MARK_NOTE: &[Shape] = &[
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.44, 0.80), (0.44, 0.50), (0.70, 0.44), (0.70, 0.74)])),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.38, 0.80), r: 0.075 }),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.64, 0.74), r: 0.075 }),
];

/// 归档:拉链。
const MARK_ZIP: &[Shape] = &[
    Shape::line(Ink::Current, 0.07, Geom::Polyline(&[(0.44, 0.50), (0.56, 0.50)])),
    Shape::line(Ink::Current, 0.07, Geom::Polyline(&[(0.44, 0.62), (0.56, 0.62)])),
    Shape::line(Ink::Current, 0.07, Geom::Polyline(&[(0.44, 0.74), (0.56, 0.74)])),
];

/// 二进制:芯片。
const MARK_CHIP: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Rect {
            x: 0.32,
            y: 0.52,
            w: 0.36,
            h: 0.30,
            round: 0.04,
        },
    ),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.42, 0.46), (0.42, 0.52)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.58, 0.46), (0.58, 0.52)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.42, 0.82), (0.42, 0.88)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.58, 0.82), (0.58, 0.88)])),
];

/// 字体:一个「T」形。
const MARK_TYPE: &[Shape] = &[
    Shape::line(Ink::Current, 0.07, Geom::Polyline(&[(0.34, 0.52), (0.66, 0.52)])),
    Shape::line(Ink::Current, 0.07, Geom::Polyline(&[(0.50, 0.52), (0.50, 0.82)])),
];

/// 数据库:三张碟。
const MARK_DISCS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.055,
        Geom::Ellipse {
            c: (0.50, 0.56),
            r: (0.20, 0.075),
            tilt: 0.0,
        },
    ),
    Shape::line(Ink::Current, 0.055, Geom::Polyline(&[(0.30, 0.56), (0.30, 0.78)])),
    Shape::line(Ink::Current, 0.055, Geom::Polyline(&[(0.70, 0.56), (0.70, 0.78)])),
    Shape::line(
        Ink::Current,
        0.055,
        Geom::Arc {
            c: (0.50, 0.78),
            r: 0.20,
            from: 0.0,
            sweep: 180.0,
        },
    ),
];

/// 壳 / 容器:`>_`。
const MARK_PROMPT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Polyline(&[(0.32, 0.54), (0.45, 0.66), (0.32, 0.78)]),
    ),
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.52, 0.80), (0.70, 0.80)])),
];

/// 锁文件:挂锁。
const MARK_LOCK: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Arc {
            c: (0.50, 0.63),
            r: 0.13,
            from: 180.0,
            sweep: 180.0,
        },
    ),
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Rect {
            x: 0.33,
            y: 0.63,
            w: 0.34,
            h: 0.24,
            round: 0.05,
        },
    ),
];

/// git:分支。
const MARK_BRANCH: &[Shape] = &[
    Shape::line(Ink::Current, 0.055, Geom::Polyline(&[(0.36, 0.52), (0.36, 0.84)])),
    Shape::line(
        Ink::Current,
        0.055,
        Geom::Polyline(&[(0.64, 0.60), (0.64, 0.66), (0.36, 0.72)]),
    ),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.36, 0.50), r: 0.065 }),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.36, 0.86), r: 0.065 }),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.64, 0.57), r: 0.065 }),
];

/// 配置:齿轮。
const MARK_GEAR: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Circle {
            c: (0.50, 0.68),
            r: 0.12,
        },
    ),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.50, 0.48), (0.50, 0.56)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.50, 0.80), (0.50, 0.88)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.30, 0.68), (0.38, 0.68)])),
    Shape::line(Ink::Current, 0.05, Geom::Polyline(&[(0.62, 0.68), (0.70, 0.68)])),
];

/// 证书 / 密钥:钥匙。
const MARK_KEY: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.06,
        Geom::Circle {
            c: (0.38, 0.60),
            r: 0.10,
        },
    ),
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.45, 0.67), (0.70, 0.84)])),
    Shape::line(Ink::Current, 0.06, Geom::Polyline(&[(0.60, 0.74), (0.54, 0.82)])),
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    let mut out = vec![FILE_SHEET, FOLDER_CLOSED, FOLDER_OPEN];
    for kind in ALL_FILE_KINDS {
        out.push(kind.mark());
    }
    out
}

/// 全部类别(遍历/演示用)。
pub const ALL_FILE_KINDS: &[FileKind] = &[
    FileKind::Directory,
    FileKind::DirectoryOpen,
    FileKind::Rust,
    FileKind::Go,
    FileKind::Python,
    FileKind::Java,
    FileKind::Kotlin,
    FileKind::Swift,
    FileKind::CFamily,
    FileKind::CSharp,
    FileKind::Ruby,
    FileKind::Php,
    FileKind::Lua,
    FileKind::Dart,
    FileKind::Scala,
    FileKind::Haskell,
    FileKind::Elixir,
    FileKind::Zig,
    FileKind::JavaScript,
    FileKind::TypeScript,
    FileKind::ReactScript,
    FileKind::Vue,
    FileKind::Svelte,
    FileKind::Html,
    FileKind::Xml,
    FileKind::Css,
    FileKind::Sass,
    FileKind::Json,
    FileKind::Yaml,
    FileKind::Toml,
    FileKind::Ini,
    FileKind::Csv,
    FileKind::Env,
    FileKind::Markdown,
    FileKind::Text,
    FileKind::Pdf,
    FileKind::Image,
    FileKind::Video,
    FileKind::Audio,
    FileKind::Archive,
    FileKind::Binary,
    FileKind::Font,
    FileKind::Database,
    FileKind::Sql,
    FileKind::Shell,
    FileKind::PowerShell,
    FileKind::Lock,
    FileKind::Git,
    FileKind::Config,
    FileKind::Docker,
    FileKind::Certificate,
    FileKind::Log,
    FileKind::Unknown,
];

/// 文件树用的图标。
///
/// ```ignore
/// FileIcon::new(&entry.name, entry.is_dir, expanded).size(px(14.0))
/// ```
#[derive(IntoElement)]
pub struct FileIcon {
    kind: FileKind,
    size: Pixels,
    color: Option<Hsla>,
}

impl FileIcon {
    /// 默认 14px —— 与原版 `w-3.5 h-3.5` 一致。
    pub fn new(name: &str, is_dir: bool, is_open: bool) -> Self {
        Self::of_kind(FileKind::of(name, is_dir, is_open))
    }

    pub fn of_kind(kind: FileKind) -> Self {
        Self {
            kind,
            size: px(14.0),
            color: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 覆盖主色 —— git 状态着色(新增绿 / 修改黄)就走这里。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }
}

impl RenderOnce for FileIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        VectorIcon::new(self.kind.outline(), self.size)
            .overlay(self.kind.mark())
            .ink(self.color.unwrap_or_else(|| self.kind.color()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(name: &str) -> FileKind {
        FileKind::of(name, false, false)
    }

    #[test]
    fn 目录两态() {
        assert_eq!(FileKind::of("src", true, false), FileKind::Directory);
        assert_eq!(FileKind::of("src", true, true), FileKind::DirectoryOpen);
        // 目录不看扩展名:`build.gradle/` 这种目录名也不能被判成 java
        assert_eq!(FileKind::of("build.gradle", true, false), FileKind::Directory);
    }

    #[test]
    fn 特殊文件名压过扩展名() {
        // 这条是 Material Icon Theme 的核心语义,顺序错了整张表就废了
        assert_eq!(kind("Cargo.lock"), FileKind::Lock);
        assert_eq!(kind("Cargo.toml"), FileKind::Toml);
        assert_eq!(kind("package-lock.json"), FileKind::Lock);
        assert_eq!(kind("package.json"), FileKind::Json);
        assert_eq!(kind("go.sum"), FileKind::Lock);
        assert_eq!(kind("go.mod"), FileKind::Go);
        assert_eq!(kind("tsconfig.json"), FileKind::Config);
        assert_eq!(kind("build.gradle.kts"), FileKind::Kotlin);
    }

    #[test]
    fn 大小写不敏感() {
        assert_eq!(kind("DOCKERFILE"), FileKind::Docker);
        assert_eq!(kind("Makefile"), FileKind::Config);
        assert_eq!(kind("README.MD"), FileKind::Markdown);
        assert_eq!(kind("Main.RS"), FileKind::Rust);
    }

    #[test]
    fn 点开头的裸文件不当成扩展名() {
        // `.gitignore` 的 "gitignore" 不是扩展名
        assert_eq!(kind(".gitignore"), FileKind::Git);
        assert_eq!(kind(".editorconfig"), FileKind::Config);
        assert_eq!(kind(".env"), FileKind::Env);
        assert_eq!(kind(".env.production"), FileKind::Env);
        // 没登记的点开头文件回落 Unknown 而不是被当成扩展名
        assert_eq!(kind(".unknownrc"), FileKind::Unknown);
        assert_eq!(extension_of(".gitignore"), None);
        assert_eq!(extension_of("a.rs"), Some("rs"));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of("trailing."), None);
    }

    #[test]
    fn 前缀规则兜住变体() {
        assert_eq!(kind("Dockerfile.dev"), FileKind::Docker);
        assert_eq!(kind("docker-compose.override.yml"), FileKind::Docker);
        assert_eq!(kind("LICENSE.txt"), FileKind::Text);
    }

    #[test]
    fn 扩展名覆盖面() {
        for (name, expect) in [
            ("main.rs", FileKind::Rust),
            ("main.go", FileKind::Go),
            ("app.tsx", FileKind::ReactScript),
            ("index.ts", FileKind::TypeScript),
            ("index.js", FileKind::JavaScript),
            ("App.vue", FileKind::Vue),
            ("page.svelte", FileKind::Svelte),
            ("style.scss", FileKind::Sass),
            ("theme.css", FileKind::Css),
            ("data.yaml", FileKind::Yaml),
            ("notes.md", FileKind::Markdown),
            ("photo.PNG", FileKind::Image),
            ("clip.mp4", FileKind::Video),
            ("song.flac", FileKind::Audio),
            ("bundle.tar.gz", FileKind::Archive),
            ("mt.exe", FileKind::Binary),
            ("Inter.woff2", FileKind::Font),
            ("app.sqlite3", FileKind::Database),
            ("seed.sql", FileKind::Sql),
            ("deploy.sh", FileKind::Shell),
            ("run.ps1", FileKind::PowerShell),
            ("server.pem", FileKind::Certificate),
            ("build.log", FileKind::Log),
            ("fix.patch", FileKind::Git),
            ("whatever.qqq", FileKind::Unknown),
        ] {
            assert_eq!(kind(name), expect, "{name}");
        }
    }

    #[test]
    fn 表里没有重复键() {
        // 重复键会让「后面那条永远查不到」,是最容易悄悄写错的一类
        for table in [EXACT_NAMES, EXTENSIONS] {
            let mut seen: Vec<&str> = table.iter().map(|(k, _)| *k).collect();
            let total = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), total, "表里有重复键");
        }
        // 键必须已经是小写,否则 lookup 永远不命中
        for table in [EXACT_NAMES, EXTENSIONS] {
            for (k, _) in table {
                assert_eq!(*k, k.to_ascii_lowercase(), "键 {k} 不是小写");
            }
        }
    }

    #[test]
    fn 每个类别都有轮廓() {
        for k in ALL_FILE_KINDS {
            assert!(!k.outline().is_empty(), "{k:?} 没有轮廓");
            assert!(k.color().a > 0.0);
        }
    }
}
