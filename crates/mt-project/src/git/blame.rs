//! 编辑器「注解」(Annotate / `git blame`)的后端。
//!
//! 走 git CLI 而不是 libgit2:实测本仓 1185 个提交时 `git blame` 对 4675 行的文件
//! 145ms、`main.rs` 204ms,libgit2 的 blame 通常慢数倍,且不认 `--contents`。
//!
//! # 行号对齐:`--contents -`
//!
//! 一律把**编辑器当前缓冲区**(调用方已还原成落盘形态:Tab、行尾都按原文件)从 stdin
//! 喂给 `git blame --contents -`:结果的行号就是缓冲区的行号,脏态下也不错位;还没
//! 提交 / 还没保存的行由 git 自己标成全零哈希(「Not Committed Yet」),见
//! [`BlameCommit::is_uncommitted`]。git 会对喂进去的内容做与工作区文件相同的
//! `convert_to_git`(autocrlf / eol 属性),CRLF 文件照样对得上。
//!
//! blame 之后缓冲区再被编辑,行的搬运在调用方用 [`super::line_alignment`] 做,不必
//! 每敲一个字都重跑一次 git。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow, bail};

use super::{QUICK_OP_TIMEOUT, run_git_command_with_input};

/// 超过这么多行就不注解(列表画得动,但 git 跑得久、porcelain 输出也大)。
pub const BLAME_MAX_LINES: usize = 20_000;

/// blame 里出现的一个提交(porcelain 每个提交只给一次详情)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlameCommit {
    /// 完整哈希(SHA-1 40 位 / SHA-256 64 位)
    pub hash: String,
    pub author: String,
    /// 去掉了两边的尖括号
    pub author_mail: String,
    /// Unix 秒
    pub author_time: i64,
    /// 形如 `+0800`
    pub author_tz: String,
    /// 提交标题(首行)
    pub summary: String,
}

impl BlameCommit {
    /// 全零哈希 = 工作区里还没提交的行(`--contents` 喂进去的改动)。
    pub fn is_uncommitted(&self) -> bool {
        !self.hash.is_empty() && self.hash.bytes().all(|b| b == b'0')
    }

    /// 7 位短哈希。
    pub fn short_hash(&self) -> &str {
        &self.hash[..self.hash.len().min(7)]
    }
}

/// 一次 blame 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlameResult {
    pub commits: Vec<BlameCommit>,
    /// 第 i 行(0-based)属于 `commits[lines[i]]`
    pub lines: Vec<u32>,
}

impl BlameResult {
    /// 第 `line` 行(0-based)的提交;越界为 `None`。
    pub fn commit_of(&self, line: usize) -> Option<&BlameCommit> {
        self.lines
            .get(line)
            .and_then(|&idx| self.commits.get(idx as usize))
    }
}

/// 对仓库 `repo_root` 里的 `rel_path` 跑 blame,内容取 `contents`(落盘形态)。
///
/// 文件不在 HEAD 里(未跟踪 / 新加入暂存区)时 git 报 `no such path`,原样作为错误
/// 返回 —— 调用方本该在 HEAD 取数那一步就把这种文件挡掉。
///
/// **阻塞最多 30s**,必须在后台线程上调用。
pub fn blame_file(repo_root: &Path, rel_path: &str, contents: &str) -> Result<BlameResult> {
    let out = run_git_command_with_input(
        repo_root,
        &["blame", "--porcelain", "--contents", "-", "--", rel_path],
        "blame",
        contents.as_bytes().to_vec(),
        // 历史很长的大文件也就几秒,30s 足够分辨「卡死」
        QUICK_OP_TIMEOUT,
        ",文件历史可能过长",
    )?;
    parse_blame_porcelain(&out)
}

/// 解析 `git blame --porcelain` 的输出。
///
/// 格式(`git help blame` 的 THE PORCELAIN FORMAT 一节):每一组行先给一行
/// `<哈希> <原行号> <现行号> [<组内行数>]`,该提交**第一次出现**时紧跟若干
/// `key value` 详情行,然后是以 Tab 开头的行内容;同一组后续的行只有头行 + 内容。
pub fn parse_blame_porcelain(output: &str) -> Result<BlameResult> {
    let mut commits: Vec<BlameCommit> = Vec::new();
    let mut by_hash: HashMap<String, u32> = HashMap::new();
    let mut lines: Vec<u32> = Vec::new();
    // 当前头行:(提交下标, 现行号 0-based);内容行到了就消费掉
    let mut current: Option<(u32, usize)> = None;
    // 详情行归属的提交(头行之后、内容行之前)
    let mut detail_of: Option<u32> = None;

    for raw in output.split('\n') {
        if raw.starts_with('\t') {
            let (idx, line) = current
                .take()
                .ok_or_else(|| anyhow!("blame 输出格式不对:内容行之前缺头行"))?;
            if lines.len() <= line {
                lines.resize(line + 1, u32::MAX);
            }
            lines[line] = idx;
            detail_of = None;
            continue;
        }
        let raw = raw.trim_end_matches('\r');
        if raw.is_empty() {
            continue;
        }
        if let Some((hash, final_line)) = parse_header(raw) {
            let idx = match by_hash.get(hash) {
                Some(&idx) => idx,
                None => {
                    let idx = commits.len() as u32;
                    commits.push(BlameCommit {
                        hash: hash.to_string(),
                        ..BlameCommit::default()
                    });
                    by_hash.insert(hash.to_string(), idx);
                    idx
                }
            };
            current = Some((idx, final_line));
            detail_of = Some(idx);
            continue;
        }
        let Some(idx) = detail_of else {
            continue;
        };
        let commit = &mut commits[idx as usize];
        let (key, value) = raw.split_once(' ').unwrap_or((raw, ""));
        match key {
            "author" => commit.author = value.to_string(),
            "author-mail" => {
                commit.author_mail = value
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string();
            }
            "author-time" => commit.author_time = value.trim().parse().unwrap_or(0),
            "author-tz" => commit.author_tz = value.to_string(),
            "summary" => commit.summary = value.to_string(),
            _ => {}
        }
    }

    if lines.contains(&u32::MAX) {
        bail!("blame 输出格式不对:有行没有归属的提交");
    }
    Ok(BlameResult { commits, lines })
}

/// 头行 `<哈希> <原行号> <现行号> [<组内行数>]` → `(哈希, 现行号 0-based)`。
/// 不是头行返回 `None`(详情行的首个词都不是 40/64 位十六进制)。
fn parse_header(line: &str) -> Option<(&str, usize)> {
    let mut parts = line.split(' ');
    let hash = parts.next()?;
    if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let _orig: usize = parts.next()?.parse().ok()?;
    let final_line: usize = parts.next()?.parse().ok()?;
    if let Some(group) = parts.next() {
        group.parse::<usize>().ok()?;
    }
    if parts.next().is_some() || final_line == 0 {
        return None;
    }
    Some((hash, final_line - 1))
}

#[cfg(test)]
#[path = "blame_tests.rs"]
mod tests;
