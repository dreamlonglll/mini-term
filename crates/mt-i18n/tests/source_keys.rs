//! 源码里的文案调用点 ↔ 字典对账:扫 `crates/` 下所有 .rs 文件里**字面量形式**的调用,
//! 每个 `(ns, key)` 都必须在字典里存在。
//!
//! 打错 key 不会崩:debug 构建在**执行到**那一行时才断言,release 直接把 key 本身显示在
//! 界面上 —— 冷门面板里的错 key 很容易一路漏到发版。这里不用跑界面,读源码就能抓。
//!
//! 认的调用形态(前面可以带 `mt_i18n::` / `crate::i18n::` 之类的路径):
//! - `t("ns", "key")`、`t_args("ns", "key", …)`、`tr!("ns", "key", …)`;
//! - `t_in(<语言>, "ns", "key")`、`t_args_in(<语言>, "ns", "key", …)`(第一个实参跳过);
//! - `t_path("ns.key")`、`t_path_args("ns.key", …)`、`tr!("ns.key")`。
//!
//! **跳过**(不算违规,也查不了):实参不是字符串字面量的调用 —— `lookup(locale, ns, &key)`
//! 这类运行时拼 key 的动态查找、`|key| t("ns", key)` 这类转发闭包。整行注释(含文档注释
//! 里的示例)也跳过。
//!
//! 参照 Winc(Apache-2.0)`crates/winc-i18n/src/tests.rs` 的 `source_calls_match_the_catalog`
//! 的思路,扫描器按本仓的调用形态重写。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mt_i18n::{Locale, lookup, namespaces};

/// 一个字面量调用点。
#[derive(Debug)]
struct Call {
    ns: String,
    key: String,
    at: String,
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            // 构建产物与依赖目录不扫
            if name != "target" && name != "node_modules" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// 去掉整行注释(`//`、`///`、`//!` 开头的行),换成空行保住行号。
fn strip_comment_lines(src: &str) -> String {
    src.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从 `rest` 开头读一个普通字符串字面量(允许前导空白),返回 (内容, 剩余)。
/// 不是字面量(变量、表达式、原始字符串)返回 `None`。key 里不会有转义,遇到就当动态。
fn string_literal(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.trim_start();
    let body = rest.strip_prefix('"')?;
    let end = body.find('"')?;
    let lit = &body[..end];
    if lit.contains('\\') {
        return None;
    }
    Some((lit, &body[end + 1..]))
}

/// 跳过第一个实参(到同层的第一个逗号为止),返回逗号之后的部分。
fn skip_first_arg(rest: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => return None,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return Some(&rest[i + 1..]),
            _ => {}
        }
    }
    None
}

/// 两个字面量 `"ns", "key"`;第二个后面必须紧跟 `,` 或 `)`。
fn ns_key(rest: &str) -> Option<(String, String)> {
    let (ns, rest) = string_literal(rest)?;
    let rest = rest.trim_start().strip_prefix(',')?;
    let (key, rest) = string_literal(rest)?;
    let next = rest.trim_start().chars().next()?;
    (next == ',' || next == ')').then(|| (ns.to_string(), key.to_string()))
}

/// 一个点分全路径字面量 `"ns.key"`。
fn dotted(rest: &str) -> Option<(String, String)> {
    let (path, rest) = string_literal(rest)?;
    let next = rest.trim_start().chars().next()?;
    if next != ',' && next != ')' {
        return None;
    }
    let (ns, key) = path.split_once('.')?;
    Some((ns.to_string(), key.to_string()))
}

fn calls_in(file: &Path, src: &str, out: &mut Vec<Call>) {
    let src = strip_comment_lines(src);
    // 长的放前面:`t_args_in(` 不能先被 `t_args(` 认走(前缀不同,但写成表更直观)
    const FORMS: [&str; 7] = [
        "t_args_in(",
        "t_args(",
        "t_in(",
        "t_path_args(",
        "t_path(",
        "tr!(",
        "t(",
    ];
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        let form = if bytes[i] == b't' {
            FORMS.iter().find(|f| src[i..].starts_with(**f))
        } else {
            None
        };
        let Some(form) = form else {
            i += src[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        // 前一个字符不能是标识符字符(排除 `foo_t(`、`fn t(` 由下面的 `fn ` 判断排除)
        let prev_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let is_def = src[..i].trim_end().ends_with("fn");
        let rest = &src[i + form.len()..];
        i += form.len();
        if !prev_ok || is_def {
            continue;
        }
        let parsed = match *form {
            "t(" | "t_args(" => ns_key(rest),
            "t_in(" | "t_args_in(" => skip_first_arg(rest).and_then(ns_key),
            "t_path(" | "t_path_args(" => dotted(rest),
            // tr! 两种写法:先试 `"ns", "key"`,不是再试点分全路径
            _ => ns_key(rest).or_else(|| dotted(rest)),
        };
        if let Some((ns, key)) = parsed {
            let line = src[..i].matches('\n').count() + 1;
            out.push(Call {
                ns,
                key,
                at: format!("{}:{line}", file.display()),
            });
        }
    }
}

fn source_calls() -> Vec<Call> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("mt-i18n 在 crates/ 下");
    let mut files = Vec::new();
    rust_files(crates, &mut files);
    files.sort();
    let mut calls = Vec::new();
    for file in &files {
        // 生成的字典本身没有调用点
        if file.ends_with(Path::new("mt-i18n/src/dict.rs")) {
            continue;
        }
        let src = std::fs::read_to_string(file).expect("读源码");
        calls_in(file, &src, &mut calls);
    }
    calls
}

/// 故意查不存在的 key 的调用点:`tests/consistency.rs` 的 `fallback_chain_in_release`
/// 验的就是「未知 key 原样返回」。
const INTENTIONALLY_MISSING: &[(&str, &str)] = &[("app", "no_such_key"), ("no_such_ns", "k")];

#[test]
fn 源码里字面量调用的每个_key_都在字典里() {
    let calls = source_calls();
    // 扫描器失灵(形态全没认出来)时也会「全过」,数量下限挡一下
    assert!(
        calls.len() > 500,
        "只认出 {} 个调用点,扫描器坏了?",
        calls.len()
    );
    let missing: Vec<String> = calls
        .iter()
        .filter(|c| !INTENTIONALLY_MISSING.contains(&(c.ns.as_str(), c.key.as_str())))
        .filter(|c| lookup(Locale::Zh, &c.ns, &c.key).is_none())
        .map(|c| format!("{}: {}.{}", c.at, c.ns, c.key))
        .collect();
    assert!(
        missing.is_empty(),
        "字典里没有这些 key({} 处):\n{}",
        missing.len(),
        missing.join("\n")
    );
}

/// 字典里有、源码里没有字面量调用的 key —— 只打印,不判失败:动态拼 key(`lookup`
/// / `format!` 出来的状态名)用到的条目这里看不见,列出来供人工清理时参考。
#[test]
fn 打印源码里没有字面量调用的_key() {
    let used: BTreeSet<(String, String)> =
        source_calls().into_iter().map(|c| (c.ns, c.key)).collect();
    let mut unused = Vec::new();
    for ns in namespaces() {
        for (key, _) in ns.zh {
            if !used.contains(&(ns.name.to_string(), key.to_string())) {
                unused.push(format!("{}.{key}", ns.name));
            }
        }
    }
    println!(
        "字典里有 {} 个 key 没有字面量调用点(可能是动态 key,仅供参考):",
        unused.len()
    );
    for key in &unused {
        println!("  {key}");
    }
}

#[test]
fn 扫描器认得各种调用形态() {
    let src = r#"
        let a = t("app", "menu.settings");
        let b = mt_i18n::t_in(Locale::En, "app", "titleBar.close");
        let c = tr!("time", "minutesAgo", n = 5);
        let d = tr!("app.menu.settings");
        let e = t_args_in(mt_i18n::locale(), "time", "minutesAgo", &[("n", "1")]);
        let f = t_path("app.titleBar.minimize");
        let g = t(
            "app",
            "titleBar.maximize",
        );
        // t("nope", "commented.out")
        let h = lookup(locale, "app", &key);
        let i = |key: &'static str| t("terminalSearch", key);
        let j = format!("{}", fmt_t("x", "y"));
        fn t(ns: &str, key: &str) {}
    "#;
    let mut calls = Vec::new();
    calls_in(Path::new("x.rs"), src, &mut calls);
    let got: Vec<String> = calls
        .iter()
        .map(|c| format!("{}.{}@{}", c.ns, c.key, c.at))
        .collect();
    assert_eq!(
        got,
        [
            "app.menu.settings@x.rs:2",
            "app.titleBar.close@x.rs:3",
            "time.minutesAgo@x.rs:4",
            "app.menu.settings@x.rs:5",
            "time.minutesAgo@x.rs:6",
            "app.titleBar.minimize@x.rs:7",
            "app.titleBar.maximize@x.rs:8",
        ]
    );
}
