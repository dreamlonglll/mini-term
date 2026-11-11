//! 模糊打分:子序列匹配 + 「命中质量」打分,返回分数与命中的字符下标。
//!
//! 移植自 Winc(Apache-2.0)`crates/winc-editor/src/workspace/picker/fuzzy.rs`:
//! [`normalize_query`] / `fold` / `is_subsequence` / `align` / `rate` 五段逐条照搬,权重一个
//! 没改(词首 +15、连续命中逐级 +15·n、命中落在文件名段 +10、整个查询落在文件名内每字 +5,
//! 外加密度 / 起点 / 长度三项;两种对齐方式各算一遍取高分)。
//!
//! 与原文的差异:
//!
//! - 去掉了 Winc 的多线程分段打分(`score_all` + `background::split`)。本仓的调用方在索引大时
//!   把**整次**打分挪到一条后台线程(见 `mt-app` 的 `quick_open` 模块注释),单线程扫二十万条
//!   路径是几十毫秒的量级,再拆线程得不偿失;
//! - 输入从 Winc 的 `IndexedFile` 换成本模块的 [`PathRef`],字段同义。
//!
//! 所有下标都是**字符**下标:中文路径里一个字占三个字节,混用字节下标会把文件名那一段算错。

/// 单个查询词最多参与匹配的字符数。超过时截去多余部分,正常输入不会达到(Winc 同值)。
pub const MAX_QUERY_CHARS: usize = 128;

/// 一条待打分的路径。`path` 是相对项目根、以 `/` 分隔的路径,`name` 是它的最后一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathRef<'a> {
    pub path: &'a str,
    pub name: &'a str,
}

impl<'a> PathRef<'a> {
    /// 从完整相对路径切出文件名段。
    pub fn new(path: &'a str) -> Self {
        let name = path.rfind('/').map_or(path, |i| &path[i + 1..]);
        Self { path, name }
    }
}

/// 归一化查询词:去掉所有空白,反斜杠换成 `/`,截断到 [`MAX_QUERY_CHARS`] 个字符,逐字符转小写。
///
/// 空白不参与匹配,可以用空格分开路径的几部分;反斜杠是 Windows 上复制出来的路径里的分隔符,
/// 索引里一律存 `/`,于是两种分隔符一视同仁。中文没有大小写,转小写对它是恒等变换。
pub fn normalize_query(query: &str) -> Vec<char> {
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(MAX_QUERY_CHARS)
        .map(|c| if c == '\\' { '/' } else { fold(c) })
        .collect()
}

/// 把一个字符转成小写,保证一对一。
///
/// `str::to_lowercase` 会把个别字符变成两个(`İ` → `i̇`),字符下标就与原路径对不上;这里只取
/// 第一个,查询和路径用同一个函数,匹配结果因此一致。
fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// 打分时复用的缓冲,整次搜索只分配一次。
#[derive(Default)]
pub struct Scratch {
    /// 路径逐字符转小写后的字符序列。
    lower: Vec<char>,
    /// 路径的原始字符序列,用于判断由小写到大写的词首。
    original: Vec<char>,
    /// 查询每个字符命中的位置(字符下标)。
    positions: Vec<usize>,
}

/// `needle` 是不是 `path` 的子序列(大小写不敏感)。
///
/// 不分配内存;大项目里绝大多数路径在这一步就被淘汰。
fn is_subsequence(needle: &[char], path: &str) -> bool {
    // 查询和路径都是 ASCII(常见情形)时按字节比较,省去 UTF-8 解码和 Unicode 折叠。路径含
    // 非 ASCII 字符时不能这样:有些字符折叠后是 ASCII(`İ` → `i`、开尔文符号 → `k`),按字节
    // 比较会漏掉。
    if path.is_ascii() && needle.iter().all(char::is_ascii) {
        let mut rest = needle.iter().map(|&c| c as u8);
        let mut want = rest.next();
        for &b in path.as_bytes() {
            match want {
                Some(w) if b.to_ascii_lowercase() == w => want = rest.next(),
                Some(_) => {}
                None => return true,
            }
        }
        return want.is_none();
    }

    let mut rest = needle.iter().peekable();
    for c in path.chars() {
        match rest.peek() {
            Some(&&want) if fold(c) == want => {
                rest.next();
            }
            Some(_) => {}
            None => return true,
        }
    }
    rest.peek().is_none()
}

/// 给一条路径打分;`needle` 不是路径的子序列时返回 `None`。空查询恒为 `Some(0)`。
///
/// 分数是「命中质量」各项的累加:命中的字符挨在一起、在词首(`/`、`_`、`-`、`.`、空格后面)、
/// 在小写到大写的转换处都加分;命中落在文件名那一段额外加分;命中占路径的比例越高越好,避免
/// 长路径靠数量取胜。`needle` 须经 [`normalize_query`] 归一化。
pub fn score(needle: &[char], file: PathRef<'_>, scratch: &mut Scratch) -> Option<u32> {
    align(needle, file, scratch, None)
}

/// `needle` 在路径里命中的字符下标(对齐方式与打分时相同),界面据此标出命中的字符;
/// 不命中时返回 `None`。
///
/// 只对界面上显示的几行计算,不在打分的耗时路径上。
pub fn matched_positions(needle: &[char], file: PathRef<'_>) -> Option<Vec<usize>> {
    let mut best = Vec::new();
    align(needle, file, &mut Scratch::default(), Some(&mut best))?;
    Some(best)
}

/// [`score`] 的实现;`keep` 不为 `None` 时把分数最高的那种对齐方式的命中位置存进去。
fn align(
    needle: &[char],
    file: PathRef<'_>,
    scratch: &mut Scratch,
    mut keep: Option<&mut Vec<usize>>,
) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    // 先做不分配内存的子序列检查,通过的路径才展开成字符表细算
    if !is_subsequence(needle, file.path) {
        return None;
    }

    let Scratch {
        lower,
        original,
        positions,
    } = scratch;
    original.clear();
    original.extend(file.path.chars());
    lower.clear();
    lower.extend(original.iter().map(|&c| fold(c)));
    positions.clear();

    let haystack = &lower[..];
    // `fold` 一对一,小写序列与原路径字符数相同,文件名起点按字符数从末尾倒推
    let name_start = haystack.len() - file.name.chars().count();

    // 先从前往后找出每个字符第一次出现的位置
    let mut hay = 0;
    for &c in needle {
        let found = haystack[hay..].iter().position(|&h| h == c)?;
        hay += found;
        positions.push(hay);
        hay += 1;
    }

    // 每个查询字符的命中位置有两种对齐方式,各算一遍分取较高者:往回收(`src/lib.rs` 中找
    // `src` 时命中开头连续的三个字符)与从路径末尾往回找(`button/button.ts` 中找 `button`
    // 时命中文件名内部)。只用其中一种时另一种情形会选错;往回找时每个字符一定找得到。
    let forward = hay;
    let mut best = 0;
    for limit in [forward, haystack.len()] {
        let mut limit = limit;
        for i in (0..positions.len()).rev() {
            let c = needle[i];
            let pos = haystack[..limit].iter().rposition(|&h| h == c)?;
            positions[i] = pos;
            limit = pos;
        }
        let points = rate(positions, haystack, original, name_start, needle.len());
        if let Some(keep) = keep.as_deref_mut()
            && (points > best || keep.is_empty())
        {
            keep.clone_from(positions);
        }
        best = best.max(points);
    }
    Some(best)
}

/// 计算一种对齐方式(每个查询字符命中的位置 `positions`)的分数。
///
/// 各项权重为经验值,没有单位,只表示相对大小,相对顺序由本文件的测试验证。
fn rate(
    positions: &[usize],
    haystack: &[char],
    original: &[char],
    name_start: usize,
    needle: usize,
) -> u32 {
    let mut total: u32 = 0;
    let mut run: u32 = 0;
    let whole_name = positions[0] >= name_start;

    for (i, &pos) in positions.iter().enumerate() {
        let mut points: u32 = 10;

        // 词首:路径开头、分隔符后面,或小写到大写的转换处(`fooBar` 中的 `B`)
        let boundary = pos == 0
            || matches!(haystack[pos - 1], '/' | '_' | '-' | '.' | ' ')
            || (original[pos].is_uppercase() && original[pos - 1].is_lowercase());
        if boundary {
            points += 15;
        }

        // 连续命中的字符越多,加分越大(逐级递增),以奖励整段命中
        if i > 0 && positions[i - 1] + 1 == pos {
            run += 1;
            points += 15 * run;
        } else {
            run = 0;
        }

        // 命中在文件名那一段
        if pos >= name_start {
            points += 10;
        }

        // 查询整个落在文件名内:用户多半按文件名查找
        if whole_name {
            points += 5;
        }

        total += points;
    }

    // 命中越靠前、占路径的比例越高,分数越高;路径越长,扣分越多
    let span = positions[positions.len() - 1] - positions[0] + 1;
    let density = (needle as u32 * 20 / span.max(1) as u32).min(20);
    let start_bonus = 20u32.saturating_sub(positions[0] as u32);
    let length_penalty = (haystack.len() as u32 / 8).min(30);

    (total + density + start_bonus).saturating_sub(length_penalty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_of(query: &str, path: &str) -> Option<u32> {
        score(
            &normalize_query(query),
            PathRef::new(path),
            &mut Scratch::default(),
        )
    }

    fn positions(query: &str, path: &str) -> Option<Vec<usize>> {
        matched_positions(&normalize_query(query), PathRef::new(path))
    }

    #[test]
    fn 查询归一化去空白统一分隔符并转小写() {
        assert_eq!(
            normalize_query(" Src\\Main "),
            "src/main".chars().collect::<Vec<_>>()
        );
        assert_eq!(normalize_query("设计 稿"), vec!['设', '计', '稿']);
        let long = "a".repeat(MAX_QUERY_CHARS + 10);
        assert_eq!(normalize_query(&long).len(), MAX_QUERY_CHARS);
    }

    #[test]
    fn 文件名段从最后一个分隔符切() {
        assert_eq!(PathRef::new("src/main.rs").name, "main.rs");
        assert_eq!(PathRef::new("README.md").name, "README.md");
        assert_eq!(PathRef::new("文档/说明.md").name, "说明.md");
    }

    #[test]
    fn 不是子序列就不命中() {
        assert_eq!(score_of("zzz", "src/main.rs"), None);
        assert_eq!(score_of("mian", "src/main.rs"), None);
        // 字符顺序颠倒时不算子序列
        assert_eq!(score_of("nim", "src/main.rs"), None);
        assert_eq!(positions("zzz", "a.rs"), None);
    }

    #[test]
    fn 空查询恒命中且不高亮() {
        assert_eq!(score_of("", "src/main.rs"), Some(0));
        assert_eq!(score_of("   ", "src/main.rs"), Some(0));
    }

    /// 标出来的字与打分时选的对齐方式一致:`button` 落进文件名,`src` 落在开头,中文按字符数
    /// 计下标。
    #[test]
    fn 命中位置跟随选中的对齐方式() {
        assert_eq!(
            positions("button", "button/button.ts"),
            Some((7..13).collect::<Vec<_>>())
        );
        assert_eq!(positions("src", "src/lib.rs"), Some(vec![0, 1, 2]));
        assert_eq!(positions("Lib", "src/lib.rs"), Some(vec![4, 5, 6]));
        assert_eq!(positions("说明", "文档/说明.md"), Some(vec![3, 4]));
    }

    #[test]
    fn 大小写不敏感() {
        assert!(score_of("main", "src/Main.rs").is_some());
        assert!(score_of("MAIN", "src/Main.rs").is_some());
        assert!(score_of("MaIn", "src/Main.rs").is_some());
    }

    /// 两种分隔符一视同仁:Windows 上复制出来的 `src\lib` 照样命中 `/` 分隔的索引路径。
    #[test]
    fn 反斜杠与斜杠一视同仁() {
        assert_eq!(
            score_of("src\\lib", "src/lib.rs"),
            score_of("src/lib", "src/lib.rs")
        );
        assert!(score_of("src\\lib", "src/lib.rs").is_some());
    }

    #[test]
    fn 逐字符折叠不会错位() {
        // `İ` 的标准小写是两个字符,逐字符折叠只取一个,下标不会错位,也不会 panic
        assert!(score_of("notes", "İstanbul/notes.md").is_some());
        assert!(score_of("istanbul", "İstanbul/notes.md").is_some());
        // 文件名起点不能用「小写路径字节数 − 原文件名字节数」算:`ẞ` 转小写后字节数变少
        assert!(score_of("md", "ẞ.md").is_some());
    }
}
