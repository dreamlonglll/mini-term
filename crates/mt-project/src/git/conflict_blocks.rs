//! 冲突文件的**逐段**解决:认出工作区文件里 git 写下的冲突标记,按用户对每一段的选择
//! (采用当前 / 采用对方 / 两者都要,两种先后)一次性生成新文本写回工作区。
//! 整文件的「采用一侧 / 标记已解决」在 [`super::conflict`]。
//!
//! # 标记
//!
//! 一段冲突是行首的 `<<<<<<<`、可选的 `|||||||`(`merge.conflictStyle=diff3` / `zdiff3`
//! 才有,后面跟着共同祖先的版本)、`=======`、`>>>>>>>`。标记是**行首七个同样的符号**,
//! 后面要么没有内容,要么是一个空格加名字(`<<<<<<< HEAD`、`>>>>>>> feature/x`);
//! `=======` 后面不能有内容。名字去掉首尾空白、最多 [`LABEL_LIMIT`] 个字符。不成对的
//! (只有 `<<<<<<<`、没有 `=======` 就碰到 `>>>>>>>`)不算;一段还没收尾又碰到新的
//! `<<<<<<<`,前面那段作废、从新的这一行重新开始。
//!
//! 标记识别(`marker` / `label_text` / 成对的规则)与按选择算替换文本(`resolution`)
//! 移植自 Winc(Apache-2.0)`crates/winc-editor/src/git/merge.rs`。与原实现的差异:
//!
//! 1. 在 `&str` 上按行切(`split_inclusive('\n')`,各行带着自己的换行符),不用 ropey;
//!    每段同时记行号(界面上「第几行」)与字节区间(拼替换文本直接切片);
//! 2. 多一种「两者都要(对方在前)」;两者都要时前一段末尾没有换行就补一个,补的是
//!    `=======` 那一行的换行符(CRLF 文件补 CRLF),不是写死的 `\n`;
//! 3. 记下 base 段的名字(`||||||| merged common ancestors`),界面上与两侧名字一起显示;
//! 4. 文件第一行开头的 UTF-8 BOM 不挡标记识别(BOM 留在段外,写回时原样保留);
//! 5. `>>>>>>>` 是文件最后一行且没有换行时,替换文本去掉末尾一个换行 —— 文件「末尾没有
//!    换行」这一点写回后不变。git 自己写的冲突标记总带换行,这种文件只会出自手工编辑。
//!
//! # 写回([`write_conflict_resolution`])
//!
//! - **一次性**:各段的选择一趟拼完再落盘;没选的段连同标记原样留着(「仅写入」);
//! - **防陈旧**:写之前重读文件,与解析时([`load_conflict_file`])读到的字节逐字节比对,
//!   不一致就拒绝并返回 [`StaleConflictFile`](界面认这个类型给「文件已变化,请刷新后
//!   重试」),工作区一个字节都不动;
//! - **编码**:按 [`crate::encoding`] 识别出的编码解码、按同一编码写回(GBK 记作 GB18030、
//!   UTF-16 带回 BOM、UTF-8 的 BOM 本来就在文本里)。识别结果是只读的
//!   ([`TextEncoding::Lossy`])或二进制时不给逐段解决([`SegmentBlocker`]);
//! - **原子写**:与文件页保存同一条路(`mt_core::atomic_write`,写临时文件再改名),半路
//!   失败原文件不动。不走「原地覆写」:开着这个文件的文件页靠目录监听收到改名事件,
//!   干净的页签静默重载、有未保存修改的挂「外部已修改」提示条,与外部编辑器存盘同一
//!   条路。符号链接一律不碰(改名会把链接本身换成普通文件);
//! - 「写入并标记已解决」= 写回之后调 [`mark_conflict_resolved`],只在每段都选了时才允许。

use std::fmt;
use std::ops::Range;
use std::path::Path;

use anyhow::{Result, anyhow, bail};

use super::{ConflictInfo, list_conflicts, mark_conflict_resolved};
use crate::encoding::{Decoded, TextEncoding};

/// 冲突两侧名字最多保留的长度(`char` 数,同 Winc)。名字一般是分支名或提交号,截断
/// 过长的名字,不把整行复制进界面。
pub const LABEL_LIMIT: usize = 192;

/// 逐段解决最多处理多大的文件(8 MiB)。冲突状态随更改列表一起取(每次都要读一遍文件
/// 判有没有成对标记),再大的文件只给整文件操作。
pub const MAX_SEGMENT_FILE_BYTES: u64 = 8 << 20;

/// 文件里的一段冲突。行号从 0 起,字节区间按解码后的文本算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBlock {
    /// `<<<<<<<` 那一行。
    pub start_line: usize,
    /// `|||||||` 那一行;没有 base 段时为 `None`。
    pub base_line: Option<usize>,
    /// `=======` 那一行。
    pub middle_line: usize,
    /// `>>>>>>>` 那一行。
    pub end_line: usize,
    /// 当前这一侧的名字(`<<<<<<< HEAD` 里的 `HEAD`)。
    pub ours_label: String,
    /// base 段的名字(`||||||| merged common ancestors`);没有 base 段时为 `None`。
    pub base_label: Option<String>,
    /// 对方的名字(`>>>>>>> feature/x` 里的 `feature/x`)。
    pub theirs_label: String,
    /// 整段(连同标记行与 `>>>>>>>` 行的换行符)。
    pub span: Range<usize>,
    /// 当前这一侧的内容(各行带着自己的换行符)。
    pub ours: Range<usize>,
    /// 共同祖先的内容。
    pub base: Option<Range<usize>>,
    /// 对方的内容。
    pub theirs: Range<usize>,
    /// `=======` 那一行的换行符(「两者都要」补换行时用)。
    sep_eol: &'static str,
    /// `>>>>>>>` 那一行有没有换行符(没有 = 它是文件最后一行、文件末尾没有换行)。
    end_has_eol: bool,
}

impl ConflictBlock {
    /// 当前这一侧占几行。
    pub fn ours_lines(&self) -> usize {
        self.base_line.unwrap_or(self.middle_line) - self.start_line - 1
    }

    /// 对方占几行。
    pub fn theirs_lines(&self) -> usize {
        self.end_line - self.middle_line - 1
    }

    /// 共同祖先占几行;没有 base 段时为 `None`。
    pub fn base_lines(&self) -> Option<usize> {
        self.base_line.map(|base| self.middle_line - base - 1)
    }
}

/// 解决一段冲突时选哪边。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictPick {
    /// 只要当前这一侧(stage 2;变基时是变基目标)。
    Ours,
    /// 只要对方(stage 3;变基时是正在重放的、你自己的提交)。
    Theirs,
    /// 两者都要,当前的在前。
    OursThenTheirs,
    /// 两者都要,对方的在前。
    TheirsThenOurs,
}

/// 冲突开头的标记。
const OPEN: &str = "<<<<<<<";

/// 一行:在文本里的起止字节(含换行符)与这一行的原文(含换行符)。
struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn split_lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    let mut at = 0;
    text.split_inclusive('\n').map(move |piece| {
        let start = at;
        at += piece.len();
        Line {
            start,
            end: at,
            text: piece,
        }
    })
}

/// 一行的换行符:`"\r\n"` / `"\n"`;没有换行(文件最后一行)为 `""`。
fn eol_of(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// `line` 是标记行时返回(符号,后面的名字,不含换行符)。
///
/// 标记是行首七个同样的符号(`<`、`|`、`=`、`>`),后面要么没有内容,要么是一个空格加
/// 名字;`=======` 后面不能有内容。行尾的换行符(`\n` / `\r`)不计入。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-editor/src/git/merge.rs` 的 `marker`。
fn marker(line: &str) -> Option<(char, &str)> {
    let first = line.chars().next()?;
    if !matches!(first, '<' | '|' | '=' | '>') {
        return None;
    }
    let content = line.trim_end_matches(['\n', '\r']);
    // 四种标记符号都是 ASCII:按字节比即可,切片也一定落在字符边界上
    if content.len() < 7 || !content.as_bytes()[..7].iter().all(|&b| b == first as u8) {
        return None;
    }
    let rest = &content[7..];
    match rest.chars().next() {
        None => Some((first, rest)),
        Some(' ') if first != '=' => Some((first, &rest[1..])),
        _ => None,
    }
}

/// 标记行上的名字:最多 [`LABEL_LIMIT`] 个字符,去掉首尾空白。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-editor/src/git/merge.rs` 的 `label_text`。
fn label_text(label: &str) -> String {
    let label: String = label.chars().take(LABEL_LIMIT).collect();
    label.trim().to_owned()
}

/// 正在读的一段:`<<<<<<<` 那一行,以及读到的 `|||||||`、`=======`。
struct Open {
    line: usize,
    span_start: usize,
    ours_start: usize,
    label: String,
    /// (行号, 标记行的起点, 标记行的终点, 名字)
    base: Option<(usize, usize, usize, String)>,
    /// (行号, 标记行的起点, 标记行的终点, 换行符)
    middle: Option<(usize, usize, usize, &'static str)>,
}

/// `text` 里按位置排列的各段冲突;标记不成对的不计入(规则见模块注释)。
pub fn find_conflict_blocks(text: &str) -> Vec<ConflictBlock> {
    let mut found = Vec::new();
    // 绝大多数文件一个 `<<<<<<<` 都没有:先整体找一遍,没有就不逐行看
    if !text.contains(OPEN) {
        return found;
    }
    let mut open: Option<Open> = None;
    for (index, line) in split_lines(text).enumerate() {
        // 第一行开头的 BOM 留在段外(见模块注释差异 4)
        let skip = if index == 0 && line.text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let start = line.start + skip;
        match marker(&line.text[skip..]) {
            Some(('<', label)) => {
                open = Some(Open {
                    line: index,
                    span_start: start,
                    ours_start: line.end,
                    label: label_text(label),
                    base: None,
                    middle: None,
                });
            }
            Some(('|', label)) => {
                if let Some(o) = open.as_mut()
                    && o.base.is_none()
                    && o.middle.is_none()
                {
                    o.base = Some((index, start, line.end, label_text(label)));
                }
            }
            Some(('=', _)) => {
                if let Some(o) = open.as_mut()
                    && o.middle.is_none()
                {
                    o.middle = Some((index, start, line.end, eol_of(line.text)));
                }
            }
            Some(('>', label)) => {
                // 没读到 `=======` 就收尾的这一段作废(`take` 照样把它拿掉)
                if let Some(o) = open.take()
                    && let Some((middle_line, middle_start, middle_end, sep_eol)) = o.middle
                {
                    let ours_end = o.base.as_ref().map_or(middle_start, |b| b.1);
                    found.push(ConflictBlock {
                        start_line: o.line,
                        base_line: o.base.as_ref().map(|b| b.0),
                        middle_line,
                        end_line: index,
                        ours_label: o.label,
                        base_label: o.base.as_ref().map(|b| b.3.clone()),
                        theirs_label: label_text(label),
                        span: o.span_start..line.end,
                        ours: o.ours_start..ours_end,
                        base: o.base.as_ref().map(|b| b.2..middle_start),
                        theirs: middle_end..start,
                        sep_eol,
                        end_has_eol: !eol_of(line.text).is_empty(),
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// 去掉末尾一个换行符(`\r\n` 或 `\n`)。
fn strip_one_eol(out: &mut String) {
    if out.ends_with("\r\n") {
        out.truncate(out.len() - 2);
    } else if out.ends_with('\n') {
        out.pop();
    }
}

/// 按 `pick` 解决 `block` 时换上去的文本,各行保留原来的换行符。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-editor/src/git/merge.rs` 的 `resolution`
/// (多了「对方在前」与两处换行的细节,见模块注释差异 2、5)。
pub fn block_resolution(text: &str, block: &ConflictBlock, pick: ConflictPick) -> String {
    let ours = &text[block.ours.clone()];
    let theirs = &text[block.theirs.clone()];
    let both = |first: &str, second: &str| {
        let mut out = String::with_capacity(first.len() + second.len() + 2);
        out.push_str(first);
        // 前一段最后一行没有换行符时补一个,两段不连成一行。按标记的写法这里总有换行
        // (后面紧跟着标记行),只是兜底
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(match block.sep_eol {
                "" => "\n",
                eol => eol,
            });
        }
        out.push_str(second);
        out
    };
    let mut out = match pick {
        ConflictPick::Ours => ours.to_owned(),
        ConflictPick::Theirs => theirs.to_owned(),
        ConflictPick::OursThenTheirs => both(ours, theirs),
        ConflictPick::TheirsThenOurs => both(theirs, ours),
    };
    if !block.end_has_eol {
        strip_one_eol(&mut out);
    }
    out
}

/// 按各段的选择拼出整份新文本:`picks[i]` 对应 `blocks[i]`,`None`(或 `picks` 比
/// `blocks` 短)的那段连同标记原样保留;段外的字节一个不动。
pub fn apply_conflict_picks(
    text: &str,
    blocks: &[ConflictBlock],
    picks: &[Option<ConflictPick>],
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (index, block) in blocks.iter().enumerate() {
        out.push_str(&text[at..block.span.start]);
        match picks.get(index).copied().flatten() {
            Some(pick) => out.push_str(&block_resolution(text, block, pick)),
            None => out.push_str(&text[block.span.clone()]),
        }
        at = block.span.end;
    }
    out.push_str(&text[at..]);
    out
}

// ─── 工作区文件 ───────────────────────────────────────────────

/// 不能逐段解决的原因。单独一个类型:UI `downcast_ref` 出来换成本地化文案(菜单项的
/// 禁用说明、弹窗里的就地提示),不把后端的中文原文直接摆出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentBlocker {
    /// 删除 / 修改冲突(一侧或两侧是删除):文件里没有冲突标记,只能整文件采用一侧。
    DeleteModify,
    /// 已经不在冲突中(终端里解决过了)。
    NotConflicted,
    /// 工作区里没有这个文件、它不是普通文件(目录 / 符号链接),或者读不了。
    Unreadable,
    /// 超过 [`MAX_SEGMENT_FILE_BYTES`]。
    TooLarge,
    /// 二进制文件。
    Binary,
    /// 认不出能无损写回的编码(按替换字符只读打开的那一档)。
    Lossy,
    /// 文件里一个成对的冲突标记都没有(已经手工改过)。
    NoMarkers,
}

impl fmt::Display for SegmentBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DeleteModify => "删除 / 修改冲突没有冲突标记,只能整文件采用一侧",
            Self::NotConflicted => "该文件已不在冲突中,请刷新",
            Self::Unreadable => "工作区里读不到这个文件(不存在、不是普通文件或无权读取)",
            Self::TooLarge => "文件过大,只能整文件处理",
            Self::Binary => "二进制文件只能整文件处理",
            Self::Lossy => "无法识别文件编码(无损写回不了),只能整文件处理",
            Self::NoMarkers => "文件里没有成对的冲突标记",
        })
    }
}

impl std::error::Error for SegmentBlocker {}

/// 写回前重读发现文件已经变了(解析之后被别的程序改过)。单独一个类型:UI 认出来给
/// 「文件已变化,请刷新后重试」并提供刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleConflictFile;

impl fmt::Display for StaleConflictFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("文件已变化,请刷新后重试")
    }
}

impl std::error::Error for StaleConflictFile {}

/// 一个冲突文件能不能逐段解决(更改列表随列表一起取,菜单项按它启用 / 写明原因)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentStatus {
    /// 能:文件里有 `blocks` 段成对的冲突。
    Ready {
        blocks: usize,
    },
    Blocked(SegmentBlocker),
}

/// 解析好的冲突文件:解码后的全文、各段冲突,以及解析时读到的原字节(写回前防陈旧比对)。
#[derive(Debug, Clone)]
pub struct ConflictFile {
    /// 仓库内相对路径(与暂存区里的写法一致)。
    pub path: String,
    /// 识别出的编码;写回按它编码。一定可写([`TextEncoding::writable`])。
    pub encoding: TextEncoding,
    /// 解码后的全文(UTF-8 的 BOM 留在开头)。
    pub text: String,
    /// 按位置排列的各段冲突,至少一段。
    pub blocks: Vec<ConflictBlock>,
    /// 解析时读到的原字节。
    original: Vec<u8>,
}

impl ConflictFile {
    /// 某段当前这一侧的内容。
    pub fn ours_text(&self, block: &ConflictBlock) -> &str {
        &self.text[block.ours.clone()]
    }

    /// 某段对方的内容。
    pub fn theirs_text(&self, block: &ConflictBlock) -> &str {
        &self.text[block.theirs.clone()]
    }

    /// 某段共同祖先的内容;没有 base 段时为 `None`。
    pub fn base_text(&self, block: &ConflictBlock) -> Option<&str> {
        block.base.clone().map(|range| &self.text[range])
    }

    /// 按各段的选择拼出的新全文(见 [`apply_conflict_picks`])。
    pub fn resolve(&self, picks: &[Option<ConflictPick>]) -> String {
        apply_conflict_picks(&self.text, &self.blocks, picks)
    }
}

/// 读工作区里的冲突文件并解码。`keep` 时连原字节一起交回(写回前防陈旧用)。
fn read_decoded(
    abs: &Path,
    keep: bool,
) -> std::result::Result<(String, TextEncoding, Vec<u8>), SegmentBlocker> {
    // 符号链接不跟:冲突在链接上时「内容」是链接目标的路径,改名写回会把链接换成普通文件
    let meta = std::fs::symlink_metadata(abs).map_err(|_| SegmentBlocker::Unreadable)?;
    if !meta.is_file() {
        return Err(SegmentBlocker::Unreadable);
    }
    if meta.len() > MAX_SEGMENT_FILE_BYTES {
        return Err(SegmentBlocker::TooLarge);
    }
    let bytes = std::fs::read(abs).map_err(|_| SegmentBlocker::Unreadable)?;
    // 量完大小到读进来之间又长大了
    if bytes.len() as u64 > MAX_SEGMENT_FILE_BYTES {
        return Err(SegmentBlocker::TooLarge);
    }
    let original = if keep { bytes.clone() } else { Vec::new() };
    match crate::encoding::decode(bytes) {
        Decoded::Binary => Err(SegmentBlocker::Binary),
        Decoded::Text {
            encoding: TextEncoding::Lossy,
            ..
        } => Err(SegmentBlocker::Lossy),
        Decoded::Text { text, encoding } => Ok((text, encoding, original)),
    }
}

/// 某个冲突文件能不能逐段解决。**阻塞**(读一遍文件),调用方放后台。
///
/// `repo_path` 是仓库根(`info.path` 相对于它)。删除 / 修改冲突不读文件直接给原因。
pub fn conflict_segment_status(repo_path: &Path, info: &ConflictInfo) -> SegmentStatus {
    if !(info.ours && info.theirs) {
        return SegmentStatus::Blocked(SegmentBlocker::DeleteModify);
    }
    match read_decoded(&repo_path.join(&info.path), false) {
        Ok((text, _, _)) => match find_conflict_blocks(&text).len() {
            0 => SegmentStatus::Blocked(SegmentBlocker::NoMarkers),
            blocks => SegmentStatus::Ready { blocks },
        },
        Err(blocker) => SegmentStatus::Blocked(blocker),
    }
}

/// 暂存区里该文件的三方条目(不在冲突中时报 [`SegmentBlocker::NotConflicted`])。
fn conflict_entry(repo_path: &Path, file: &str) -> Result<ConflictInfo> {
    list_conflicts(repo_path)?
        .into_iter()
        .find(|c| c.path == file)
        .ok_or_else(|| anyhow!(SegmentBlocker::NotConflicted))
}

/// 读出并解析一个冲突文件,供逐段解决。`repo_path` 是仓库根,`file` 是暂存区里的相对路径。
///
/// # Errors
///
/// 不能逐段解决时报 [`SegmentBlocker`](`err.downcast_ref` 认得出);仓库打不开等照常报错。
pub fn load_conflict_file(repo_path: &Path, file: &str) -> Result<ConflictFile> {
    let info = conflict_entry(repo_path, file)?;
    if !(info.ours && info.theirs) {
        return Err(SegmentBlocker::DeleteModify.into());
    }
    let (text, encoding, original) = read_decoded(&repo_path.join(file), true)?;
    let blocks = find_conflict_blocks(&text);
    if blocks.is_empty() {
        return Err(SegmentBlocker::NoMarkers.into());
    }
    Ok(ConflictFile {
        path: file.to_string(),
        encoding,
        text,
        blocks,
        original,
    })
}

/// 按各段的选择写回工作区文件(口径见模块注释「写回」)。`picks[i]` 对应
/// `file.blocks[i]`,`None` 的那段原样保留标记。`mark_resolved` = 写完接着标记为已解决
/// (此时每段都必须选了)。
///
/// # Errors
///
/// - 文件在解析之后被改过:[`StaleConflictFile`],文件一个字节不动;
/// - 已经不在冲突中:[`SegmentBlocker::NotConflicted`];
/// - 编码表示不了拼出来的文本:[`crate::encoding::EncodeError`](按段拼接只会用到原文里的
///   字符,正常走不到,兜底);
/// - 选择与段数对不上、一段都没选、标记已解决时还有段没选:直接报错。
pub fn write_conflict_resolution(
    repo_path: &Path,
    file: &ConflictFile,
    picks: &[Option<ConflictPick>],
    mark_resolved: bool,
) -> Result<()> {
    if picks.len() != file.blocks.len() {
        bail!("选择与冲突段对不上,请刷新后重试");
    }
    if picks.iter().all(Option::is_none) {
        bail!("还没有选择任何冲突段");
    }
    if mark_resolved && picks.iter().any(Option::is_none) {
        bail!("还有冲突段没有选择,不能标记为已解决");
    }
    conflict_entry(repo_path, &file.path)?;
    let bytes = file.encoding.encode(&file.resolve(picks))?;
    let abs = repo_path.join(&file.path);
    // 防陈旧:长度不同不必读完整个文件;读不到(被删了)同样算变了
    let meta = std::fs::symlink_metadata(&abs).map_err(|_| StaleConflictFile)?;
    if !meta.is_file() || meta.len() != file.original.len() as u64 {
        return Err(StaleConflictFile.into());
    }
    let now = std::fs::read(&abs).map_err(|_| StaleConflictFile)?;
    if now != file.original {
        return Err(StaleConflictFile.into());
    }
    crate::fs::atomic_write(&abs, &bytes)?;
    if mark_resolved {
        mark_conflict_resolved(repo_path, &file.path)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "conflict_blocks_tests.rs"]
mod tests;
