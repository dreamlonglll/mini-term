//! 语法高亮扩展包(`mt-syntax-pack`,cdylib)与宿主(mt-app)之间的 C ABI。
//!
//! # 为什么是 C ABI
//!
//! Rust 没有稳定 ABI,但语法 crate 导出的本来就是 C 函数 `tree_sitter_<名>()`:语法包只要
//! 把这些函数指针连同高亮查询原文放进一张 `#[repr(C)]` 静态表,宿主加载动态库后调一次
//! 入口 [`ENTRY_SYMBOL`] 拿到表即可。tree-sitter **运行时只在宿主里有一份**(语法 crate
//! 只依赖 `tree-sitter-language`,不依赖 `tree-sitter`),语言对象(`TSLanguage`)是动态库里
//! 的静态数据,所以宿主**永不卸载**库句柄。
//!
//! # 字符串
//!
//! 一律 [`AbiStr`](指针 + 字节数,不要求 NUL 结尾)。宿主读出后自己校验 UTF-8,不信任。
//! `len == 0` 时宿主不解引用指针。
//!
//! # 版本
//!
//! [`ABI_VERSION`] 只在表结构变化时加一;表里另带 `entry_size` 兜「改了结构忘了加版本号」。
//! 语法包与主程序同版本发布(扩展清单绑定版本),对不上只会出现在手工拷错文件时:
//! 宿主拒绝加载,这些语言退成纯文本。
//!
//! # 语言清单
//!
//! [`LANGUAGES`] 是语法包提供的全部语言(名称 + 显示名 + 别名)。宿主拿它:
//! - 判断「这个文件需要语法包」(文件预览的提示条);
//! - **白名单**:表里只有清单内的名字才注册,语法包不能借机覆盖宿主内置语言;
//! - 别名注册:组件库自己的别名解析(`Language::from_name`)只认编进组件库的语言,
//!   挪进语法包的语言要宿主把 `cs` / `c++` / `rb` / `kt` … 逐个注册成注册表的键。
//!
//! 语法包的单测逐条对账本清单。
#![no_std]

/// 表结构版本。改了 [`PackTable`] / [`LanguageEntry`] / [`AbiStr`] / [`AbiStrList`] 的布局就加一。
pub const ABI_VERSION: u32 = 1;

/// 语法包导出的入口函数名([`EntryFn`])。
pub const ENTRY_SYMBOL: &str = "mt_syntax_pack_entry";

/// 同 [`ENTRY_SYMBOL`],带结尾 NUL(`libloading::Library::get` 要的形状)。
pub const ENTRY_SYMBOL_NUL: &[u8] = b"mt_syntax_pack_entry\0";

/// 入口函数:返回指向静态表的指针(永不为空,生命周期 = 库句柄)。
pub type EntryFn = unsafe extern "C" fn() -> *const PackTable;

/// 语法 crate 导出的 `tree_sitter_<名>()`。与 `tree_sitter_language::LanguageFn::into_raw`
/// 的返回类型逐字一致,宿主用 `LanguageFn::from_raw` 原样包回去。
pub type RawLanguageFn = unsafe extern "C" fn() -> *const ();

/// [`LanguageEntry::flags`] 的位。
pub mod flags {
    /// 高亮查询是 nvim-treesitter 口径(`@field` / `@method` / `#lua-match?` …),宿主注册前
    /// 要先做 `zed_captures` 再做 `demote_catch_alls`(改写代码在宿主
    /// `mt-app::syntax_languages`,不进语法包)。组件库来源的查询已是 Zed 口径,不置这一位。
    pub const ZED_REWRITE: u32 = 1 << 0;

    /// 宿主认识的全部位;表里出现别的位 = 新版语法包配了旧版宿主,宿主忽略那些位。
    pub const KNOWN: u32 = ZED_REWRITE;
}

/// 一段 UTF-8 字节(不要求 NUL 结尾)。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AbiStr {
    pub ptr: *const u8,
    pub len: usize,
}

impl AbiStr {
    pub const fn new(text: &'static str) -> Self {
        Self {
            ptr: text.as_ptr(),
            len: text.len(),
        }
    }
}

/// [`AbiStr`] 数组。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AbiStrList {
    pub ptr: *const AbiStr,
    pub len: usize,
}

impl AbiStrList {
    pub const EMPTY: Self = Self::new(&[]);

    pub const fn new(items: &'static [AbiStr]) -> Self {
        Self {
            ptr: items.as_ptr(),
            len: items.len(),
        }
    }
}

/// 一种语言:注册表键名 + 解析器 + 三份查询 + 注入目标 + 处理标志。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LanguageEntry {
    /// 注册表里的名字,必须在 [`LANGUAGES`] 里(宿主按白名单收)。
    pub name: AbiStr,
    pub language: RawLanguageFn,
    pub highlights: AbiStr,
    pub injections: AbiStr,
    pub locals: AbiStr,
    /// 注入目标语言名(宿主内置或语法包里的名字均可)。
    pub injection_languages: AbiStrList,
    /// [`flags`] 的位或。
    pub flags: u32,
}

/// 入口返回的整张表。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PackTable {
    /// 必须等于宿主编译时的 [`ABI_VERSION`]。
    pub abi_version: u32,
    /// `size_of::<LanguageEntry>()`,宿主核对。
    pub entry_size: u32,
    pub languages: *const LanguageEntry,
    pub languages_len: usize,
}

// SAFETY: 这几种表结构里的指针只指向 `'static` 的只读数据(字符串字面量、静态数组、
// 语法 crate 的 C 函数),构造后从不写;跨线程共享只读引用没有数据竞争。
unsafe impl Sync for AbiStr {}
unsafe impl Sync for AbiStrList {}
unsafe impl Sync for LanguageEntry {}
unsafe impl Sync for PackTable {}

/// 语法包提供的一种语言(宿主侧的静态清单)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackLanguage {
    /// 注册表键名,与 `file_viewer::language_for` 返回的字符串一致。
    pub name: &'static str,
    /// 界面上的名字(提示条「<语言> 的语法高亮需要…」)。
    pub display_name: &'static str,
    /// 额外注册成注册表键的别名(Markdown 代码围栏写 ```cs / ```rb 时靠它)。
    /// 组件库来源的照抄组件库 0.10.0 `Language::from_name` 的别名。
    pub aliases: &'static [&'static str],
    /// 语法包表里该语言应带的 [`flags`](语法包单测对账)。
    pub flags: u32,
}

/// 语法包的全部语言,顺序即语法包表的顺序。
pub const LANGUAGES: &[PackLanguage] = &[
    // ---- 来自 gpui-component 0.10.0(配置照抄 `highlighter/languages.rs`)----
    PackLanguage {
        name: "cpp",
        display_name: "C++",
        aliases: &["c++"],
        flags: 0,
    },
    PackLanguage {
        name: "elixir",
        display_name: "Elixir",
        aliases: &["ex"],
        flags: 0,
    },
    PackLanguage {
        name: "php",
        display_name: "PHP",
        aliases: &["php3", "php4", "php5", "phtml"],
        flags: 0,
    },
    PackLanguage {
        name: "ruby",
        display_name: "Ruby",
        aliases: &["rb"],
        flags: 0,
    },
    PackLanguage {
        name: "scala",
        display_name: "Scala",
        aliases: &[],
        flags: 0,
    },
    PackLanguage {
        name: "sql",
        display_name: "SQL",
        aliases: &[],
        flags: 0,
    },
    PackLanguage {
        name: "zig",
        display_name: "Zig",
        aliases: &[],
        flags: 0,
    },
    // ---- 来自 mt-app `syntax_languages.rs`(nvim 口径查询,宿主改写)----
    PackLanguage {
        name: "csharp",
        display_name: "C#",
        aliases: &["cs"],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "swift",
        display_name: "Swift",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "cmake",
        display_name: "CMake",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "proto",
        display_name: "Protocol Buffers",
        aliases: &["protobuf"],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "graphql",
        display_name: "GraphQL",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "kotlin",
        display_name: "Kotlin",
        aliases: &["kt", "kts", "ktm"],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "powershell",
        display_name: "PowerShell",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "dart",
        display_name: "Dart",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
    PackLanguage {
        name: "groovy",
        display_name: "Groovy",
        aliases: &[],
        flags: flags::ZED_REWRITE,
    },
];

/// 按注册表键名或别名找语法包语言(区分大小写,与组件库注册表同口径)。
pub fn find_language(name: &str) -> Option<&'static PackLanguage> {
    LANGUAGES
        .iter()
        .find(|lang| lang.name == name || lang.aliases.contains(&name))
}

/// 当前平台上语法包动态库的文件名(扩展清单里 `syntax-pack` 组件的那个文件)。
pub const fn library_file_name() -> &'static str {
    if cfg!(windows) {
        "mt_syntax_pack.dll"
    } else if cfg!(target_os = "macos") {
        "libmt_syntax_pack.dylib"
    } else {
        "libmt_syntax_pack.so"
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn 名字与别名全局唯一() {
        let mut seen = BTreeSet::new();
        for lang in LANGUAGES {
            assert!(seen.insert(lang.name), "重名: {}", lang.name);
            for alias in lang.aliases {
                assert!(seen.insert(alias), "别名撞名: {alias}");
            }
        }
    }

    #[test]
    fn 别名查得到本名() {
        assert_eq!(find_language("cs").map(|l| l.name), Some("csharp"));
        assert_eq!(find_language("c++").map(|l| l.name), Some("cpp"));
        assert_eq!(find_language("rb").map(|l| l.name), Some("ruby"));
        assert_eq!(find_language("kt").map(|l| l.name), Some("kotlin"));
        assert_eq!(find_language("kotlin").map(|l| l.name), Some("kotlin"));
        assert_eq!(find_language("rust"), None, "内置语言不在语法包里");
        assert_eq!(find_language("CS"), None, "区分大小写");
    }

    #[test]
    fn 标志位只用已知位() {
        for lang in LANGUAGES {
            assert_eq!(lang.flags & !flags::KNOWN, 0, "{}", lang.name);
        }
    }

    #[test]
    fn 入口符号两种写法一致() {
        assert_eq!(
            &ENTRY_SYMBOL_NUL[..ENTRY_SYMBOL_NUL.len() - 1],
            ENTRY_SYMBOL.as_bytes()
        );
        assert_eq!(ENTRY_SYMBOL_NUL.last(), Some(&0));
    }
}
