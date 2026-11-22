//! 文件编辑器语法高亮的补充语言包。
//!
//! gpui-component 的 `tree-sitter-languages` feature 给了 30 种语言,但其中
//! C# / Swift / CMake / Proto / GraphQL 五种**只挂了解析器、高亮查询是空串**
//! (`highlighter/languages.rs` 里 `(tree_sitter_c_sharp::LANGUAGE, "", "", "")`),
//! 打开 .cs 文件整篇是纯文本色;PHP / Kotlin / Lua / PowerShell / XML 这些常见类型
//! 组件库根本没有。
//!
//! 出口只有一个:[`register`],启动时调一次,往 `LanguageRegistry` 单例里
//! (a) 用带高亮查询的配置**覆盖**上面五种;(b) **追加**九种主流语言。注册表按
//! 名字查找,`InputState::code_editor(name)` 那条路一字不改;[`crate::file_viewer::language_for`]
//! 负责扩展名 → 名字。
//!
//! # 高亮查询从哪来
//!
//! 优先用语法 crate 自带的 `HIGHLIGHTS_QUERY`(与解析器同版本发布,节点名必定对得上);
//! crate 没带的(C# / Swift / Proto / GraphQL)从 Zed 的语言扩展或 nvim-treesitter 抄一份
//! 进 `assets/syntax/`,文件头注明来源与许可证(都是 Apache-2.0 / MIT),GPL 的一律不碰;
//! Groovy 那份是拿 Java 的查询拼的(见文件头)。
//!
//! # 捕获名要翻译成 Zed 的那套
//!
//! 主题只认 `highlighter/registry.rs::HIGHLIGHT_NAMES` 那 40 个名字(Zed 的口径),
//! 带点的名字回退到第一段(`keyword.modifier` → `keyword`)。各家语法 crate 的查询
//! 多按 nvim-treesitter 的口径写(`@field` / `@method` / `@conditional` / `@float` …),
//! 不翻译就整片没颜色。[`zed_captures`] 做这一步文本级改写,映射表见 [`CAPTURE_ALIASES`]。
//!
//! `#lua-match?` 是 nvim 私有谓词,tree-sitter 会当成「一般谓词」原样收下但**从不求值**
//! —— 模式于是无条件命中(CMake 会把所有标识符都涂成常量色)。[`zed_captures`] 顺手
//! 把它改成 `#match?`,Lua 字符类翻成正则。
//!
//! # 「兜底 @variable」要挪到最后
//!
//! 组件库的合并规则(`highlighter.rs::match_styles`):同一节点被多个模式捕获时,
//! **先到的名字赢**;而 tree-sitter 对同一节点的单节点模式按模式顺序出结果。nvim 风格
//! 的查询把 `(identifier) @variable` 放最前、靠「后者覆盖前者」让带谓词的特化模式
//! (`((identifier) @constant (#match? "^[A-Z_]+$"))`)生效 —— 在组件库里正好反过来,
//! 常量色永远出不来。[`demote_catch_alls`] 把这类兜底模式整条挪到查询末尾,两种
//! 风格的查询在组件库里都对(上游在 gpui-component 0.10.0 修复了 `ByteChunks` 谓词 bug
//! 后,带谓词的特化模式现已正常生效)。
//!
//! # 历史记档:0.5.1 的 `ByteChunks` 文本谓词 bug(0.10.0 已修复)
//!
//! 旧版 gpui-component 0.5.1 里 `highlighter.rs` 给 tree-sitter 的 `TextProvider`
//! 使用 ropey 的 `ChunkCursor`,但 `ByteChunks::next` 先 `cursor.next()` 再取 chunk,
//! 导致 `#eq?` / `#match?` / `#any-of?` 拿不到正确的节点文本、带谓词的模式大面积失效。
//! gpui-component 0.10.0 上游已修复该 bug,带谓词的模式现在可正常命中。
//!
//! `#lua-match?` 仍需翻译成 `#match?`:tree-sitter 本身把 `#lua-match?` 当作未求值的
//! 一般谓词无条件放行(CMake 会把所有变量涂成常量色),翻成 `#match?` 才能按正则求值。
//!
//! # 注入只支持 `#set! injection.language`
//!
//! 组件库的注入解析(`highlighter.rs::injection_for_match`)只认 `#set!` 写死的语言名,
//! `@injection.language` 捕获那条路被注释掉了。PHP 的 HTML 段是写死的,够用;
//! heredoc 之类动态注入不做。
//!
//! # 挑选口径
//!
//! - **只收主流类型**(用户 2026-09-17 定的):每个语法 crate 都是一份 `cc` 编译的
//!   parser.c,进二进制几百 KB 到几 MB,冷门语言不值这个价。现在是 PHP / Kotlin / Lua /
//!   PowerShell / XML / Dart / Groovy / INI / 批处理九种;Haskell / OCaml / HCL / Nix / R /
//!   Erlang / F# / Pascal / Svelte / nginx / Jinja2 这些曾经接过、按这条口径又撤了,
//!   要加回来只是 Cargo.toml 一行 + 这张表一项(crates.io 上都有带查询的 crate,
//!   HCL / Pascal / nginx 的查询要从 nvim-treesitter 或 crate 的 queries/ 目录抄)
//! - 只要依赖 `tree-sitter-language 0.1` 的 crate。老一代 crate 直接依赖
//!   `tree-sitter = "0.20"~"0.22"`(dockerfile / fish / scss / vue / vim / json5),
//!   会把第二份 tree-sitter 运行时链进来、C 符号重定义 —— 这些类型退到近似语言
//!   (Dockerfile → bash,scss → css,vue → html),映射在 `language_for`
//! - 没有高亮查询也没处抄的(nim / crystal / clojure / kotlin-ng)不要
use std::sync::Once;

use gpui_component::highlighter::{LanguageConfig, LanguageRegistry};
use tree_sitter_language::LanguageFn;

/// 一种补充语言:注册表键名 + 解析器 + 查询 + 注入目标。
struct Pack {
    /// 注册表里的名字,也是 [`crate::file_viewer::language_for`] 返回的字符串。
    name: &'static str,
    /// 语法 crate 导出的解析器。
    language: LanguageFn,
    /// 高亮查询原文,注册前经 [`zed_captures`] + [`demote_catch_alls`] 改写。
    highlights: &'static str,
    /// 注入查询(见模块注释「注入只支持 `#set!`」)。
    injections: &'static str,
    /// 注入目标语言名,注册表要能查到(组件库内建的名字即可)。
    injection_languages: &'static [&'static str],
}

impl Pack {
    const fn plain(name: &'static str, language: LanguageFn, highlights: &'static str) -> Self {
        Self {
            name,
            language,
            highlights,
            injections: "",
            injection_languages: &[],
        }
    }

    fn config(&self) -> LanguageConfig {
        let highlights = demote_catch_alls(&zed_captures(self.highlights));
        LanguageConfig::new(
            self.name,
            self.language.into(),
            self.injection_languages
                .iter()
                .map(|s| (*s).into())
                .collect(),
            &highlights,
            self.injections,
            "",
        )
    }
}

/// 全部语言包。名字与组件库内建重名的(csharp / swift / cmake / proto / graphql)
/// 是覆盖,其余是追加。
const PACKS: &[Pack] = &[
    // ---- 组件库有解析器、缺高亮查询的五种 ----
    Pack::plain(
        "csharp",
        tree_sitter_c_sharp::LANGUAGE,
        include_str!("../assets/syntax/csharp.scm"),
    ),
    Pack::plain(
        "swift",
        tree_sitter_swift::LANGUAGE,
        include_str!("../assets/syntax/swift.scm"),
    ),
    Pack::plain(
        "cmake",
        tree_sitter_cmake::LANGUAGE,
        tree_sitter_cmake::HIGHLIGHTS_QUERY,
    ),
    Pack::plain(
        "proto",
        tree_sitter_proto::LANGUAGE,
        include_str!("../assets/syntax/proto.scm"),
    ),
    Pack::plain(
        "graphql",
        tree_sitter_graphql::LANGUAGE,
        include_str!("../assets/syntax/graphql.scm"),
    ),
    // ---- 新增(只收主流类型,冷门语言不进来,见模块注释「挑选口径」)----
    // php / lua 曾在这里补过:gpui-component 0.6 起内置(php 带 HTML 注入、lua 带
    // injections + locals),不再覆盖 —— 再引 tree-sitter-lua 会与组件库那份版本
    // 不同、C 符号重定义链接失败。
    //
    // kotlin 0.6 也内置了,但它那份 highlights.scm 在自己拉的 kotlin-sg 0.4.1 上
    // **编不过**(`Invalid node type "!is"`),组件库只 warn 后静默退成纯文本 ——
    // 这里继续用 crate 自带的 nvim 派生查询覆盖(kotlin-sg 是 fwcd 那份语法的
    // crates.io 发布版;tree-sitter-grammars 的 kotlin-ng 是重写过的语法,节点名
    // 不同,nvim / Zed / Helix 三家的查询都对不上它,没处抄)。
    Pack::plain(
        "kotlin",
        tree_sitter_kotlin_sg::LANGUAGE,
        tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY,
    ),
    Pack::plain(
        "powershell",
        tree_sitter_powershell::LANGUAGE,
        tree_sitter_powershell::HIGHLIGHTS_QUERY,
    ),
    // csproj / xaml / plist / svg 这一大家子都是它
    Pack::plain(
        "xml",
        tree_sitter_xml::LANGUAGE_XML,
        tree_sitter_xml::XML_HIGHLIGHT_QUERY,
    ),
    Pack::plain(
        "dart",
        tree_sitter_dart::LANGUAGE,
        tree_sitter_dart::HIGHLIGHTS_QUERY,
    ),
    // 主要为 build.gradle / Jenkinsfile
    Pack::plain(
        "groovy",
        tree_sitter_groovy::LANGUAGE,
        include_str!("../assets/syntax/groovy.scm"),
    ),
    // .ini / .cfg / .conf / .editorconfig / .gitconfig / .properties 都是 key=value
    Pack::plain(
        "ini",
        tree_sitter_ini::LANGUAGE,
        tree_sitter_ini::HIGHLIGHTS_QUERY,
    ),
    Pack::plain(
        "batch",
        tree_sitter_batch::LANGUAGE,
        tree_sitter_batch::HIGHLIGHTS_QUERY,
    ),
];

/// 把全部语言包注册进组件库的 `LanguageRegistry` 单例。幂等,启动时调一次。
///
/// 只是往 HashMap 里放几十份字符串 + 解析器句柄,查询真正编译发生在编辑器建
/// 高亮器那一刻(`SyntaxHighlighter::new`),这里毫秒级,不必丢后台 —— 丢了反而
/// 有「用户先于注册完成打开 .cs」的竞态。
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let registry = LanguageRegistry::singleton();
        for pack in PACKS {
            registry.register(pack.name, &pack.config());
        }
    });
}

/// nvim-treesitter / 各语法 crate 的捕获名 → Zed(组件库主题)的捕获名。
///
/// 只列**翻译后才有颜色**的;`keyword.conditional` 这种靠主题的「取第一段」回退
/// 已经能上色的不列。查找按「最长带点前缀」匹配:`markup.heading.1` 命中
/// `markup.heading`。
const CAPTURE_ALIASES: &[(&str, &str)] = &[
    // 变量 / 属性 / 参数
    ("field", "property"),
    ("variable.member", "property"),
    ("parameter", "variable.parameter"),
    ("variable.builtin", "variable.special"),
    ("identifier", "variable"),
    ("method", "function.method"),
    ("method.call", "function.method"),
    ("macro", "function"),
    // 字面量
    ("float", "number"),
    ("character", "string"),
    ("character.special", "string.special"),
    ("string.regexp", "string.regex"),
    ("escape", "string.escape"),
    ("symbol", "string.special.symbol"),
    // 老 nvim 口径的关键字细分
    ("conditional", "keyword"),
    ("repeat", "keyword"),
    ("include", "keyword"),
    ("exception", "keyword"),
    ("storageclass", "keyword"),
    ("type.qualifier", "keyword"),
    ("define", "preproc"),
    // Zed 没有 namespace/module 这一档,与 zed-extensions/csharp 一样落到 type
    ("namespace", "type"),
    ("module", "type"),
    ("comment.documentation", "comment.doc"),
    // 标记语言
    ("tag.attribute", "attribute"),
    ("tag.delimiter", "punctuation.bracket"),
    ("delimiter", "punctuation.delimiter"),
    ("text.title", "title"),
    ("text.emphasis", "emphasis"),
    ("text.strong", "emphasis.strong"),
    ("text.uri", "link_uri"),
    ("text.reference", "link_text"),
    ("markup.heading", "title"),
    ("markup.italic", "emphasis"),
    ("markup.bold", "emphasis.strong"),
    ("markup.strong", "emphasis.strong"),
    ("markup.raw", "text.literal"),
    ("markup.link.url", "link_uri"),
    ("markup.link.label", "link_text"),
    ("markup.link", "link_text"),
    ("markup.list", "punctuation.list_marker"),
];

/// 查一个捕获名的别名:先整名,再逐级去掉末段。查不到返回 `None`(原样保留)。
fn alias_for(name: &str) -> Option<&'static str> {
    let mut key = name;
    loop {
        if let Some((_, to)) = CAPTURE_ALIASES.iter().find(|(from, _)| *from == key) {
            return Some(to);
        }
        key = key.rsplit_once('.')?.0;
    }
}

/// 把查询里的捕获名改写成 Zed 口径,`#lua-match?` 改成 `#match?`(Lua 模式翻成正则),
/// 顺手删掉 `;` 注释(后面 [`demote_catch_alls`] 的切分不用再躲注释)。
///
/// 字符串字面量里的 `@` 原样保留(`"@interface"` 这类匹配文本不能动)。
pub fn zed_captures(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let bytes = query.as_bytes();
    let mut i = 0;
    // 刚写出 `#lua-match?`,下一个字符串字面量是 Lua 模式,要翻译
    let mut pending_lua_pattern = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b';' => {
                // 注释到行尾
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                let literal = &query[start..i];
                if pending_lua_pattern {
                    pending_lua_pattern = false;
                    out.push('"');
                    out.push_str(&lua_pattern_to_regex(literal.trim_matches('"')));
                    out.push('"');
                } else {
                    out.push_str(literal);
                }
            }
            b'#' => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                    i += 1;
                }
                let name = &query[start..i];
                if let Some(rest) = name.strip_suffix("lua-match") {
                    // `#lua-match` / `#not-lua-match` / `#any-lua-match`
                    out.push_str(rest);
                    out.push_str("match");
                    pending_lua_pattern = true;
                } else {
                    out.push_str(name);
                }
            }
            b'@' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'-'))
                {
                    i += 1;
                }
                let name = &query[start..i];
                out.push('@');
                out.push_str(alias_for(name).unwrap_or(name));
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Lua 模式 → Rust 正则。只覆盖高亮查询里实际出现的子集:`%a %d %l %u %w %s %x`
/// 字符类(裸的与在 `[...]` 里的)、`%` 转义标点、`-` 懒惰量词。
fn lua_pattern_to_regex(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len() + 8);
    let mut chars = pat.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let Some(n) = chars.next() else {
                    out.push_str("\\\\%");
                    break;
                };
                let class = |bare: &str, inner: &str| {
                    if in_class {
                        inner.to_string()
                    } else {
                        bare.to_string()
                    }
                };
                let s = match n {
                    'a' => class("[A-Za-z]", "A-Za-z"),
                    'd' => class("[0-9]", "0-9"),
                    'l' => class("[a-z]", "a-z"),
                    'u' => class("[A-Z]", "A-Z"),
                    'w' => class("[A-Za-z0-9]", "A-Za-z0-9"),
                    's' => class("\\\\s", "\\\\s"),
                    'x' => class("[0-9A-Fa-f]", "0-9A-Fa-f"),
                    // `%.` `%-` `%(` … 转义标点;查询源文本里正则的反斜杠要写成 `\\`
                    other => format!("\\\\{other}"),
                };
                out.push_str(&s);
            }
            '[' => {
                in_class = true;
                out.push(c);
            }
            ']' => {
                in_class = false;
                out.push(c);
            }
            '-' if !in_class => out.push_str("*?"),
            _ => out.push(c),
        }
    }
    out
}

/// 按顶层「项」重排查询:
///
/// 1. `(identifier) @variable` 这类「单节点、单捕获 `@variable`、无谓词」的兜底模式
///    整条挪到末尾(理由见模块注释);
/// 2. 单独捕获引号记号的模式(`[ "\"" "'" ] @punctuation.delimiter`)删掉引号项:
///    组件库 `match_styles` 会把**相邻同名**捕获连成一段(前一项的起点到后一项的终点),
///    开引号与闭引号一合并就盖住了整个字符串内容 —— XML 的属性值因此整个失色。
///    引号与字符串本来就该同色,删了没损失。
///
/// 输入应已去掉注释([`zed_captures`] 的产物)。
pub fn demote_catch_alls(query: &str) -> String {
    let mut kept = String::with_capacity(query.len());
    let mut demoted = String::new();
    for item in top_level_items(query) {
        if is_catch_all(item) {
            demoted.push_str(item);
            demoted.push('\n');
        } else if let Some(item) = strip_quote_tokens(item) {
            kept.push_str(&item);
            kept.push('\n');
        }
    }
    kept.push_str(&demoted);
    kept
}

/// 从 `[ ... ] @cap` 形式的项里删掉引号记号;删空了返回 `None`。非此形式原样返回。
fn strip_quote_tokens(item: &str) -> Option<String> {
    const QUOTES: [&str; 4] = [r#""\"""#, r#""'""#, r#""\"\"\"""#, r#""'''""#];
    let Some(body) = item.strip_prefix('[') else {
        return Some(item.to_string());
    };
    let Some((alts, tail)) = body.rsplit_once(']') else {
        return Some(item.to_string());
    };
    if !alts.split_whitespace().any(|t| QUOTES.contains(&t)) {
        return Some(item.to_string());
    }
    let kept: Vec<&str> = alts
        .split_whitespace()
        .filter(|t| !QUOTES.contains(t))
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(format!("[ {} ]{tail}", kept.join(" ")))
}

/// 顶层切分:一个「项」= 一组配平的括号 + 紧随其后的 `@捕获` / 量词。
fn top_level_items(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // 跳过项之间的空白
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut depth = 0i32;
        // 括号组;顶层也可以是裸的字符串字面量(`"xml" @keyword`)或裸记号
        loop {
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                b'(' | b'[' => {
                    depth += 1;
                    i += 1;
                }
                b')' | b']' => {
                    depth -= 1;
                    i += 1;
                    if depth <= 0 {
                        break;
                    }
                }
                _ => {
                    i += 1;
                    if depth == 0 {
                        // 顶层裸记号(`_ @x` 之类),到空白为止
                        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        break;
                    }
                }
            }
        }
        // 尾随的捕获与量词:`@name` / `?` / `*` / `+`,允许中间有空格
        loop {
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'@' {
                j += 1;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'_' | b'.' | b'-'))
                {
                    j += 1;
                }
                i = j;
            } else if j < bytes.len() && matches!(bytes[j], b'?' | b'*' | b'+') {
                i = j + 1;
            } else {
                break;
            }
        }
        items.push(query[start..i].trim_end());
    }
    items
}

/// `(node_type) @variable`,仅此形状。
fn is_catch_all(item: &str) -> bool {
    let Some(rest) = item.strip_prefix('(') else {
        return false;
    };
    let Some((node, tail)) = rest.split_once(')') else {
        return false;
    };
    let node = node.trim();
    !node.is_empty()
        && node.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && tail.trim() == "@variable"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 捕获名翻译成_zed_口径() {
        assert_eq!(zed_captures("(x) @field"), "(x) @property");
        assert_eq!(zed_captures("(x) @method.call"), "(x) @function.method");
        assert_eq!(
            zed_captures("(x) @keyword.conditional"),
            "(x) @keyword.conditional",
            "主题自己会回退到 keyword,不动"
        );
        assert_eq!(
            zed_captures("(x) @markup.heading.1"),
            "(x) @title",
            "最长带点前缀"
        );
        assert_eq!(zed_captures("(x) @spell"), "(x) @spell", "不认识的原样保留");
    }

    #[test]
    fn 字符串与注释里的_at_不动() {
        assert_eq!(
            zed_captures(r#"((x) @field (#eq? @field "@field"))"#),
            r#"((x) @property (#eq? @property "@field"))"#
        );
        assert_eq!(zed_captures("; @field 注释\n(x) @field"), "\n(x) @property");
    }

    #[test]
    fn lua_match_翻成_match() {
        assert_eq!(
            zed_captures(r#"((x) @constant (#lua-match? @constant "^[%u@][%u%d_]+$"))"#),
            r#"((x) @constant (#match? @constant "^[A-Z@][A-Z0-9_]+$"))"#
        );
        assert_eq!(
            zed_captures(r#"(#not-lua-match? @x "^gl_")"#),
            r#"(#not-match? @x "^gl_")"#
        );
        assert_eq!(
            lua_pattern_to_regex("^/[*][*][^*].*[*]/$"),
            "^/[*][*][^*].*[*]/$"
        );
        assert_eq!(lua_pattern_to_regex("%.py$"), "\\\\.py$", "转义标点");
        assert_eq!(lua_pattern_to_regex("a-b"), "a*?b", "懒惰量词");
    }

    #[test]
    fn 兜底模式挪到末尾() {
        let q = "(identifier) @variable\n((identifier) @constant (#match? @constant \"^[A-Z]+$\"))\n[\"if\" \"else\"] @keyword";
        let out = demote_catch_alls(q);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.last(), Some(&"(identifier) @variable"));
        assert_eq!(
            lines[0],
            "((identifier) @constant (#match? @constant \"^[A-Z]+$\"))"
        );
        assert_eq!(lines[1], "[\"if\" \"else\"] @keyword");
    }

    #[test]
    fn 引号记号从标点模式里删掉() {
        assert_eq!(
            strip_quote_tokens(r#"[ "\"" "'" "," ] @punctuation.delimiter"#).as_deref(),
            Some(r#"[ "," ] @punctuation.delimiter"#)
        );
        assert_eq!(
            strip_quote_tokens(r#"[ "\"" "'" ] @punctuation.delimiter"#),
            None,
            "删空整条丢掉"
        );
        assert_eq!(
            strip_quote_tokens(r#"[ "(" ")" ] @punctuation.bracket"#).as_deref(),
            Some(r#"[ "(" ")" ] @punctuation.bracket"#),
            "没有引号的原样"
        );
        assert_eq!(
            strip_quote_tokens("(x) @string").as_deref(),
            Some("(x) @string")
        );
        let out = demote_catch_alls("[ \"\\\"\" ] @punctuation.delimiter\n(a) @tag");
        assert_eq!(out.trim(), "(a) @tag");
    }

    #[test]
    fn 顶层切分认得多行模式与量词() {
        let q = "(a\n  (b) @x)\n(c)? @y\n(d) @variable";
        let items = top_level_items(q);
        assert_eq!(items, vec!["(a\n  (b) @x)", "(c)? @y", "(d) @variable"]);
        assert!(is_catch_all("(d) @variable"));
        assert!(!is_catch_all("(d) @variable @spell"));
        assert!(!is_catch_all("(d (e)) @variable"));
    }

    /// 每份查询都亲手编一遍:组件库那条路编不过只会 warn 后静默退成纯文本。
    #[test]
    fn 每份高亮查询都能编译() {
        let mut failed = Vec::new();
        for pack in PACKS {
            let config = pack.config();
            let source = format!("{}\n{}", config.injections, config.highlights);
            let language = config.language.as_ref().expect("语言包一定带解析器");
            if let Err(err) = tree_sitter::Query::new(language, &source) {
                failed.push(format!("{}: {err}", pack.name));
            }
        }
        assert!(failed.is_empty(), "编不过的查询:\n{}", failed.join("\n"));
    }

    /// 注入目标必须是注册表里有的名字,否则那段就没颜色。
    #[test]
    fn 注入目标语言都在注册表里() {
        register();
        let registry = LanguageRegistry::singleton();
        for pack in PACKS {
            for lang in pack.injection_languages {
                assert!(
                    registry.language(lang).is_some(),
                    "{}: 注入目标 {lang} 未注册",
                    pack.name
                );
            }
        }
    }

    /// 某段代码在某语言下的颜色,与主题里某个捕获名的颜色对账。
    #[track_caller]
    fn assert_colored_as(lang: &str, code: &str, needle: &str, capture: &str) {
        use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
        register();
        let mut highlighter = SyntaxHighlighter::new(lang);
        highlighter.update(None, &ropey::Rope::from_str(code), None);
        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..code.len()), &*theme);
        let at = code.find(needle).expect("靶子不在代码里");
        let (range, style) = styles
            .iter()
            .find(|(range, _)| range.start <= at && at < range.end)
            .expect("靶子位置没有任何样式区间");
        let expected = theme.style(capture).and_then(|s| s.color);
        assert!(
            expected.is_some(),
            "主题里 {capture} 没有颜色,这个对账靶子选错了"
        );
        assert_eq!(
            style.color, expected,
            "{lang}: `{needle}`({range:?})该是 {capture} 色"
        );
    }

    /// C# 是这次的由头:方法名 function 色、关键字 keyword 色、字符串 string 色。
    /// 顺带验「同一节点先到的名字赢」这条组件库规则下 Zed 的查询(兜底
    /// `(identifier) @variable` 在最前)仍然对。
    #[test]
    fn csharp_端到端上色() {
        let code =
            "using System;\nclass Program { static void Main() { Console.WriteLine(\"hi\"); } }";
        assert_colored_as("csharp", code, "Main", "function");
        assert_colored_as("csharp", code, "WriteLine", "function");
        assert_colored_as("csharp", code, "class", "keyword");
        assert_colored_as("csharp", code, "\"hi\"", "string");
        assert_colored_as("csharp", code, "Program", "type");
    }

    /// crate 自带的 nvim 风格查询经捕获名翻译后能上色:`this` 是 variable.special
    /// (`(this_expression) @variable.builtin` 翻过来的),关键字 / 类型 / 字符串各归各。
    /// 同时也是「组件库内置 kotlin 查询编不过、我们的覆盖仍然生效」的对账。
    #[test]
    fn kotlin_捕获名翻译后上色() {
        let code = "class A { fun f(xs: List<Int>) = this.g(xs, \"s\") }";
        assert_colored_as("kotlin", code, "this", "variable.special");
        assert_colored_as("kotlin", code, "fun", "keyword");
        assert_colored_as("kotlin", code, "List", "type");
        assert_colored_as("kotlin", code, "\"s\"", "string");
    }

    /// XML 的属性值:引号记号删掉之前,开闭引号两个 punctuation 捕获被组件库连成一段、
    /// 盖掉整个 `(AttValue) @string`,csproj 里的 `Sdk="Microsoft.NET.Sdk"` 是黑的。
    #[test]
    fn xml_属性值是_string_色() {
        let code = "<Project Sdk=\"Microsoft.NET.Sdk\"><!-- c --><A>x</A></Project>";
        assert_colored_as("xml", code, "\"Microsoft.NET.Sdk\"", "string");
        assert_colored_as("xml", code, "Project", "tag");
        assert_colored_as("xml", code, "Sdk", "property");
        assert_colored_as("xml", code, "<!-- c -->", "comment");
    }

    /// CMake 是组件库「有解析器没查询」的五种之一,补上后命令名 / 字符串有颜色。
    #[test]
    fn cmake_补上查询后上色() {
        let code = "set(MY_FLAG ON)\nmessage(STATUS \"x\")";
        assert_colored_as("cmake", code, "set", "function");
        assert_colored_as("cmake", code, "\"x\"", "string");
    }

    #[test]
    fn 注册后按名字都查得到且带高亮查询() {
        register();
        let registry = LanguageRegistry::singleton();
        for pack in PACKS {
            let config = registry.language(pack.name).expect(pack.name);
            assert!(
                !config.highlights.is_empty(),
                "{} 的高亮查询是空的",
                pack.name
            );
        }
    }
}
