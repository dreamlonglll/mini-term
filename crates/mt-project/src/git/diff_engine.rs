use super::*;

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// 内置 diff 视图的上限,与 fs 侧的文件查看上限同为 1MB。
pub(super) const MAX_DIFF_BYTES: usize = 1_048_576;

/// `oid` 是个超过 [`MAX_DIFF_BYTES`] 的 blob。只读对象头(打包对象不解压正文),
/// 不把内容读进内存;不是 blob 的(目录、子模块的提交)交给调用方按原口径处理。
pub(super) fn blob_too_large(repo: &Repository, oid: Oid) -> Result<bool> {
    let (size, kind) = repo.odb()?.read_header(oid)?;
    Ok(kind == git2::ObjectType::Blob && size > MAX_DIFF_BYTES)
}

/// LCS 的 O(m·n) DP 表在超大文件上会吃爆内存,越过这条线就不跑 LCS(先跑的 Myers 也
/// 放弃了的话,退化成「整块替换」)。
///
/// ⚠️ 这条线量的是**剥掉公共前后缀之后**的中段(见 [`lcs_workload`])。按整文件
/// 量的话,一个 4000 行的文件只改一行也会越线 —— 退化出来的「整块替换」把全文
/// 标成一删一增,等于没 diff。
pub(super) const MAX_LCS_CELLS: u64 = 10_000_000;

pub(super) fn binary_diff_result() -> GitDiffResult {
    GitDiffResult {
        old_content: String::new(),
        new_content: String::new(),
        hunks: Vec::new(),
        is_binary: true,
        too_large: false,
        encoding: None,
    }
}

pub(super) fn too_large_diff_result() -> GitDiffResult {
    GitDiffResult {
        old_content: String::new(),
        new_content: String::new(),
        hunks: Vec::new(),
        is_binary: false,
        too_large: true,
        encoding: None,
    }
}

/// 两段文本 → 完整 diff 结果(改动面太大时退化为整块替换,见 [`build_hunks`])。
///
/// 公开给不经 git 的两段文本比较用(文件页「磁盘版本 ↔ 未保存的修改」的对比);
/// 结果的 `encoding` 恒为 `None`,由调用方按需补。
pub fn diff_two_texts(old_content: String, new_content: String) -> GitDiffResult {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let hunks = build_hunks(&old_lines, &new_lines);

    GitDiffResult {
        old_content,
        new_content,
        hunks,
        is_binary: false,
        too_large: false,
        encoding: None,
    }
}

/// HEAD 里这个文件的内容。`None` = 空仓库;HEAD 里没有这个文件是空文本。
/// 超过 [`MAX_DIFF_BYTES`] 的不读内容(先看对象头),报 [`SideText::TooLarge`]。
pub(super) fn head_text(repo: &Repository, rel_path: &str) -> Result<Option<SideText>> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None), // 空仓库
    };
    let tree = head.peel_to_tree()?;
    let entry = match tree.get_path(Path::new(rel_path)) {
        Ok(e) => e,
        Err(_) => return Ok(Some(SideText::Text(String::new(), None))), // 文件还没进 HEAD
    };
    if blob_too_large(repo, entry.id())? {
        return Ok(Some(SideText::TooLarge));
    }
    let obj = entry.to_object(repo)?;
    let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;
    Ok(Some(match decode_side(blob.content(), blob.is_binary()) {
        Some((text, encoding)) => SideText::Text(text, encoding),
        None => SideText::Binary,
    }))
}

/// HEAD 版本的文本(按块操作用:要拿它拼回暂存区)。二进制 / 过大 / 非 UTF-8 都是
/// 错误 —— 块操作按 UTF-8 文本拼接再写回字节,非 UTF-8 的拼回去就写坏了。
pub(super) fn get_head_content(repo: &Repository, rel_path: &str) -> Result<Option<String>> {
    match head_text(repo, rel_path)? {
        None => Ok(None),
        Some(SideText::Text(s, None)) => Ok(Some(s)),
        Some(SideText::Text(_, Some(_))) => bail!("not utf-8"),
        Some(SideText::Binary) => bail!("binary"),
        Some(SideText::TooLarge) => bail!("too large"),
    }
}

/// 两段行序列的公共前缀 / 后缀行数(前缀优先,后缀在剩下的范围里数)。
pub(super) fn common_affix(old_lines: &[&str], new_lines: &[&str]) -> (usize, usize) {
    let max = old_lines.len().min(new_lines.len());
    let mut head = 0;
    while head < max && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < max - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }
    (head, tail)
}

/// 剥掉公共前后缀之后,LCS 还要开多大的 DP 表(格子数)。
///
/// 「大文件」与「大改动」不是一回事:1 万行的文件改一行,中段只剩几行,
/// DP 表小到可以忽略;整份重写才是真的要退化。
pub(super) fn lcs_workload(old_lines: &[&str], new_lines: &[&str]) -> u64 {
    let (head, tail) = common_affix(old_lines, new_lines);
    (old_lines.len() - head - tail) as u64 * (new_lines.len() - head - tail) as u64
}

/// 编辑序列: `('=', old_i, new_j)` | `('-', old_i, _)` | `('+', _, new_j)`。
///
/// 两头对得上的行直接抄成 `=`,只有中段跑 LCS —— DP 表按 [`lcs_workload`] 算。
pub(super) fn edit_script(old_lines: &[&str], new_lines: &[&str]) -> Vec<(char, usize, usize)> {
    let (head, tail) = common_affix(old_lines, new_lines);
    let mut flat: Vec<(char, usize, usize)> = (0..head).map(|i| ('=', i, i)).collect();

    let a = &old_lines[head..old_lines.len() - tail];
    let b = &new_lines[head..new_lines.len() - tail];
    // DP 表里的值不超过 min(m, n)。格子数被 MAX_LCS_CELLS 封顶时 min(m, n) ≤ √1000 万 ≈ 3162,
    // u16 装得下 —— 1000 万格的表从原先的 usize(80 MB,外加每行一次分配)降到 20 MB
    if a.len().min(b.len()) < usize::from(u16::MAX) {
        lcs_backtrack::<u16>(a, b, head, &mut flat);
    } else {
        lcs_backtrack::<u32>(a, b, head, &mut flat);
    }

    for k in 0..tail {
        flat.push(('=', old_lines.len() - tail + k, new_lines.len() - tail + k));
    }
    flat
}

/// LCS 表的格子类型(按 min(m, n) 选够用的最窄的那种)。
pub(super) trait LcsCell: Copy + Ord {
    const ZERO: Self;
    fn succ(self) -> Self;
}

impl LcsCell for u16 {
    const ZERO: Self = 0;
    fn succ(self) -> Self {
        self + 1
    }
}

impl LcsCell for u32 {
    const ZERO: Self = 0;
    fn succ(self) -> Self {
        self + 1
    }
}

/// 中段 `a` / `b` 的 LCS:填表(一整块 `(m+1)·(n+1)` 的平铺数组)再回溯,编辑追加进 `flat`
/// (下标加回 `head`)。
pub(super) fn lcs_backtrack<T: LcsCell>(
    a: &[&str],
    b: &[&str],
    head: usize,
    flat: &mut Vec<(char, usize, usize)>,
) {
    let (m, n) = (a.len(), b.len());
    let width = n + 1;
    let mut dp = vec![T::ZERO; (m + 1) * width];
    let at = |i: usize, j: usize| i * width + j;
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[at(i, j)] = if a[i] == b[j] {
                dp[at(i + 1, j + 1)].succ()
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }

    // 回溯。下标要加回 head —— 外面拿它当行号用
    //
    // ⚠️ 平局时**先走删除**(`>` 而不是 `>=`)。换行改动的 LCS 是平的,
    // 取 `>=` 会先吐 `+` 再吐 `-` —— 一行改写在行内视图里显示成「先增后删」,与 git
    // 的习惯相反。(并排视图不受先后影响:`mt-app::git_diff::split` 把连续的增删行整段
    // 当成一处改动,删除行进左栏、新增行进右栏。)
    let mut i = 0;
    let mut j = 0;
    while i < m || j < n {
        if i < m && j < n && a[i] == b[j] {
            flat.push(('=', head + i, head + j));
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[at(i, j + 1)] > dp[at(i + 1, j)]) {
            flat.push(('+', head + i, head + j));
            j += 1;
        } else {
            flat.push(('-', head + i, head + j));
            i += 1;
        }
    }
}

/// 行 diff,上下文 3 行。编辑序列走 [`line_diff::diff_ops`](中段小走 LCS,大了先 Myers
/// 再 LCS),改动块已按 git 的口径滑到位(见 `slide` 模块注释),行号才与 `git diff`
/// 对得上。两条路都放弃(编辑距离超过 Myers 上限、中段又越过 [`MAX_LCS_CELLS`])时
/// 退化成整块替换。
pub(super) fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let Some(flat) = line_diff::diff_ops(old_lines, new_lines) else {
        return full_replace_hunks(old_lines, new_lines);
    };

    // 按上下文 3 行分组成 hunk
    const CONTEXT: usize = 3;
    let mut hunks: Vec<DiffHunk> = Vec::new();

    let changed_indices: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, (k, _, _))| *k != '=')
        .map(|(idx, _)| idx)
        .collect();

    if changed_indices.is_empty() {
        return hunks;
    }

    // 把变更下标按「带上下文后是否相接」合并成区间
    let mut groups: Vec<(usize, usize)> = Vec::new(); // (start, end) in flat[]
    let start = changed_indices[0].saturating_sub(CONTEXT);
    let end = (changed_indices[0] + CONTEXT + 1).min(flat.len());
    groups.push((start, end));

    for &idx in &changed_indices[1..] {
        let last = groups.last_mut().unwrap();
        let expanded_start = idx.saturating_sub(CONTEXT);
        let expanded_end = (idx + CONTEXT + 1).min(flat.len());
        if expanded_start <= last.1 {
            last.1 = last.1.max(expanded_end);
        } else {
            groups.push((expanded_start, expanded_end));
        }
    }

    for (grp_start, grp_end) in groups {
        let slice = &flat[grp_start..grp_end];
        let mut lines_out: Vec<DiffLine> = Vec::new();
        let mut old_start = 0u32;
        let mut new_start = 0u32;
        let mut old_count = 0u32;
        let mut new_count = 0u32;
        let mut first = true;

        for (k, oi, ni) in slice {
            let old_lineno = (*oi as u32) + 1;
            let new_lineno = (*ni as u32) + 1;
            match k {
                '=' => {
                    if first {
                        old_start = old_lineno;
                        new_start = new_lineno;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "context".to_string(),
                        content: old_lines[*oi].to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: Some(new_lineno),
                    });
                    old_count += 1;
                    new_count += 1;
                }
                '-' => {
                    if first {
                        old_start = old_lineno;
                        // new_start 可能是紧随其后的插入位置,这里取近似值
                        new_start = (*ni as u32) + 1;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "delete".to_string(),
                        content: old_lines[*oi].to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: None,
                    });
                    old_count += 1;
                }
                '+' => {
                    if first {
                        old_start = (*oi as u32) + 1;
                        new_start = new_lineno;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "add".to_string(),
                        content: new_lines[*ni].to_string(),
                        old_lineno: None,
                        new_lineno: Some(new_lineno),
                    });
                    new_count += 1;
                }
                _ => {}
            }
        }

        hunks.push(DiffHunk {
            old_start,
            old_lines: old_count,
            new_start,
            new_lines: new_count,
            lines: lines_out,
        });
    }

    hunks
}

#[cfg(test)]
pub(super) fn full_replace_diff(old_content: &str, new_content: &str) -> Vec<DiffHunk> {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    full_replace_hunks(&old_lines, &new_lines)
}

pub(super) fn full_replace_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let mut lines_out: Vec<DiffLine> = Vec::new();

    for (i, l) in old_lines.iter().enumerate() {
        lines_out.push(DiffLine {
            kind: "delete".to_string(),
            content: l.to_string(),
            old_lineno: Some((i as u32) + 1),
            new_lineno: None,
        });
    }
    for (i, l) in new_lines.iter().enumerate() {
        lines_out.push(DiffLine {
            kind: "add".to_string(),
            content: l.to_string(),
            old_lineno: None,
            new_lineno: Some((i as u32) + 1),
        });
    }

    if lines_out.is_empty() {
        return Vec::new();
    }

    vec![DiffHunk {
        old_start: 1,
        old_lines: old_lines.len() as u32,
        new_start: 1,
        new_lines: new_lines.len() as u32,
        lines: lines_out,
    }]
}

/// 单文件 diff 比的是哪两侧(与 `git diff` 家族同口径)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffSide {
    /// HEAD → 暂存区(`git diff --cached`,「已暂存」分组)
    Staged,
    /// 暂存区 → 工作区(`git diff`,「未暂存」/「未跟踪」分组)。部分暂存的文件
    /// 只显示**还没暂存**的那部分 —— 按块暂存 / 丢弃都以这一侧为准
    Unstaged,
    /// HEAD → 工作区(`git diff HEAD`,文件树「查看变更」:相对上次提交的全部改动)
    Working,
}

/// 单文件 diff,两侧由 [`DiffSide`] 决定。`file_path` 是相对 `project_path` 的路径。
pub fn get_git_diff(project_path: &Path, file_path: &str, side: DiffSide) -> Result<GitDiffResult> {
    get_git_diff_from(project_path, file_path, None, side)
}

/// 同 [`get_git_diff`],外加重命名前的路径 `old_path`(同样相对 `project_path`)。
///
/// 已暂存的重命名(`git mv a b`)在 HEAD 里只有旧路径 `a`:拿新路径 `b` 去 HEAD 里查
/// 查不到,整份内容就成了「新增」。给了 `old_path` 时,取 HEAD 那一侧(`Staged` /
/// `Working` 的旧侧)改按旧路径取 —— 与 `git diff --cached -M` 一样只显示真正改了的行。
/// 暂存区 / 工作区那一侧照旧按新路径取;`None` 与 [`get_git_diff`] 完全相同。
pub fn get_git_diff_from(
    project_path: &Path,
    file_path: &str,
    old_path: Option<&str>,
    side: DiffSide,
) -> Result<GitDiffResult> {
    let abs_file = project_path.join(file_path);

    let repo = discover_repo_for(project_path, &abs_file).ok_or_else(|| {
        anyhow!(
            "no git repository found for {} (searched up to {} parents above {})",
            abs_file.display(),
            MAX_DISCOVER_PARENTS,
            project_path.display()
        )
    })?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repository not supported"))?;

    let rel_str = repo_relative_path(workdir, project_path, &abs_file)
        .ok_or_else(|| anyhow!("file is outside repository working directory"))?;
    // HEAD 里按哪个路径取:重命名取旧路径(算不出来就退回新路径,至少不报错)
    let head_rel = old_path
        .and_then(|old| repo_relative_path(workdir, project_path, &project_path.join(old)))
        .unwrap_or_else(|| rel_str.clone());

    // 新侧:暂存区取 index,否则取工作区(已删除的文件 = 空)
    let (new_content, new_encoding) = match side {
        DiffSide::Staged => match index_text(&repo, &rel_str)? {
            Some(SideText::Text(s, encoding)) => (s, encoding),
            Some(SideText::Binary) => return Ok(binary_diff_result()),
            Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
            None => (String::new(), None),
        },
        DiffSide::Unstaged | DiffSide::Working => match std::fs::metadata(&abs_file) {
            // 不存在(工作区里已删除)= 空
            Err(_) => (String::new(), None),
            // 先看大小再读:超限的文件不整份读进内存
            Ok(meta) if meta.len() > MAX_DIFF_BYTES as u64 => {
                return Ok(too_large_diff_result());
            }
            Ok(_) => {
                let new_bytes = std::fs::read(&abs_file)?;
                // 量完大小到读完之间文件又长大了:照样按上限算
                if new_bytes.len() > MAX_DIFF_BYTES {
                    return Ok(too_large_diff_result());
                }
                // UTF-8 照旧原样收下(不复制);不是的才交给编码识别
                match String::from_utf8(new_bytes) {
                    Ok(s) => (s, None),
                    Err(err) => match decode_side(err.as_bytes(), false) {
                        Some(side) => side,
                        None => return Ok(binary_diff_result()),
                    },
                }
            }
        },
    };

    // 旧侧:未暂存比暂存区,其余比 HEAD。两侧同一条大小上限(旧侧超限同样报「过大」)
    let (old_content, old_encoding) = match side {
        DiffSide::Unstaged => match index_text(&repo, &rel_str)? {
            Some(SideText::Text(s, encoding)) => (s, encoding),
            Some(SideText::Binary) => return Ok(binary_diff_result()),
            Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
            // 冲突中的文件没有 stage 0 条目 —— 退回 HEAD(ours),不能当成新增
            None if is_conflicted(&repo, &rel_str)? => match head_text(&repo, &rel_str)? {
                Some(SideText::Text(text, encoding)) => (text, encoding),
                Some(SideText::Binary) => return Ok(binary_diff_result()),
                Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
                None => (String::new(), None),
            },
            // 不在暂存区 = 未跟踪 / 已暂存删除,全部算新增
            None => (String::new(), None),
        },
        DiffSide::Staged | DiffSide::Working => match head_text(&repo, &head_rel)? {
            Some(SideText::Text(text, encoding)) => (text, encoding),
            Some(SideText::Binary) => return Ok(binary_diff_result()),
            Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
            None => (String::new(), None),
        },
    };

    let mut result = diff_two_texts(old_content, new_content);
    result.encoding = diff_encoding(old_encoding, new_encoding);
    Ok(result)
}

/// index / blob 一侧的取值结果。`Text` 的第二项见 [`decode_side`]:`None` = 原样的
/// UTF-8,`Some` = 非 UTF-8 解出来的(diff 照常显示,块操作一律拒绝)。
pub(super) enum SideText {
    Text(String, Option<TextEncoding>),
    Binary,
    TooLarge,
}

/// 暂存区 stage 0 条目的内容。`None` = 该路径不在暂存区(未跟踪 / 已暂存删除 / 冲突中)。
pub(super) fn index_text(repo: &Repository, rel_path: &str) -> Result<Option<SideText>> {
    let index = repo.index()?;
    let Some(entry) = index.get_path(Path::new(rel_path), 0) else {
        return Ok(None);
    };
    // 大小先从对象头取:超限的不读内容
    if blob_too_large(repo, entry.id)? {
        return Ok(Some(SideText::TooLarge));
    }
    let blob = repo.find_blob(entry.id)?;
    Ok(Some(match decode_side(blob.content(), blob.is_binary()) {
        Some((text, encoding)) => SideText::Text(text, encoding),
        None => SideText::Binary,
    }))
}

/// 该路径在暂存区里有没有冲突条目(stage 1~3)。
pub(super) fn is_conflicted(repo: &Repository, rel_path: &str) -> Result<bool> {
    let index = repo.index()?;
    let path = Path::new(rel_path);
    Ok((1..=3).any(|stage| index.get_path(path, stage).is_some()))
}
