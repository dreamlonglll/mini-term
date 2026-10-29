//! 按块暂存 / 取消暂存 / 丢弃(单文件 diff 弹窗里每个 hunk 头上的三个按钮)。
//!
//! # 用户看到的块就是被应用的块
//!
//! **不走 patch**(`git apply --cached`):那条路要把自写 LCS 算出来的 hunk 再翻成
//! unified diff 交给 git 重新定位,上下文行数 / 行号 / 换行符 / 末尾换行任何一处与
//! git 的理解有出入,落下去的就不是用户眼前那一块 —— 而且失败时只有一句
//! `patch does not apply`。这里直接拿 diff 两侧的**全文**与该 hunk 每一行的精确行号
//! (`DiffLine::old_lineno` / `new_lineno`,逐行都是准的;hunk 头的区间只在「纯插入、
//! 没有任何一侧的行可锚定」时兜底用)做纯文本拼接:
//!
//! | 操作 | 改哪一侧 | 怎么拼 |
//! |---|---|---|
//! | 暂存此块 | 暂存区(`DiffSide::Unstaged` 的旧侧) | 旧侧 + 该块 → 写回 index 条目 |
//! | 取消暂存此块 | 暂存区(`DiffSide::Staged` 的新侧) | 新侧里把该块反向还原 → 写回 index |
//! | 丢弃此块 | 工作区(`DiffSide::Unstaged` 的新侧) | 新侧里把该块反向还原 → 写回文件 |
//!
//! 拼接是纯函数 [`splice_hunk`],单测覆盖首行 / 末行 / 纯增 / 纯删 / 多块取中间 /
//! 相邻块 / CRLF / 无末尾换行。
//!
//! # 防陈旧
//!
//! 执行前按同一口径**重读两侧**,与 diff 计算时的全文(`GitDiffResult` 的
//! `old_content` / `new_content`)逐字节比对,不一致就拒绝并返回 [`StaleDiff`]
//! (UI 认这个类型给「文件已变化,请刷新后重试」)。块里每一行的内容也会与对应行号上
//! 的文本对账 —— 两侧全文一致 + 行行对得上,才能保证落下去的正是用户看到的那一块。
//!
//! # 换行与编码
//!
//! diff 按 `str::lines()` 切行,CRLF 与 LF 在 diff 里不可见(Windows 上
//! `core.autocrlf=true` 时暂存区是 LF、工作区是 CRLF,这是常态)。拼接时:
//!
//! - 块以外的行、块里的上下文行**原样保留字节**(含它们自己的换行符);
//! - 从另一侧搬进来的行,换行符一律改用**被写那一侧**的风格(该侧占多数的那种;
//!   该侧一个换行都没有时按 `core.autocrlf` 推断,见 [`fallback_eol`]);
//! - 末尾换行:块一直延伸到两侧文件末尾时,结果的「末行有没有换行」跟随来源侧 ——
//!   与 git 的 patch 语义一致(`\ No newline at end of file` 本来就是末行的一部分)。
//!   块没碰到末尾就一个字节都不动。

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use git2::{Index, IndexEntry, IndexTime, Oid, Repository};
use pathdiff::diff_paths;

use super::{DiffHunk, GitDiffResult, discover_repo_for, get_head_content, is_conflicted};

/// diff 已过期:取 diff 之后文件或暂存区又变了(或者块与全文对不上)。
///
/// 单独一个类型而不是一句字符串:UI 用 `err.is::<StaleDiff>()` 认出来,给出本地化的
/// 「文件已变化,请刷新后重试」,而不是把后端的中文原文直接摆出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleDiff;

impl fmt::Display for StaleDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("文件已变化,请刷新后重试")
    }
}

impl std::error::Error for StaleDiff {}

/// 把块用在哪个方向上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkDirection {
    /// 旧侧 + 块 → 新的旧侧(暂存此块:暂存区里补上这一块)。
    Apply,
    /// 新侧 − 块 → 新的新侧(取消暂存此块 / 丢弃此块:把这一块还原成旧侧的样子)。
    Revert,
}

/// 一行原文:内容(不含换行符)+ 它自己的换行符(`"\n"` / `"\r\n"`;末行无换行时 `""`)。
///
/// 内容的切法与 `str::lines()` 逐字一致(先剥 `\n` 再剥紧挨着的 `\r`)—— diff 就是
/// 按 `lines()` 算的,行号与内容才对得上;换行符留着,拼回去时块外的字节才能原样不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawLine<'a> {
    text: &'a str,
    eol: &'a str,
}

fn split_raw(s: &str) -> Vec<RawLine<'_>> {
    s.split_inclusive('\n')
        .map(|piece| {
            if let Some(text) = piece.strip_suffix("\r\n") {
                RawLine { text, eol: "\r\n" }
            } else if let Some(text) = piece.strip_suffix('\n') {
                RawLine { text, eol: "\n" }
            } else {
                RawLine {
                    text: piece,
                    eol: "",
                }
            }
        })
        .collect()
}

/// 这段文本占多数的换行风格;一个换行都没有就是 `None`。平局算 LF。
fn dominant_eol(lines: &[RawLine<'_>]) -> Option<&'static str> {
    let crlf = lines.iter().filter(|l| l.eol == "\r\n").count();
    let lf = lines.iter().filter(|l| l.eol == "\n").count();
    match (crlf, lf) {
        (0, 0) => None,
        (crlf, lf) if crlf > lf => Some("\r\n"),
        _ => Some("\n"),
    }
}

/// 结果里的一行从哪来。
enum Piece {
    /// 被写那一侧原有的第 n 行(0 起),字节原样保留
    Keep(usize),
    /// 另一侧的第 n 行,换行符改成被写那一侧的风格
    Insert(usize),
}

/// 1 起的行号 → 0 起的下标,并确认落在 `len` 以内。
fn line_index(no: Option<u32>, len: usize) -> Result<usize> {
    match no {
        Some(n) if n >= 1 && (n as usize) <= len => Ok(n as usize - 1),
        _ => Err(StaleDiff.into()),
    }
}

/// 同一侧的行号必须逐行连续(块里不许跳行,否则拼出来的东西没有定义)。
fn advance(next: &mut Option<usize>, first: &mut Option<usize>, ix: usize) -> Result<()> {
    if let Some(expected) = *next
        && expected != ix
    {
        return Err(StaleDiff.into());
    }
    if first.is_none() {
        *first = Some(ix);
    }
    *next = Some(ix + 1);
    Ok(())
}

/// 按块拼接的纯函数:`old` / `new` 是 diff 两侧的全文,`hunk` 是其中一块。
///
/// - [`HunkDirection::Apply`]:返回「`old` 里补上这一块」之后的 `old`;
/// - [`HunkDirection::Revert`]:返回「`new` 里把这一块还原」之后的 `new`。
///
/// `fallback_eol` 只在被写那一侧一个换行都没有时才用(空文件 / 单行文件)。
///
/// 块里的每一行都会与对应行号上的文本对账,对不上(或行号不连续)就返回
/// [`StaleDiff`] —— 只有「块与全文完全吻合」时才有资格落盘。
pub fn splice_hunk(
    old: &str,
    new: &str,
    hunk: &DiffHunk,
    dir: HunkDirection,
    fallback_eol: &str,
) -> Result<String> {
    let old_lines = split_raw(old);
    let new_lines = split_raw(new);
    // target = 被写的那一侧,source = 搬行过来的那一侧
    let (target, source) = match dir {
        HunkDirection::Apply => (&old_lines, &new_lines),
        HunkDirection::Revert => (&new_lines, &old_lines),
    };
    // 「另一侧有、这一侧没有」的行要搬进来;「这一侧有、另一侧没有」的行要拿掉
    let (insert_kind, drop_kind) = match dir {
        HunkDirection::Apply => ("add", "delete"),
        HunkDirection::Revert => ("delete", "add"),
    };
    let target_no = |line: &super::DiffLine| match dir {
        HunkDirection::Apply => line.old_lineno,
        HunkDirection::Revert => line.new_lineno,
    };
    let source_no = |line: &super::DiffLine| match dir {
        HunkDirection::Apply => line.new_lineno,
        HunkDirection::Revert => line.old_lineno,
    };

    let mut pieces: Vec<Piece> = Vec::with_capacity(hunk.lines.len());
    let (mut target_first, mut target_next) = (None, None);
    let (mut source_first, mut source_next) = (None, None);
    for line in &hunk.lines {
        let kind = line.kind.as_str();
        if kind == "context" {
            let t = line_index(target_no(line), target.len())?;
            let s = line_index(source_no(line), source.len())?;
            if target[t].text != line.content || source[s].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut target_next, &mut target_first, t)?;
            advance(&mut source_next, &mut source_first, s)?;
            pieces.push(Piece::Keep(t));
        } else if kind == drop_kind {
            let t = line_index(target_no(line), target.len())?;
            if target[t].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut target_next, &mut target_first, t)?;
        } else if kind == insert_kind {
            let s = line_index(source_no(line), source.len())?;
            if source[s].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut source_next, &mut source_first, s)?;
            pieces.push(Piece::Insert(s));
        } else {
            return Err(StaleDiff.into());
        }
    }

    // 块在被写那一侧占的区间。一行都没占(纯插入)时只能看 hunk 头 ——
    // 头里的起点是「插在第几行之前」(1 起),见 `build_hunks`
    let header_start = |start: u32, len: usize| (start.max(1) as usize - 1).min(len);
    let (target_header, source_header) = match dir {
        HunkDirection::Apply => (hunk.old_start, hunk.new_start),
        HunkDirection::Revert => (hunk.new_start, hunk.old_start),
    };
    let target_begin = target_first.unwrap_or_else(|| header_start(target_header, target.len()));
    let target_end = target_next.unwrap_or(target_begin);
    let source_begin = source_first.unwrap_or_else(|| header_start(source_header, source.len()));
    let source_end = source_next.unwrap_or(source_begin);
    // 块碰到了两侧的文件末尾:末行有没有换行要跟随来源侧(见模块注释)
    let at_eof = target_end == target.len() && source_end == source.len();

    let eol = dominant_eol(target).unwrap_or(fallback_eol);
    let mut out: Vec<(&str, &str)> = Vec::with_capacity(target.len() + pieces.len());
    out.extend(target[..target_begin].iter().map(|l| (l.text, l.eol)));
    let hunk_from = out.len();
    for piece in &pieces {
        out.push(match *piece {
            Piece::Keep(t) => (target[t].text, target[t].eol),
            Piece::Insert(s) => (source[s].text, eol),
        });
    }
    let hunk_to = out.len();
    out.extend(target[target_end..].iter().map(|l| (l.text, l.eol)));

    // 原先的末行(没有换行)被挤到了中间:补上换行,否则两行会粘成一行。
    // 只可能发生在块的范围里 —— 块没碰到末尾时原末行仍是末行
    let last = out.len().saturating_sub(1);
    for (i, line) in out.iter_mut().enumerate() {
        if i != last && line.1.is_empty() {
            line.1 = eol;
        }
    }
    // 末行换行跟随来源侧 —— 只在结果的末行正是块里的行时才动(块外一个字节都不碰)
    if at_eof && hunk_to > hunk_from && hunk_to == out.len() {
        let source_has_final_eol = source.last().is_none_or(|l| !l.eol.is_empty());
        let tail = &mut out[last];
        if !source_has_final_eol {
            tail.1 = "";
        } else if tail.1.is_empty() {
            tail.1 = eol;
        }
    }

    let mut text = String::with_capacity(old.len().max(new.len()) + 16);
    for (body, line_eol) in out {
        text.push_str(body);
        text.push_str(line_eol);
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// `core.autocrlf` 的三档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCrlf {
    False,
    True,
    Input,
}

fn autocrlf(repo: &Repository) -> AutoCrlf {
    let Ok(config) = repo.config() else {
        return AutoCrlf::False;
    };
    match config.get_string("core.autocrlf") {
        Ok(v) if v.eq_ignore_ascii_case("input") => AutoCrlf::Input,
        _ if config.get_bool("core.autocrlf").unwrap_or(false) => AutoCrlf::True,
        _ => AutoCrlf::False,
    }
}

/// 被写那一侧一个换行都没有(空文件 / 单行文件)时,搬进来的行用什么换行。
///
/// - 写暂存区:开了 autocrlf(true / input)时暂存区是规范化的 LF;否则 git 原样存字节,
///   沿用来源侧(工作区)的风格;
/// - 写工作区:autocrlf=true 时检出就是 CRLF;否则沿用来源侧(暂存区)的风格。
fn fallback_eol(repo: &Repository, writing_index: bool, source: &str) -> &'static str {
    let mode = autocrlf(repo);
    if writing_index && mode != AutoCrlf::False {
        return "\n";
    }
    if !writing_index && mode == AutoCrlf::True {
        return "\r\n";
    }
    dominant_eol(&split_raw(source)).unwrap_or("\n")
}

/// 定位到文件所属仓库 + 仓库内相对路径(与 `get_git_diff` 同一口径)。
struct Located {
    repo: Repository,
    rel: String,
    abs: PathBuf,
}

fn locate(project_path: &Path, file_path: &str) -> Result<Located> {
    let abs = project_path.join(file_path);
    let repo = discover_repo_for(project_path, &abs)
        .ok_or_else(|| anyhow!("找不到 {} 所在的 git 仓库", abs.display()))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repository not supported"))?;
    let rel = diff_paths(&abs, workdir)
        .ok_or_else(|| anyhow!("file is outside repository working directory"))?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Located { repo, rel, abs })
}

/// diff 本身能不能按块操作、以及要操作的那一块。
fn pick_hunk(diff: &GitDiffResult, hunk_index: usize) -> Result<&DiffHunk> {
    if diff.is_binary || diff.too_large {
        bail!("二进制 / 过大的文件只能整文件操作");
    }
    diff.hunks.get(hunk_index).ok_or_else(|| StaleDiff.into())
}

/// 暂存区里该路径的 stage 0 条目 —— 只认普通文件(符号链接 / 子模块的「内容」不是文本)。
fn regular_entry(index: &Index, rel: &str) -> Result<IndexEntry> {
    let entry = index
        .get_path(Path::new(rel), 0)
        .ok_or_else(|| anyhow!("{rel} 不在暂存区里,只能整文件操作"))?;
    if !matches!(entry.mode, 0o100644 | 0o100755) {
        bail!("{rel} 不是普通文件(符号链接 / 子模块),不支持按块操作");
    }
    Ok(entry)
}

fn blob_text(repo: &Repository, id: Oid) -> Result<String> {
    let blob = repo.find_blob(id)?;
    match std::str::from_utf8(blob.content()) {
        Ok(s) if !blob.is_binary() => Ok(s.to_string()),
        _ => Err(StaleDiff.into()),
    }
}

/// 工作区当前内容。长度与 diff 那一份不一致时不必读完整个大文件就能判陈旧。
fn worktree_text(abs: &Path, expected_len: usize) -> Result<String> {
    let meta = std::fs::symlink_metadata(abs)
        .map_err(|_| anyhow!("工作区里没有 {},只能整文件操作", abs.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("{} 不是普通文件,不支持按块操作", abs.display());
    }
    if meta.len() as usize != expected_len {
        return Err(StaleDiff.into());
    }
    String::from_utf8(std::fs::read(abs)?).map_err(|_| StaleDiff.into())
}

fn ensure_fresh(diff: &GitDiffResult, old: &str, new: &str) -> Result<()> {
    if diff.old_content == old && diff.new_content == new {
        Ok(())
    } else {
        Err(StaleDiff.into())
    }
}

/// `git add -N` 留下的「意图添加」标记(`GIT_INDEX_ENTRY_INTENT_TO_ADD`)。
/// 条目一旦有了真内容就该摘掉。
const INTENT_TO_ADD: u16 = 1 << 13;

/// 把 `text` 写成该路径新的 index 条目(保留原 mode / 标志位)。
///
/// stat 信息全部清零:旧条目的 mtime / size 记的是**上一次** `git add` 时的工作区,
/// 留着的话 git 可能凭 stat 认定「工作区与暂存区一致」而跳过内容比对 —— 部分暂存
/// 之后两者明明不一样。清零后 git / libgit2 下次一定会重新比内容。
fn write_index_text(index: &mut Index, old: IndexEntry, text: &str) -> Result<()> {
    let entry = IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: old.mode,
        uid: 0,
        gid: 0,
        file_size: text.len() as u32,
        id: Oid::zero(),
        flags: old.flags,
        flags_extended: old.flags_extended & !INTENT_TO_ADD,
        path: old.path,
    };
    index.add_frombuffer(&entry, text.as_bytes())?;
    index.write()?;
    Ok(())
}

fn refuse_conflicted(loc: &Located) -> Result<()> {
    if is_conflicted(&loc.repo, &loc.rel)? {
        bail!("冲突中的文件不支持按块操作,请先解决冲突");
    }
    Ok(())
}

/// 暂存此块:`diff` 必须是 [`super::DiffSide::Unstaged`] 那一侧(暂存区 → 工作区)。
///
/// `project_path` 与 `get_git_diff` 一样可以是仓库内的子目录。
pub fn stage_hunk(
    project_path: &Path,
    file_path: &str,
    diff: &GitDiffResult,
    hunk_index: usize,
) -> Result<()> {
    let hunk = pick_hunk(diff, hunk_index)?;
    let loc = locate(project_path, file_path)?;
    refuse_conflicted(&loc)?;
    let mut index = loc.repo.index()?;
    let entry = regular_entry(&index, &loc.rel)?;
    let staged = blob_text(&loc.repo, entry.id)?;
    let worktree = worktree_text(&loc.abs, diff.new_content.len())?;
    ensure_fresh(diff, &staged, &worktree)?;
    let fallback = fallback_eol(&loc.repo, true, &worktree);
    let next = splice_hunk(&staged, &worktree, hunk, HunkDirection::Apply, fallback)?;
    write_index_text(&mut index, entry, &next)
}

/// 取消暂存此块:`diff` 必须是 [`super::DiffSide::Staged`] 那一侧(HEAD → 暂存区)。
///
/// 新增文件(HEAD 里没有)的最后一块也取消之后,暂存区里只剩一个空文件 —— 那不是
/// 「取消暂存」该有的样子,这时直接把它移出暂存区(回到未跟踪),与整文件取消暂存一致。
pub fn unstage_hunk(
    project_path: &Path,
    file_path: &str,
    diff: &GitDiffResult,
    hunk_index: usize,
) -> Result<()> {
    let hunk = pick_hunk(diff, hunk_index)?;
    let loc = locate(project_path, file_path)?;
    refuse_conflicted(&loc)?;
    let mut index = loc.repo.index()?;
    let entry = regular_entry(&index, &loc.rel)?;
    let staged = blob_text(&loc.repo, entry.id)?;
    let head = get_head_content(&loc.repo, &loc.rel)
        .map_err(|_| anyhow::Error::from(StaleDiff))?
        .unwrap_or_default();
    ensure_fresh(diff, &head, &staged)?;
    let fallback = fallback_eol(&loc.repo, true, &head);
    let next = splice_hunk(&head, &staged, hunk, HunkDirection::Revert, fallback)?;
    if next.is_empty() && !in_head(&loc.repo, &loc.rel) {
        index.remove_path(Path::new(&loc.rel))?;
        index.write()?;
        return Ok(());
    }
    write_index_text(&mut index, entry, &next)
}

/// 丢弃此块:`diff` 必须是 [`super::DiffSide::Unstaged`] 那一侧;只改工作区文件,
/// 暂存区不动。**不可恢复**,UI 先确认。
pub fn discard_hunk(
    project_path: &Path,
    file_path: &str,
    diff: &GitDiffResult,
    hunk_index: usize,
) -> Result<()> {
    let hunk = pick_hunk(diff, hunk_index)?;
    let loc = locate(project_path, file_path)?;
    refuse_conflicted(&loc)?;
    let index = loc.repo.index()?;
    let entry = regular_entry(&index, &loc.rel)?;
    let staged = blob_text(&loc.repo, entry.id)?;
    let worktree = worktree_text(&loc.abs, diff.new_content.len())?;
    ensure_fresh(diff, &staged, &worktree)?;
    let fallback = fallback_eol(&loc.repo, false, &staged);
    let next = splice_hunk(&staged, &worktree, hunk, HunkDirection::Revert, fallback)?;
    // 原地覆写(不走「写临时文件再改名」):改名会换掉文件身份与权限位,
    // 编辑器里开着的这个文件也会被当成「被删后重建」
    std::fs::write(&loc.abs, next)?;
    Ok(())
}

fn in_head(repo: &Repository, rel: &str) -> bool {
    repo.head()
        .ok()
        .and_then(|h| h.peel_to_tree().ok())
        .is_some_and(|tree| tree.get_path(Path::new(rel)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::init_repo_with_file;
    use super::super::{DiffSide, diff_two_texts, get_git_diff, git_stage};
    use super::*;

    /// 两侧文本 → diff → 对第 `ix` 块做 `dir` 方向的拼接。
    fn splice(old: &str, new: &str, ix: usize, dir: HunkDirection) -> String {
        let diff = diff_two_texts(old.to_string(), new.to_string());
        splice_hunk(old, new, &diff.hunks[ix], dir, "\n").unwrap()
    }

    fn hunk_count(old: &str, new: &str) -> usize {
        diff_two_texts(old.to_string(), new.to_string()).hunks.len()
    }

    /// 编号行 `l1\n` … `l{n}\n`。
    fn numbered(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("l{i}")).collect()
    }

    fn join(lines: &[String], eol: &str) -> String {
        lines.iter().map(|l| format!("{l}{eol}")).collect()
    }

    /// 切行口径必须与 `str::lines()` 逐字一致 —— diff 的行号与内容就是按它算的。
    #[test]
    fn 切行与_lines_一致() {
        for s in [
            "",
            "a",
            "a\n",
            "a\r\n",
            "a\nb",
            "a\r\nb\r\n",
            "\n\n",
            "a\r",
            "a\rb\n",
            "x\r\r\n",
        ] {
            let ours: Vec<&str> = split_raw(s).iter().map(|l| l.text).collect();
            let std: Vec<&str> = s.lines().collect();
            assert_eq!(ours, std, "输入 {s:?}");
            let rebuilt: String = split_raw(s)
                .iter()
                .map(|l| format!("{}{}", l.text, l.eol))
                .collect();
            assert_eq!(rebuilt, s, "拼回去必须一字不差");
        }
    }

    /// 单块的文件:正向应用 = 新侧,反向还原 = 旧侧。首行块、末行块各来一遍。
    #[test]
    fn 首行块与末行块() {
        let base = numbered(10);
        let mut first = base.clone();
        first[0] = "FIRST".into();
        let mut last = base.clone();
        last[9] = "LAST".into();
        let old = join(&base, "\n");
        for changed in [first, last] {
            let new = join(&changed, "\n");
            assert_eq!(hunk_count(&old, &new), 1);
            assert_eq!(splice(&old, &new, 0, HunkDirection::Apply), new);
            assert_eq!(splice(&old, &new, 0, HunkDirection::Revert), old);
        }
    }

    /// 纯新增块(中间插两行)与纯删除块(中间删两行)。
    #[test]
    fn 纯新增块与纯删除块() {
        let base = numbered(12);
        let mut added = base.clone();
        added.insert(6, "new-a".into());
        added.insert(7, "new-b".into());
        let old = join(&base, "\n");
        let new = join(&added, "\n");
        assert_eq!(splice(&old, &new, 0, HunkDirection::Apply), new);
        assert_eq!(splice(&old, &new, 0, HunkDirection::Revert), old);
        // 反过来就是纯删除
        assert_eq!(splice(&new, &old, 0, HunkDirection::Apply), old);
        assert_eq!(splice(&new, &old, 0, HunkDirection::Revert), new);
    }

    /// 文件开头 / 空文件上的纯插入:没有任何一侧的行可锚定,只能靠 hunk 头。
    #[test]
    fn 空文件上的纯新增() {
        assert_eq!(splice("", "a\nb\n", 0, HunkDirection::Apply), "a\nb\n");
        assert_eq!(splice("", "a\nb\n", 0, HunkDirection::Revert), "");
        assert_eq!(splice("a\nb\n", "", 0, HunkDirection::Apply), "");
        assert_eq!(splice("a\nb\n", "", 0, HunkDirection::Revert), "a\nb\n");
        // 开头插入(后面有上下文)
        let old = join(&numbered(8), "\n");
        let new = format!("top\n{old}");
        assert_eq!(splice(&old, &new, 0, HunkDirection::Apply), new);
        assert_eq!(splice(&old, &new, 0, HunkDirection::Revert), old);
    }

    /// 三块里只动中间那一块:其余两块原样不动。
    #[test]
    fn 多块只动中间一块() {
        let base = numbered(40);
        let mut changed = base.clone();
        changed[2] = "A".into();
        changed[20] = "B".into();
        changed[37] = "C".into();
        let old = join(&base, "\n");
        let new = join(&changed, "\n");
        assert_eq!(hunk_count(&old, &new), 3);

        let mut only_mid = base.clone();
        only_mid[20] = "B".into();
        assert_eq!(
            splice(&old, &new, 1, HunkDirection::Apply),
            join(&only_mid, "\n")
        );

        let mut reverted_mid = changed.clone();
        reverted_mid[20] = "l21".into();
        assert_eq!(
            splice(&old, &new, 1, HunkDirection::Revert),
            join(&reverted_mid, "\n")
        );
    }

    /// 相邻块(上下文刚好不重叠,分成两块):动第一块不碰第二块,反之亦然。
    #[test]
    fn 相邻块互不影响() {
        let base = numbered(20);
        let mut changed = base.clone();
        changed[3] = "X".into();
        changed[11] = "Y".into(); // 中间隔 7 行 > 2×3 行上下文
        let old = join(&base, "\n");
        let new = join(&changed, "\n");
        assert_eq!(hunk_count(&old, &new), 2);
        let mut first_only = base.clone();
        first_only[3] = "X".into();
        assert_eq!(
            splice(&old, &new, 0, HunkDirection::Apply),
            join(&first_only, "\n")
        );
        let mut second_only = base.clone();
        second_only[11] = "Y".into();
        assert_eq!(
            splice(&old, &new, 1, HunkDirection::Apply),
            join(&second_only, "\n")
        );
        // 两块先后应用 = 新侧
        let step = splice(&old, &new, 0, HunkDirection::Apply);
        let diff = diff_two_texts(step.clone(), new.clone());
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(
            splice_hunk(&step, &new, &diff.hunks[0], HunkDirection::Apply, "\n").unwrap(),
            new
        );
    }

    /// 工作区 CRLF、暂存区 LF(autocrlf=true 的常态):
    /// 暂存块写回暂存区用 LF,丢弃块写回工作区用 CRLF,块外字节一个不动。
    #[test]
    fn crlf_工作区与_lf_暂存区() {
        let base = numbered(20);
        let mut changed = base.clone();
        changed[2] = "A".into();
        changed.insert(15, "inserted".into());
        let index = join(&base, "\n");
        let worktree = join(&changed, "\r\n");
        let diff = diff_two_texts(index.clone(), worktree.clone());
        assert_eq!(diff.hunks.len(), 2, "CRLF 本身不算改动");

        // 暂存第二块(纯插入):暂存区仍是纯 LF
        let staged = splice_hunk(
            &index,
            &worktree,
            &diff.hunks[1],
            HunkDirection::Apply,
            "\n",
        )
        .unwrap();
        let mut expect = base.clone();
        expect.insert(15, "inserted".into());
        assert_eq!(staged, join(&expect, "\n"));
        assert!(!staged.contains('\r'));

        // 丢弃第一块:工作区仍是纯 CRLF,只有那一行回到旧值
        let discarded = splice_hunk(
            &index,
            &worktree,
            &diff.hunks[0],
            HunkDirection::Revert,
            "\n",
        )
        .unwrap();
        let mut expect = changed.clone();
        expect[2] = "l3".into();
        assert_eq!(discarded, join(&expect, "\r\n"));
    }

    /// 混合换行的文件:块外每一行的换行符原样保留。
    #[test]
    fn 混合换行块外字节不动() {
        let old = "a\r\nb\nc\r\nd\ne\r\nf\ng\r\nh\n";
        let new = "a\r\nb\nc\r\nD\ne\r\nf\ng\r\nh\n";
        let out = splice(old, new, 0, HunkDirection::Apply);
        assert_eq!(out, "a\r\nb\nc\r\nD\ne\r\nf\ng\r\nh\n");
    }

    /// 末尾无换行的几种形态:结果的末行换行跟随来源侧,块没碰到末尾就不动。
    #[test]
    fn 无末尾换行() {
        // 新侧在末尾追加了一行且无换行
        assert_eq!(
            splice("a\nb\n", "a\nb\nc", 0, HunkDirection::Apply),
            "a\nb\nc"
        );
        assert_eq!(
            splice("a\nb\n", "a\nb\nc", 0, HunkDirection::Revert),
            "a\nb\n"
        );
        // 旧侧无末尾换行,新侧追加一行带换行:原末行要补上换行
        assert_eq!(
            splice("a\nb", "a\nb\nc\n", 0, HunkDirection::Apply),
            "a\nb\nc\n"
        );
        assert_eq!(
            splice("a\nb", "a\nb\nc\n", 0, HunkDirection::Revert),
            "a\nb"
        );
        // 改的是末行本身,两侧都无末尾换行
        assert_eq!(splice("a\nb", "a\nB", 0, HunkDirection::Apply), "a\nB");
        assert_eq!(splice("a\nb", "a\nB", 0, HunkDirection::Revert), "a\nb");
        // 块不碰末尾:末尾无换行的状态原样保留
        let base = numbered(12);
        let old = format!("{}l13", join(&base, "\n"));
        let new = old.replacen("l2\n", "L2\n", 1);
        assert_eq!(splice(&old, &new, 0, HunkDirection::Apply), new);
        assert!(!splice(&old, &new, 0, HunkDirection::Revert).ends_with('\n'));
    }

    /// 块与全文对不上(被篡改 / 行号越界 / 跳行)一律按陈旧拒绝,不许落盘。
    #[test]
    fn 块与全文对不上就拒绝() {
        let old = join(&numbered(10), "\n");
        let new = old.replacen("l5\n", "L5\n", 1);
        let diff = diff_two_texts(old.clone(), new.clone());
        let mut hunk = diff.hunks[0].clone();
        hunk.lines[0].content = "tampered".into();
        let err = splice_hunk(&old, &new, &hunk, HunkDirection::Apply, "\n").unwrap_err();
        assert!(err.is::<StaleDiff>());

        let mut hunk = diff.hunks[0].clone();
        hunk.lines[0].old_lineno = Some(99);
        assert!(splice_hunk(&old, &new, &hunk, HunkDirection::Apply, "\n").is_err());

        let mut hunk = diff.hunks[0].clone();
        hunk.lines.remove(1);
        let err = splice_hunk(&old, &new, &hunk, HunkDirection::Apply, "\n").unwrap_err();
        assert!(err.is::<StaleDiff>(), "跳行 = 块不连续");
        // 两侧换个文本:块就对不上了
        let other = join(&numbered(10), "\n").replacen("l2\n", "x\n", 1);
        assert!(splice_hunk(&other, &new, &diff.hunks[0], HunkDirection::Apply, "\n").is_err());
    }

    // ── 真仓库 ──────────────────────────────────────────────

    fn changed(result: &GitDiffResult) -> Vec<(String, String)> {
        result
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind != "context")
            .map(|l| (l.kind.clone(), l.content.clone()))
            .collect()
    }

    fn pair(kind: &str, content: &str) -> (String, String) {
        (kind.to_string(), content.to_string())
    }

    fn index_blob(root: &Path, rel: &str) -> String {
        let repo = Repository::open(root).unwrap();
        let index = repo.index().unwrap();
        let entry = index.get_path(Path::new(rel), 0).unwrap();
        String::from_utf8(repo.find_blob(entry.id).unwrap().content().to_vec()).unwrap()
    }

    /// 两块改动,暂存第一块:「已暂存」只剩第一块,「未暂存」只剩第二块。
    /// 再取消暂存它:回到全部未暂存。
    #[test]
    fn 暂存块后两侧_diff_符合预期() {
        let rel = "src/a.txt";
        let base = join(&numbered(30), "\n");
        let root = init_repo_with_file("hunk-stage", rel, &base);
        let edited = base
            .replacen("l3\n", "THREE\n", 1)
            .replacen("l25\n", "TWENTY-FIVE\n", 1);
        std::fs::write(root.join(rel), &edited).unwrap();

        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(unstaged.hunks.len(), 2);
        stage_hunk(&root, rel, &unstaged, 0).unwrap();

        let staged = get_git_diff(&root, rel, DiffSide::Staged).unwrap();
        assert_eq!(
            changed(&staged),
            vec![pair("delete", "l3"), pair("add", "THREE")]
        );
        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(
            changed(&unstaged),
            vec![pair("delete", "l25"), pair("add", "TWENTY-FIVE")]
        );
        assert_eq!(
            std::fs::read_to_string(root.join(rel)).unwrap(),
            edited,
            "工作区不动"
        );

        // 状态口径也跟上:同一文件既在已暂存也在未暂存
        let status = super::super::get_changes_status(&root).unwrap();
        let entry = status.iter().find(|c| c.path == rel).unwrap();
        assert!(entry.staged_status.is_some() && entry.unstaged_status.is_some());

        unstage_hunk(&root, rel, &staged, 0).unwrap();
        assert!(
            get_git_diff(&root, rel, DiffSide::Staged)
                .unwrap()
                .hunks
                .is_empty()
        );
        assert_eq!(index_blob(&root, rel), base);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 部分暂存的文件丢弃未暂存的那一块:工作区只回退那一块,暂存区那一块不受影响。
    #[test]
    fn 丢弃块后工作区内容正确() {
        let rel = "a.txt";
        let base = join(&numbered(30), "\n");
        let root = init_repo_with_file("hunk-discard", rel, &base);
        let first = base.replacen("l2\n", "TWO\n", 1);
        std::fs::write(root.join(rel), &first).unwrap();
        git_stage(&root, &[rel.to_string()]).unwrap();
        let both = first.replacen("l28\n", "l28\nextra\n", 1);
        std::fs::write(root.join(rel), &both).unwrap();

        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(changed(&unstaged), vec![pair("add", "extra")]);
        discard_hunk(&root, rel, &unstaged, 0).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(rel)).unwrap(), first);
        assert_eq!(index_blob(&root, rel), first, "暂存区不动");
        std::fs::remove_dir_all(&root).ok();
    }

    /// autocrlf=true:暂存区 LF、工作区 CRLF。暂存块写进去的仍是 LF,丢弃块写回的仍是 CRLF。
    #[test]
    fn autocrlf_仓库按块操作保留两侧换行() {
        let rel = "crlf.txt";
        let base = join(&numbered(30), "\n");
        let root = init_repo_with_file("hunk-crlf", rel, &base);
        Repository::open(&root)
            .unwrap()
            .config()
            .unwrap()
            .set_bool("core.autocrlf", true)
            .unwrap();
        let mut lines = numbered(30);
        lines[1] = "B".into();
        lines[26] = "Z".into();
        let worktree = join(&lines, "\r\n");
        std::fs::write(root.join(rel), &worktree).unwrap();

        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(unstaged.hunks.len(), 2);
        stage_hunk(&root, rel, &unstaged, 1).unwrap();
        assert_eq!(index_blob(&root, rel), base.replacen("l27\n", "Z\n", 1));

        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(
            changed(&unstaged),
            vec![pair("delete", "l2"), pair("add", "B")]
        );
        discard_hunk(&root, rel, &unstaged, 0).unwrap();
        let mut expect = numbered(30);
        expect[26] = "Z".into();
        assert_eq!(
            std::fs::read_to_string(root.join(rel)).unwrap(),
            join(&expect, "\r\n")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 取 diff 之后文件又被改了:拒绝,且错误能被认成 [`StaleDiff`];暂存区与工作区都不动。
    #[test]
    fn 陈旧_diff_拒绝执行() {
        let rel = "a.txt";
        let base = join(&numbered(10), "\n");
        let root = init_repo_with_file("hunk-stale", rel, &base);
        let edited = base.replacen("l4\n", "FOUR\n", 1);
        std::fs::write(root.join(rel), &edited).unwrap();
        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();

        let later = edited.replacen("l9\n", "NINE\n", 1);
        std::fs::write(root.join(rel), &later).unwrap();
        let err = stage_hunk(&root, rel, &unstaged, 0).unwrap_err();
        assert!(err.is::<StaleDiff>(), "实际: {err:#}");
        let err = discard_hunk(&root, rel, &unstaged, 0).unwrap_err();
        assert!(err.is::<StaleDiff>(), "实际: {err:#}");
        assert_eq!(index_blob(&root, rel), base);
        assert_eq!(std::fs::read_to_string(root.join(rel)).unwrap(), later);

        // 暂存区变了同样算陈旧
        std::fs::write(root.join(rel), &edited).unwrap();
        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        git_stage(&root, &[rel.to_string()]).unwrap();
        let err = stage_hunk(&root, rel, &unstaged, 0).unwrap_err();
        assert!(err.is::<StaleDiff>(), "实际: {err:#}");
        // 块号越界也是陈旧
        let fresh = get_git_diff(&root, rel, DiffSide::Staged).unwrap();
        assert!(
            unstage_hunk(&root, rel, &fresh, 9)
                .unwrap_err()
                .is::<StaleDiff>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 新增文件取消暂存最后一块:移出暂存区(回到未跟踪),而不是留一个空的已暂存文件。
    #[test]
    fn 新增文件取消暂存最后一块回到未跟踪() {
        let root = init_repo_with_file("hunk-new", "a.txt", "a\n");
        std::fs::write(root.join("new.txt"), "n1\nn2\n").unwrap();
        git_stage(&root, &["new.txt".to_string()]).unwrap();
        let staged = get_git_diff(&root, "new.txt", DiffSide::Staged).unwrap();
        assert_eq!(staged.hunks.len(), 1);
        unstage_hunk(&root, "new.txt", &staged, 0).unwrap();
        let repo = Repository::open(&root).unwrap();
        assert!(
            repo.index()
                .unwrap()
                .get_path(Path::new("new.txt"), 0)
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "n1\nn2\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 不在暂存区(未跟踪)的文件只能整文件操作。
    #[test]
    fn 未跟踪文件拒绝按块操作() {
        let root = init_repo_with_file("hunk-untracked", "a.txt", "a\n");
        std::fs::write(root.join("u.txt"), "x\n").unwrap();
        let diff = get_git_diff(&root, "u.txt", DiffSide::Unstaged).unwrap();
        let err = stage_hunk(&root, "u.txt", &diff, 0).unwrap_err();
        assert!(!err.is::<StaleDiff>());
        assert!(format!("{err:#}").contains("整文件"), "实际: {err:#}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 仓库在项目根之上(monorepo 子目录):与 get_git_diff 同一口径能找到。
    #[test]
    fn 项目是仓库子目录时同样可用() {
        let rel_in_repo = "packages/app/x.txt";
        let base = join(&numbered(10), "\n");
        let root = init_repo_with_file("hunk-sub", rel_in_repo, &base);
        let edited = base.replacen("l6\n", "SIX\n", 1);
        std::fs::write(root.join(rel_in_repo), &edited).unwrap();
        let project = root.join("packages").join("app");
        let diff = get_git_diff(&project, "x.txt", DiffSide::Unstaged).unwrap();
        stage_hunk(&project, "x.txt", &diff, 0).unwrap();
        assert_eq!(index_blob(&root, rel_in_repo), edited);
        std::fs::remove_dir_all(&root).ok();
    }
}
