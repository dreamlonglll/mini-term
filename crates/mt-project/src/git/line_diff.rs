//! 编辑器 gutter 变更标记的后端:HEAD 版本取数 + 「HEAD ↔ 编辑器缓冲区」的行级 diff。
//!
//! 对标 IDEA 的 line status tracker:行号旁的竖条只关心「哪几行是新增 / 修改、哪里删过
//! 行」,不要上下文、不要逐行配对,所以这里不复用 [`super::get_git_diff`] 的 hunk(那是给
//! diff 弹窗看的,带 3 行上下文),而是把编辑序列里**连续的非相等段**直接收成一块:
//!
//! | 段里有 | 块的类型 | 标在哪 |
//! |---|---|---|
//! | 只有新增行 | [`LineChangeKind::Added`] | 新侧那几行 |
//! | 只有删除行 | [`LineChangeKind::Deleted`] | 新侧的删除点(两行之间) |
//! | 两者都有 | [`LineChangeKind::Modified`] | 新侧那几行(与 IDEA 一样整段算修改) |
//!
//! # diff 算法:先 Myers,兜底才是父模块的 LCS
//!
//! 编辑器里每停顿一次就要重算(300ms 去抖),而父模块的 [`super::edit_script`] 是
//! O(m·n) 的 DP 表:剥掉公共前后缀之后中段一大(比如 4000 行的文件首尾各改一行),
//! 就会越过 [`super::MAX_LCS_CELLS`] 退化成整块替换 —— 整份文件标成「修改」,等于没标。
//! 这里先跑 O((N+M)·D) 的 Myers(D = 编辑距离,日常编辑只有几到几十),D 超过
//! [`MYERS_MAX_D`] 才回落到父模块那条 LCS,LCS 也越线时再按原口径退化成一整块。
//! 三条路产出的都是同一种编辑序列(`('=', 旧, 新)` / `('-', 旧, 新位置)` /
//! `('+', 旧位置, 新)`),分块逻辑只有一份。
//!
//! # 行的口径
//!
//! 两侧都按 `str::lines()` 切行(与 [`super::diff_two_texts`] 一致):末尾换行有无
//! 不算差异,`\r` 被剥掉。调用方(编辑器)负责把 HEAD 内容与缓冲区归一成同一种
//! 形态(行尾、Tab 展开),这里只比字符串。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use git2::{ObjectType, Repository};
use pathdiff::diff_paths;

use super::{
    MAX_DIFF_BYTES, MAX_LCS_CELLS, common_affix, discover_repo_for, edit_script, lcs_workload,
};

/// Myers 允许的最大编辑距离。超过它说明改动面已经大到「整段重写」的量级,
/// 交给 LCS / 退化兜底。回溯要留每一步的 V 数组(≈ D² 个 `i32`),1000 步约 4 MB。
pub const MYERS_MAX_D: usize = 1000;

/// gutter 上一块变更的类型。颜色口径与 Git 状态色一致:新增绿 / 修改橙 / 删除红。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineChangeKind {
    Added,
    Modified,
    Deleted,
}

/// gutter 上的一块变更。行号全是 **0-based**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHunk {
    pub kind: LineChangeKind,
    /// 新侧(编辑器缓冲区)起始行。删除块是**删除点**:被删的内容原本位于这一行之前
    /// (等于新侧行数时就是删在了文件末尾)
    pub new_start: usize,
    /// 新侧行数;删除块为 0
    pub new_len: usize,
    /// 旧侧(HEAD)起始行。新增块是对应的插入位置
    pub old_start: usize,
    /// 旧侧行数;新增块为 0
    pub old_len: usize,
}

impl LineHunk {
    /// 新侧覆盖的行范围(删除块是空范围)。
    pub fn new_range(&self) -> std::ops::Range<usize> {
        self.new_start..self.new_start + self.new_len
    }

    /// 旧侧覆盖的行范围(新增块是空范围)。
    pub fn old_range(&self) -> std::ops::Range<usize> {
        self.old_start..self.old_start + self.old_len
    }
}

/// HEAD 版本 → 缓冲区的 gutter 变更块,按新侧行号升序。
pub fn line_hunks(old: &str, new: &str) -> Vec<LineHunk> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    group_hunks(&line_ops(&old_lines, &new_lines, MYERS_MAX_D))
}

/// 新侧每一行对应旧侧的哪一行(`None` = 这一行是新增 / 改过的)。
///
/// 注解(blame)在缓冲区被编辑之后靠它把「blame 时的那份内容」的结果搬到当前行上:
/// 对得上的行沿用原来的提交,对不上的行一律算未提交。
pub fn line_alignment(old: &str, new: &str) -> Vec<Option<usize>> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut map = vec![None; new_lines.len()];
    for (kind, oi, ni) in line_ops(&old_lines, &new_lines, MYERS_MAX_D) {
        if kind == '=' {
            map[ni] = Some(oi);
        }
    }
    map
}

/// 编辑序列:Myers → LCS → 整块退化,三选一(见模块注释)。
fn line_ops(old: &[&str], new: &[&str], max_d: usize) -> Vec<(char, usize, usize)> {
    let (head, tail) = common_affix(old, new);
    let a = &old[head..old.len() - tail];
    let b = &new[head..new.len() - tail];

    let middle = myers_ops(a, b, max_d).or_else(|| {
        // LCS 自己也会剥前后缀;只把中段交给它,下标再整体平移回来
        (lcs_workload(a, b) <= MAX_LCS_CELLS).then(|| edit_script(a, b))
    });

    let mut ops: Vec<(char, usize, usize)> = (0..head).map(|i| ('=', i, i)).collect();
    match middle {
        Some(middle) => {
            ops.extend(
                middle
                    .into_iter()
                    .map(|(kind, oi, ni)| (kind, oi + head, ni + head)),
            );
        }
        // 两条路都放弃了:中段整块算「删掉旧的、换上新的」
        None => {
            ops.extend((0..a.len()).map(|i| ('-', head + i, head)));
            ops.extend((0..b.len()).map(|j| ('+', head + a.len(), head + j)));
        }
    }
    ops.extend((0..tail).map(|k| ('=', old.len() - tail + k, new.len() - tail + k)));
    ops
}

/// Myers 最短编辑脚本(下标相对传入的切片)。编辑距离超过 `max_d` 返回 `None`。
///
/// 行先换成整数 id 再比,内层循环只比 `u32`。回溯用的是「进入第 d 步之前的 V」,
/// 每步只存 `k ∈ [-(d+1), d+1]` 那一段,内存 ≈ D²。
fn myers_ops<'s>(a: &[&'s str], b: &[&'s str], max_d: usize) -> Option<Vec<(char, usize, usize)>> {
    let mut ids: HashMap<&'s str, u32> = HashMap::new();
    let mut intern = |s: &'s str| -> u32 {
        let next = ids.len() as u32;
        *ids.entry(s).or_insert(next)
    };
    let a_ids: Vec<u32> = a.iter().map(|s| intern(s)).collect();
    let b_ids: Vec<u32> = b.iter().map(|s| intern(s)).collect();

    let n = a_ids.len() as i64;
    let m = b_ids.len() as i64;
    let max_d = (max_d as i64).min(n + m);
    // V 按 k + offset 下标;k 的取值范围是 [-(max_d+1), max_d+1]
    let offset = max_d + 1;
    let mut v = vec![0i64; (2 * offset + 1) as usize];
    let mut trace: Vec<Vec<i64>> = Vec::new();
    let mut found = None;

    'outer: for d in 0..=max_d {
        // 进入第 d 步之前的 V,只留回溯会读到的那一段 k ∈ [-(d+1), d+1]
        // (d ≤ max_d,所以 offset - d - 1 ≥ 0、offset + d + 1 ≤ 2·offset,不会越界)
        let lo = (offset - d - 1) as usize;
        let hi = (offset + d + 1) as usize;
        trace.push(v[lo..=hi].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a_ids[x as usize] == b_ids[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_end = found?;

    // 回溯:从 (n, m) 一步步退回 (0, 0),逆序吐出编辑,最后翻转
    let mut ops = Vec::with_capacity((n + m) as usize);
    let (mut x, mut y) = (n, m);
    for d in (0..=d_end).rev() {
        // 快照里第 0 格是 k = -(d+1)
        let snap = &trace[d as usize];
        let at = |k: i64| snap[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(('=', (x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                // 竖着走一步:新侧多一行
                ops.push(('+', x as usize, (y - 1) as usize));
            } else {
                // 横着走一步:旧侧删一行
                ops.push(('-', (x - 1) as usize, y as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

/// 编辑序列 → gutter 变更块:连续的非 `=` 段收成一块(见模块注释的表)。
fn group_hunks(ops: &[(char, usize, usize)]) -> Vec<LineHunk> {
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].0 == '=' {
            i += 1;
            continue;
        }
        // 段首那一条的两个下标就是两侧的起点:`-` 记的是新侧当前位置,`+` 记的是
        // 旧侧当前位置,与父模块 `edit_script` 同一口径
        let (_, old_start, new_start) = ops[i];
        let (mut dels, mut adds) = (0, 0);
        while i < ops.len() && ops[i].0 != '=' {
            if ops[i].0 == '-' {
                dels += 1;
            } else {
                adds += 1;
            }
            i += 1;
        }
        let kind = match (dels, adds) {
            (0, _) => LineChangeKind::Added,
            (_, 0) => LineChangeKind::Deleted,
            _ => LineChangeKind::Modified,
        };
        hunks.push(LineHunk {
            kind,
            new_start,
            new_len: adds,
            old_start,
            old_len: dels,
        });
    }
    hunks
}

// ---------------------------------------------------------------------------
// HEAD 版本
// ---------------------------------------------------------------------------

/// HEAD 在这个文件上的「指纹」,轮询它来判断要不要重取 HEAD 版本 / 重跑注解。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadSignature {
    /// HEAD 指向的提交;仓库还没有任何提交时为 `None`
    pub head: Option<String>,
    /// HEAD 里这个文件的 blob;文件不在 HEAD 里时为 `None`
    pub blob: Option<String>,
}

/// 文件在仓库里的定位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoFile {
    /// 仓库工作区根(git CLI 在这里跑)
    pub root: PathBuf,
    /// 相对工作区根、正斜杠分隔
    pub rel_path: String,
    pub signature: HeadSignature,
}

/// 一个文件在 HEAD 里的样子(gutter 取数的结论)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadFile {
    /// 不在任何仓库里 / 裸仓库 / 在工作区之外:什么都不画
    NotInRepo,
    /// 在仓库里但 HEAD 里没有这个文件(未跟踪、刚加进暂存区、仓库还没有提交)
    NotInHead(RepoFile),
    /// HEAD 里的版本是二进制 / 非 UTF-8
    Binary(RepoFile),
    /// HEAD 里的版本超过 diff 上限
    TooLarge(RepoFile),
    /// HEAD 里的原文(行尾、Tab 都原样,归一由调用方做)
    Text { repo: RepoFile, text: String },
}

impl HeadFile {
    /// 文件所在仓库;不在仓库里时为 `None`。
    pub fn repo(&self) -> Option<&RepoFile> {
        match self {
            HeadFile::NotInRepo => None,
            HeadFile::NotInHead(repo)
            | HeadFile::Binary(repo)
            | HeadFile::TooLarge(repo)
            | HeadFile::Text { repo, .. } => Some(repo),
        }
    }
}

/// 取 `project_path` 下文件 `abs_file` 的 HEAD 版本。仓库定位与
/// [`super::get_git_diff`] 同一套(离文件最近的仓库,上界锚在项目根往上几级)。
///
/// 阻塞(git2 读对象库),调用方丢后台执行器。
pub fn head_file(project_path: &Path, abs_file: &Path) -> Result<HeadFile> {
    let Some(repo) = discover_repo_for(project_path, abs_file) else {
        return Ok(HeadFile::NotInRepo);
    };
    let Some(workdir) = repo.workdir() else {
        return Ok(HeadFile::NotInRepo);
    };
    let Some(rel) = diff_paths(abs_file, workdir) else {
        return Ok(HeadFile::NotInRepo);
    };
    // 工作区之外的文件 diff_paths 给的是 `../x`,不归这个仓库管
    if rel
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Ok(HeadFile::NotInRepo);
    }
    let rel_path = rel.to_string_lossy().replace('\\', "/");
    let signature = signature_of(&repo, &rel_path)?;
    let blob_id = signature.blob.clone();
    let repo_file = RepoFile {
        root: workdir.to_path_buf(),
        rel_path,
        signature,
    };
    let Some(blob_id) = blob_id else {
        return Ok(HeadFile::NotInHead(repo_file));
    };
    let blob = repo.find_blob(git2::Oid::from_str(&blob_id)?)?;
    if blob.is_binary() {
        return Ok(HeadFile::Binary(repo_file));
    }
    if blob.content().len() > MAX_DIFF_BYTES {
        return Ok(HeadFile::TooLarge(repo_file));
    }
    Ok(match std::str::from_utf8(blob.content()) {
        Ok(text) => HeadFile::Text {
            repo: repo_file,
            text: text.to_string(),
        },
        Err(_) => HeadFile::Binary(repo_file),
    })
}

/// 只取指纹,不读 blob 内容。轮询用:几毫秒的事。
pub fn head_signature(repo_root: &Path, rel_path: &str) -> Result<HeadSignature> {
    let repo = Repository::open(repo_root)?;
    signature_of(&repo, rel_path)
}

fn signature_of(repo: &Repository, rel_path: &str) -> Result<HeadSignature> {
    // 还没有任何提交(unborn HEAD):两项都空
    let Ok(head) = repo.head() else {
        return Ok(HeadSignature::default());
    };
    let commit = head.peel_to_commit()?;
    let blob = commit
        .tree()?
        .get_path(Path::new(rel_path))
        .ok()
        .filter(|entry| entry.kind() == Some(ObjectType::Blob))
        .map(|entry| entry.id().to_string());
    Ok(HeadSignature {
        head: Some(commit.id().to_string()),
        blob,
    })
}

#[cfg(test)]
#[path = "line_diff_tests.rs"]
mod tests;
