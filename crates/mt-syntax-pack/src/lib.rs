//! 语法高亮扩展包:把体积大的 tree-sitter 语法编进一个动态库(`mt_syntax_pack.dll` /
//! `libmt_syntax_pack.dylib` / `libmt_syntax_pack.so`),作为扩展 `syntax-pack` 按需下载。
//!
//! 导出唯一的 C 入口 [`mt_syntax_pack_entry`](符号名见 `mt_syntax_pack_abi::ENTRY_SYMBOL`),
//! 返回一张静态表:每种语言的解析器函数指针 + 高亮 / 注入 / locals 查询原文 + 处理标志。
//! 表结构与语言清单在 `mt-syntax-pack-abi`,宿主侧的加载器在 mt-app `syntax_pack.rs`。
//!
//! # 查询从哪来
//!
//! - 组件库(gpui-component 0.10.0)来源的七种:配置照抄 `highlighter/languages.rs`,
//!   组件库自带的查询文件照抄进 `queries/`(Apache-2.0,文件头注明来源),其余用语法 crate
//!   自带的常量。已是 Zed 口径,不置 `ZED_REWRITE`。
//! - 原 mt-app `syntax_languages.rs` 来源的九种:连同 `assets/syntax/*.scm` 一起挪来
//!   (各文件头有原始出处与许可证)。都是 nvim 口径,置 `ZED_REWRITE`,宿主注册前改写。
//!
//! # 分配器
//!
//! 语法 crate 的 build.rs **都没有**定义 `TREE_SITTER_REUSE_ALLOCATOR`(只在各自的
//! `src/tree_sitter/alloc.h` 里 `#ifdef` 引用),外部扫描器于是直接用 C 运行时的
//! `malloc` / `free`,不引用 `ts_current_malloc` 等宿主符号;扫描器状态的分配与释放
//! (`create` / `destroy`)都在本库内配对,不跨模块释放。

// MSVC 链接 cdylib 时 stdout 必报一句「正在创建库 ….dll.lib 和对象 ….exp」(导出了
// 入口符号就会生成导入库),rustc 把它当 `linker_messages` 警告转出来,无害。
#![allow(linker_messages)]

use mt_syntax_pack_abi::{
    ABI_VERSION, AbiStr, AbiStrList, LanguageEntry, PackTable, flags::ZED_REWRITE,
};
use tree_sitter_language::LanguageFn;

const fn entry(
    name: &'static str,
    language: LanguageFn,
    highlights: &'static str,
    injections: &'static str,
    locals: &'static str,
    injection_languages: AbiStrList,
    flags: u32,
) -> LanguageEntry {
    LanguageEntry {
        name: AbiStr::new(name),
        language: language.into_raw(),
        highlights: AbiStr::new(highlights),
        injections: AbiStr::new(injections),
        locals: AbiStr::new(locals),
        injection_languages,
        flags,
    }
}

/// nvim 口径、只有高亮查询的一种(原 syntax_languages 的 `Pack::plain`)。
const fn rewrite(
    name: &'static str,
    language: LanguageFn,
    highlights: &'static str,
) -> LanguageEntry {
    entry(
        name,
        language,
        highlights,
        "",
        "",
        AbiStrList::EMPTY,
        ZED_REWRITE,
    )
}

/// 组件库 `Language::injection_languages` 里 PHP 那一组(组件库按 feature 裁剪,
/// 这里全列:html / css / javascript / jsdoc 是宿主内置,graphql 在本包)。
static PHP_INJECTION_LANGUAGES: [AbiStr; 7] = [
    AbiStr::new("php"),
    AbiStr::new("html"),
    AbiStr::new("css"),
    AbiStr::new("javascript"),
    AbiStr::new("json"),
    AbiStr::new("jsdoc"),
    AbiStr::new("graphql"),
];

/// 顺序与 `mt_syntax_pack_abi::LANGUAGES` 一致(单测对账)。
static ENTRIES: [LanguageEntry; 16] = [
    // ---- 组件库 0.10.0 来源(`highlighter/languages.rs` 的 `Language::config`)----
    entry(
        "cpp",
        tree_sitter_cpp::LANGUAGE,
        tree_sitter_cpp::HIGHLIGHT_QUERY,
        "",
        "",
        AbiStrList::EMPTY,
        0,
    ),
    entry(
        "elixir",
        tree_sitter_elixir::LANGUAGE,
        tree_sitter_elixir::HIGHLIGHTS_QUERY,
        tree_sitter_elixir::INJECTIONS_QUERY,
        "",
        AbiStrList::EMPTY,
        0,
    ),
    entry(
        "php",
        tree_sitter_php::LANGUAGE_PHP,
        tree_sitter_php::HIGHLIGHTS_QUERY,
        include_str!("../queries/php/injections.scm"),
        "",
        AbiStrList::new(&PHP_INJECTION_LANGUAGES),
        0,
    ),
    entry(
        "ruby",
        tree_sitter_ruby::LANGUAGE,
        tree_sitter_ruby::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_ruby::LOCALS_QUERY,
        AbiStrList::EMPTY,
        0,
    ),
    entry(
        "scala",
        tree_sitter_scala::LANGUAGE,
        tree_sitter_scala::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_scala::LOCALS_QUERY,
        AbiStrList::EMPTY,
        0,
    ),
    entry(
        "sql",
        tree_sitter_sequel::LANGUAGE,
        tree_sitter_sequel::HIGHLIGHTS_QUERY,
        "",
        "",
        AbiStrList::EMPTY,
        0,
    ),
    entry(
        "zig",
        tree_sitter_zig::LANGUAGE,
        include_str!("../queries/zig/highlights.scm"),
        include_str!("../queries/zig/injections.scm"),
        "",
        AbiStrList::EMPTY,
        0,
    ),
    // ---- 原 mt-app syntax_languages.rs 来源(nvim 口径,宿主改写)----
    rewrite(
        "csharp",
        tree_sitter_c_sharp::LANGUAGE,
        include_str!("../queries/csharp.scm"),
    ),
    rewrite(
        "swift",
        tree_sitter_swift::LANGUAGE,
        include_str!("../queries/swift.scm"),
    ),
    rewrite(
        "cmake",
        tree_sitter_cmake::LANGUAGE,
        tree_sitter_cmake::HIGHLIGHTS_QUERY,
    ),
    rewrite(
        "proto",
        tree_sitter_proto::LANGUAGE,
        include_str!("../queries/proto.scm"),
    ),
    rewrite(
        "graphql",
        tree_sitter_graphql::LANGUAGE,
        include_str!("../queries/graphql.scm"),
    ),
    // 组件库内置的 kotlin 查询在 kotlin-sg 0.4.1 上编不过(`Invalid node type "!is"`),
    // 一直用 crate 自带的 nvim 派生查询(见 mt-app syntax_languages.rs 的历史注释)。
    rewrite(
        "kotlin",
        tree_sitter_kotlin_sg::LANGUAGE,
        tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY,
    ),
    rewrite(
        "powershell",
        tree_sitter_powershell::LANGUAGE,
        tree_sitter_powershell::HIGHLIGHTS_QUERY,
    ),
    rewrite(
        "dart",
        tree_sitter_dart::LANGUAGE,
        tree_sitter_dart::HIGHLIGHTS_QUERY,
    ),
    // 主要为 build.gradle / Jenkinsfile
    rewrite(
        "groovy",
        tree_sitter_groovy::LANGUAGE,
        include_str!("../queries/groovy.scm"),
    ),
];

static TABLE: PackTable = PackTable {
    abi_version: ABI_VERSION,
    entry_size: size_of::<LanguageEntry>() as u32,
    languages: ENTRIES.as_ptr(),
    languages_len: ENTRIES.len(),
};

/// C 入口:返回静态表(永不为空)。
#[unsafe(no_mangle)]
pub extern "C" fn mt_syntax_pack_entry() -> *const PackTable {
    &TABLE
}

/// 同一张表的 Rust 入口(rlib 用户 —— mt-app 的单测 —— 不经动态库直接拿)。
pub fn table() -> &'static PackTable {
    &TABLE
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_syntax_pack_abi::{ENTRY_SYMBOL, LANGUAGES};

    fn text(s: AbiStr) -> &'static str {
        if s.len == 0 {
            return "";
        }
        // SAFETY: 表里的 AbiStr 全由 `AbiStr::new(&'static str)` 构造。
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
        std::str::from_utf8(bytes).expect("UTF-8")
    }

    fn list(l: AbiStrList) -> Vec<&'static str> {
        if l.len == 0 {
            return Vec::new();
        }
        // SAFETY: 同上,指向静态数组。
        unsafe { std::slice::from_raw_parts(l.ptr, l.len) }
            .iter()
            .map(|s| text(*s))
            .collect()
    }

    fn language(e: &LanguageEntry) -> tree_sitter::Language {
        // SAFETY: 函数指针来自语法 crate 的 `LANGUAGE` 常量。
        tree_sitter::Language::new(unsafe { LanguageFn::from_raw(e.language) })
    }

    #[test]
    fn 表头与_abi_版本一致() {
        let table = table();
        assert_eq!(table.abi_version, ABI_VERSION);
        assert_eq!(table.entry_size as usize, size_of::<LanguageEntry>());
        assert_eq!(table.languages_len, ENTRIES.len());
        assert!(std::ptr::eq(mt_syntax_pack_entry(), table));
        assert_eq!(
            ENTRY_SYMBOL, "mt_syntax_pack_entry",
            "导出函数名与 ABI 常量对账"
        );
    }

    #[test]
    fn 表与_abi_语言清单逐条对账() {
        let names: Vec<_> = ENTRIES.iter().map(|e| text(e.name)).collect();
        let expected: Vec<_> = LANGUAGES.iter().map(|l| l.name).collect();
        assert_eq!(names, expected, "顺序与名字都要一致");
        for (entry, lang) in ENTRIES.iter().zip(LANGUAGES) {
            assert_eq!(entry.flags, lang.flags, "{}: 标志位", lang.name);
        }
    }

    #[test]
    fn 每种语言的_abi_版本宿主认得() {
        for e in &ENTRIES {
            let version = language(e).abi_version();
            assert!(
                (tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
                    .contains(&version),
                "{}: ABI {version}",
                text(e.name)
            );
        }
    }

    /// 每份查询原文都亲手编一遍(组件库那条路编不过只会 warn 后静默退成纯文本)。
    /// `ZED_REWRITE` 的那几份在宿主改写后还要再编一遍,见 mt-app `syntax_pack` 的单测。
    #[test]
    fn 每份查询原文都能编译() {
        let mut failed = Vec::new();
        for e in &ENTRIES {
            assert!(
                !text(e.highlights).is_empty(),
                "{}: 高亮查询为空",
                text(e.name)
            );
            let source = format!(
                "{}\n{}\n{}",
                text(e.injections),
                text(e.locals),
                text(e.highlights)
            );
            if let Err(err) = tree_sitter::Query::new(&language(e), &source) {
                failed.push(format!("{}: {err}", text(e.name)));
            }
        }
        assert!(failed.is_empty(), "编不过的查询:\n{}", failed.join("\n"));
    }

    #[test]
    fn 注入目标非空且无重复() {
        for e in &ENTRIES {
            let langs = list(e.injection_languages);
            let mut dedup = langs.clone();
            dedup.sort();
            dedup.dedup();
            assert_eq!(dedup.len(), langs.len(), "{}", text(e.name));
            assert!(langs.iter().all(|l| !l.is_empty()));
        }
        assert_eq!(list(ENTRIES[2].injection_languages)[0], "php");
    }
}
