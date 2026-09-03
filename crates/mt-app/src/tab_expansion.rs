//! 显示用 Tab 展开(issue #74:Go 文件预览整篇顶格)。
//!
//! # 为什么要有这一层
//!
//! gpui-component 的编辑器把每行原文**原样**交给 GPUI `shape_line`,而 GPUI 的整形器
//! (Windows 走 DirectWrite,mac / Linux 走 core-text / cosmic-text)对 `\t` 没有制表位
//! 的概念,画出来是零宽 —— Go / Makefile 这类靠 Tab 缩进的文件在预览里整篇顶格,
//! gofmt 的结构体字段对齐也全塌。Zed 自己是在 display map 那层把 Tab 展成空格再整形的,
//! gpui-component 0.5.1 没有这一层(上游 main 长出了 `display_map/`,但只有 fold / wrap
//! 两张图,且绑定的是 zed git 版 gpui,本仓用不上)。
//!
//! 做法与 [`crate::file_viewer::LineEnding`] 三件套同源:**读入时按制表位把 Tab 展成
//! 空格喂编辑器,写回时还原**。还原分两档:
//!
//! 1. **没动过的行逐字还原**:展开时记下「展开后的行 → 原始行」,写回时命中即原样吐回。
//!    只看内容不看位置,行被移动 / 复制照样还原得回来;
//! 2. **新增 / 改过的行按行首缩进折回**:文件本身以 Tab 缩进时,行首每 [`TAB_WIDTH`] 个
//!    空格折成一个 `\t`,不足一档的留空格;行中间的空格一律不动(gofmt 本来就是
//!    「Tab 缩进、空格对齐」)。空格缩进的文件不折。
//!
//! 不变式:`expand(restore(v)) == v` —— 写回后编辑器内容不必重建,只要用写回的文本
//! 重算一次映射即可([`crate::file_viewer::FileViewer::finish_save`])。
//!
//! # 刻意的取舍
//!
//! - **改过的行里、行中间的 Tab 会变成空格**(TSV 之类拿 Tab 当数据的文件):映射只对
//!   整行命中,行内改一个字就只剩行首缩进能折回。查看 / 只改几行是压倒性的主流用法,
//!   没动的行仍是逐字节保真。
//! - **`"\tfoo"` 与 `"    foo"` 同时出现**时,后者写回也会变成前者:两者展开后是同一行,
//!   映射分不出来。视觉一样、gofmt 也会这么改,不值得为它多记一份位置信息。
//! - **制表位按字符数算列**,CJK 不按两列:Tab 出现在 CJK 之后的行少到可以忽略,而按显示
//!   宽算的话缩进参考线(gpui-component 按字符数)反而对不上。

use std::borrow::Cow;
use std::collections::HashMap;

/// 一个 Tab 展开的列宽。Go 官方工具链假定 8,但主流编辑器默认 4,取 4 —— 深层嵌套
/// 的代码 8 列一档会把行推得很宽。
pub const TAB_WIDTH: usize = 4;

/// 单行按制表位展开。没有 Tab 的行原样借出,不分配。
pub fn expand_line(line: &str, width: usize) -> Cow<'_, str> {
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let width = width.max(1);
    let mut out = String::with_capacity(line.len() + width * 4);
    let mut col = 0usize;
    for c in line.chars() {
        if c == '\t' {
            let n = width - col % width;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    Cow::Owned(out)
}

/// 一份文件的 Tab 展开记录:读入时由 [`TabExpansion::expand`] 生成,写回时
/// [`TabExpansion::restore`] 还原。
#[derive(Debug, Clone, Default)]
pub struct TabExpansion {
    width: usize,
    /// 展开后的行 → 原始行。只记含 Tab 的行;同一展开结果先到先得。
    originals: HashMap<String, String>,
    /// 文件以 Tab 缩进(行首是 `\t` 的行不少于行首是空格的行)。写回时新增 / 改过的行
    /// 据此决定要不要把行首空格折回 Tab。
    indents_with_tabs: bool,
}

impl TabExpansion {
    /// 展开整份文本(`\n` 行尾,先过 [`crate::file_viewer::normalize_to_lf`])。
    /// 返回 `(展开后的文本, 还原用的记录)`。
    pub fn expand(text: &str, width: usize) -> (String, Self) {
        let width = width.max(1);
        if !text.contains('\t') {
            return (
                text.to_string(),
                Self {
                    width,
                    ..Self::default()
                },
            );
        }
        let mut originals = HashMap::new();
        let (mut tab_led, mut space_led) = (0usize, 0usize);
        let mut out = String::with_capacity(text.len() + text.len() / 8);
        for (ix, line) in text.split('\n').enumerate() {
            if ix > 0 {
                out.push('\n');
            }
            match line.as_bytes().first() {
                Some(b'\t') => tab_led += 1,
                Some(b' ') => space_led += 1,
                _ => {}
            }
            match expand_line(line, width) {
                Cow::Borrowed(plain) => out.push_str(plain),
                Cow::Owned(expanded) => {
                    out.push_str(&expanded);
                    originals
                        .entry(expanded)
                        .or_insert_with(|| line.to_string());
                }
            }
        }
        (
            out,
            Self {
                width,
                originals,
                indents_with_tabs: tab_led > 0 && tab_led >= space_led,
            },
        )
    }

    /// 文件以 Tab 缩进。编辑器的 Tab 键按它决定一次缩进几个空格
    /// (与 [`TAB_WIDTH`] 一致,写回时正好折成一个 `\t`)。
    pub fn indents_with_tabs(&self) -> bool {
        self.indents_with_tabs
    }

    /// 编辑器内容 → 磁盘内容。见模块注释的两档规则。
    pub fn restore(&self, text: &str) -> String {
        if self.originals.is_empty() && !self.indents_with_tabs {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        for (ix, line) in text.split('\n').enumerate() {
            if ix > 0 {
                out.push('\n');
            }
            if let Some(raw) = self.originals.get(line) {
                out.push_str(raw);
            } else if self.indents_with_tabs {
                push_with_tab_indent(&mut out, line, self.width);
            } else {
                out.push_str(line);
            }
        }
        out
    }
}

/// 行首每 `width` 个空格折成一个 `\t`,余下的空格与行的其余部分原样接上。
fn push_with_tab_indent(out: &mut String, line: &str, width: usize) {
    let width = width.max(1);
    let spaces = line.bytes().take_while(|b| *b == b' ').count();
    out.extend(std::iter::repeat_n('\t', spaces / width));
    out.extend(std::iter::repeat_n(' ', spaces % width));
    out.push_str(&line[spaces..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_viewer::{LineEnding, normalize_to_lf, restore_line_ending};

    #[test]
    fn 单行_按制表位展开() {
        assert_eq!(expand_line("\tfoo", 4), "    foo");
        assert_eq!(expand_line("\t\tfoo", 4), "        foo");
        // 行中间的 Tab 推到下一个制表位,而不是固定补 4 个
        assert_eq!(expand_line("a\tb", 4), "a   b");
        assert_eq!(expand_line("abc\tb", 4), "abc b");
        assert_eq!(expand_line("abcd\tb", 4), "abcd    b");
        // 宽度 1 退化成「一个 Tab 一个空格」
        assert_eq!(expand_line("\ta\tb", 1), " a b");
    }

    #[test]
    fn 单行_没有tab不分配() {
        assert!(matches!(expand_line("    foo", 4), Cow::Borrowed(_)));
        assert!(matches!(expand_line("", 4), Cow::Borrowed(_)));
    }

    const GO: &str = "package main\n\nfunc main() {\n\tif ok {\n\t\treturn\n\t}\n\tx := 1\n}\n";

    #[test]
    fn 整份_展开后没有tab且行数不变() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        assert!(!expanded.contains('\t'));
        assert_eq!(expanded.lines().count(), GO.lines().count());
        assert!(expanded.contains("\n    if ok {\n        return\n    }\n"));
        assert!(tabs.indents_with_tabs());
    }

    #[test]
    fn 整份_没动过就逐字节还原() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        assert_eq!(tabs.restore(&expanded), GO);
        // 末尾没有换行的文件也一样
        let no_trailing = GO.trim_end_matches('\n');
        let (expanded, tabs) = TabExpansion::expand(no_trailing, 4);
        assert_eq!(tabs.restore(&expanded), no_trailing);
    }

    #[test]
    fn 整份_行被移动复制照样还原() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        // 把 `x := 1` 挪到 if 之前,再复制一份 return
        let edited = expanded
            .replace("    if ok {\n", "    x := 1\n    if ok {\n")
            .replace("        return\n", "        return\n        return\n");
        let restored = tabs.restore(&edited);
        assert_eq!(
            restored,
            "package main\n\nfunc main() {\n\tx := 1\n\tif ok {\n\t\treturn\n\t\treturn\n\t}\n\tx := 1\n}\n"
        );
    }

    #[test]
    fn 整份_新增行按行首缩进折回tab() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        let edited = expanded.replace("    x := 1\n", "    x := 1\n        y := 2\n      z := 3\n");
        let restored = tabs.restore(&edited);
        // 8 个空格 = 两个 Tab;6 个 = 一个 Tab + 两个空格
        assert!(
            restored.contains("\n\t\ty := 2\n\t  z := 3\n"),
            "{restored:?}"
        );
    }

    #[test]
    fn 整份_改过的行只折行首_行中空格不动() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        let edited = expanded.replace("    x := 1\n", "    x  :=    1\n");
        let restored = tabs.restore(&edited);
        assert!(restored.contains("\n\tx  :=    1\n"), "{restored:?}");
    }

    #[test]
    fn 整份_空格缩进的文件写回不折tab() {
        // 只有字符串里有一个 Tab:不算 Tab 缩进
        let src = "def f():\n    s = \"a\tb\"\n    return s\n";
        let (expanded, tabs) = TabExpansion::expand(src, 4);
        assert!(!tabs.indents_with_tabs());
        // `    s = "a` 到 Tab 时已在第 10 列,下一制表位是 12 ⇒ 补 2 个空格
        assert_eq!(expanded, "def f():\n    s = \"a  b\"\n    return s\n");
        // 没动:字符串里的 Tab 逐字还原
        assert_eq!(tabs.restore(&expanded), src);
        // 新增一行 8 空格缩进:保持空格
        let edited = expanded.replace("    return s\n", "        pass\n    return s\n");
        assert_eq!(
            tabs.restore(&edited),
            "def f():\n    s = \"a\tb\"\n        pass\n    return s\n"
        );
    }

    #[test]
    fn 整份_完全没有tab的文件是直通() {
        let src = "fn main() {\n    println!(\"hi\");\n}\n";
        let (expanded, tabs) = TabExpansion::expand(src, 4);
        assert_eq!(expanded, src);
        assert!(!tabs.indents_with_tabs());
        let edited = format!("{src}        extra\n");
        assert_eq!(tabs.restore(&edited), edited);
    }

    #[test]
    fn 整份_tab与四空格同形时归并成tab() {
        let src = "\tfoo\n    foo\n";
        let (expanded, tabs) = TabExpansion::expand(src, 4);
        assert_eq!(expanded, "    foo\n    foo\n");
        assert_eq!(tabs.restore(&expanded), "\tfoo\n\tfoo\n");
    }

    /// 写回后的文本再展开一次必须与编辑器内容一致 —— `finish_save` 靠它不重建编辑器。
    #[test]
    fn 不变式_展开还原再展开是恒等() {
        let (expanded, tabs) = TabExpansion::expand(GO, 4);
        let edited = expanded
            .replace("    x := 1\n", "    x := 1\n          y := 2\n")
            .replace("        return\n", "        return  // changed\n");
        let restored = tabs.restore(&edited);
        let (again, _) = TabExpansion::expand(&restored, 4);
        assert_eq!(again, edited);
    }

    /// 与行尾三件套叠在一起:CRLF + Tab 的文件没动过就整份保真。
    #[test]
    fn 与行尾还原叠加_crlf文件保真() {
        let src = "package main\r\n\r\nfunc main() {\r\n\tx := 1\r\n}\r\n";
        let ending = LineEnding::detect(src);
        let (expanded, tabs) = TabExpansion::expand(&normalize_to_lf(src), 4);
        assert_eq!(expanded, "package main\n\nfunc main() {\n    x := 1\n}\n");
        assert_eq!(restore_line_ending(&tabs.restore(&expanded), ending), src);
    }
}
