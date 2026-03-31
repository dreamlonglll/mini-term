# Git 集成功能实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在文件树中集成只读 Git 状态显示（M/A/D/R/?/C 标记）和 diff 查看（Modal 弹窗，并排+内联切换）。

**Architecture:** Rust 后端新增 `git.rs` 模块，使用 `git2` crate 读取仓库状态和 diff 数据，通过 Tauri 命令暴露给前端。前端在 `FileTree.tsx` 中获取并展示 git 状态，新增 `DiffModal.tsx` 组件渲染 diff 视图。

**Tech Stack:** Rust + git2 crate, React 19 + TypeScript, Tailwind CSS v4

**Spec:** `docs/superpowers/specs/2026-03-31-git-integration-design.md`

---

## File Structure

### 新增文件

| 文件 | 职责 |
|------|------|
| `src-tauri/src/git.rs` | git2 仓库发现、`get_git_status` 和 `get_git_diff` 命令 |
| `src/components/DiffModal.tsx` | Diff 弹窗组件（并排+内联切换，loading/binary/too-large 状态） |

### 修改文件

| 文件 | 改动 |
|------|------|
| `src-tauri/Cargo.toml` | 添加 `git2`、`pathdiff` 依赖 |
| `src-tauri/src/lib.rs` | 添加 `mod git;`，注册两个命令 |
| `src/types.ts` | 新增 Git 相关类型 |
| `src/components/FileTree.tsx` | 加载 git status、TreeNode 标记、右键菜单、DiffModal 集成 |

---

## Task 1: 添加 git2 依赖并注册模块

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`
- Create: `src-tauri/src/git.rs` (空壳)

- [ ] **Step 1: 在 Cargo.toml 添加 git2 依赖**

在 `[dependencies]` 末尾添加：

```toml
git2 = "0.19"
```

- [ ] **Step 2: 创建 git.rs 空壳模块**

创建 `src-tauri/src/git.rs`：

```rust
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GitStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitFileStatus {
    pub path: String,
    pub old_path: Option<String>,
    pub status: GitStatus,
    pub status_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: String,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub old_content: String,
    pub new_content: String,
    pub hunks: Vec<DiffHunk>,
    pub is_binary: bool,
    pub too_large: bool,
}

#[tauri::command]
pub fn get_git_status(project_path: String) -> Result<Vec<GitFileStatus>, String> {
    Ok(vec![])
}

#[tauri::command]
pub fn get_git_diff(project_path: String, file_path: String) -> Result<GitDiffResult, String> {
    Err("Not implemented".to_string())
}
```

- [ ] **Step 3: 在 lib.rs 注册模块和命令**

在 `src-tauri/src/lib.rs` 中：

1. 在模块声明区域（第 1-5 行之后）添加 `mod git;`
2. 在 `generate_handler![]` 宏中添加 `git::get_git_status` 和 `git::get_git_diff`

修改后的 `lib.rs`：

```rust
mod ai_sessions;
mod config;
mod fs;
mod git;
mod process_monitor;
mod pty;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(pty::PtyManager::new())
        .manage(fs::FsWatcherManager::new())
        .setup(|app| {
            let pty_manager = app.state::<crate::pty::PtyManager>();
            let pty_clone = pty_manager.inner().clone();
            process_monitor::start_monitor(app.handle().clone(), pty_clone);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::load_config,
            config::save_config,
            pty::create_pty,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_pty,
            fs::list_directory,
            fs::watch_directory,
            fs::unwatch_directory,
            fs::create_file,
            fs::create_directory,
            ai_sessions::get_ai_sessions,
            git::get_git_status,
            git::get_git_diff,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- [ ] **Step 4: 验证编译通过**

运行：`cd src-tauri && cargo build 2>&1`

预期：编译成功，无错误（git2 首次编译可能需要几分钟）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/git.rs src-tauri/src/lib.rs
git commit -m "feat: add git2 dependency and scaffold git module"
```

---

## Task 2: 实现 get_git_status 命令

**Files:**
- Modify: `src-tauri/src/git.rs`
- Modify: `src-tauri/Cargo.toml`

- [ ] **Step 0: 在 Cargo.toml 添加 pathdiff 依赖**

在 `[dependencies]` 末尾添加：

```toml
pathdiff = "0.2"
```

- [ ] **Step 1: 实现仓库发现和状态映射辅助函数**

在 `git.rs` 中，在数据结构定义之后、命令函数之前，添加辅助函数：

```rust
use git2::{Repository, StatusOptions, Status};
use std::path::PathBuf;

/// 将 git2 Status 位标志转换为 GitFileStatus
/// is_empty_repo: 空仓库时 WT_NEW 映射为 Added 而非 Untracked
fn map_status(status: Status, is_empty_repo: bool) -> (GitStatus, &'static str) {
    if status.intersects(Status::CONFLICTED) {
        (GitStatus::Conflicted, "C")
    } else if status.intersects(Status::INDEX_DELETED | Status::WT_DELETED) {
        (GitStatus::Deleted, "D")
    } else if status.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED) {
        (GitStatus::Renamed, "R")
    } else if status.intersects(Status::INDEX_NEW) {
        (GitStatus::Added, "A")
    } else if status.intersects(Status::INDEX_MODIFIED | Status::WT_MODIFIED) {
        (GitStatus::Modified, "M")
    } else if status.intersects(Status::WT_NEW) {
        if is_empty_repo {
            (GitStatus::Added, "A")
        } else {
            (GitStatus::Untracked, "?")
        }
    } else {
        (GitStatus::Modified, "M")
    }
}

/// 获取单个仓库内所有变更文件
fn collect_repo_status(
    repo: &Repository,
    path_prefix: &str,
) -> Result<Vec<GitFileStatus>, String> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let is_empty_repo = repo.head().is_err();
    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| e.to_string())?;
    let mut result = Vec::new();

    for entry in statuses.iter() {
        let raw_path = entry.path().unwrap_or_default().to_string();
        let (git_status, label) = map_status(entry.status(), is_empty_repo);

        let full_path = if path_prefix.is_empty() {
            raw_path.clone()
        } else {
            format!("{}/{}", path_prefix, raw_path)
        };

        // Renamed 文件的旧路径
        let old_path = if matches!(git_status, GitStatus::Renamed) {
            entry.head_to_index()
                .and_then(|d| d.old_file().path().map(|p| p.to_string_lossy().to_string()))
                .map(|p| if path_prefix.is_empty() { p } else { format!("{}/{}", path_prefix, p) })
        } else {
            None
        };

        result.push(GitFileStatus {
            path: full_path.replace('\\', "/"),
            old_path,
            status: git_status,
            status_label: label.to_string(),
        });
    }

    Ok(result)
}
```

- [ ] **Step 2: 实现 get_git_status 命令**

替换 `get_git_status` 函数：

```rust
#[tauri::command]
pub fn get_git_status(project_path: String) -> Result<Vec<GitFileStatus>, String> {
    let project = PathBuf::from(&project_path);
    if !project.is_dir() {
        return Err(format!("目录不存在: {}", project_path));
    }

    // 尝试将 project_path 本身作为 git 仓库
    if let Ok(repo) = Repository::discover(&project) {
        let repo_root = repo.workdir().ok_or("无法获取仓库工作目录")?.to_path_buf();
        // 计算 project_path 相对于仓库根的前缀（用于子目录指向仓库时过滤路径）
        let prefix = pathdiff::diff_paths(&project, &repo_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let prefix = if prefix == "." { String::new() } else { prefix };

        let all = collect_repo_status(&repo, "")?;
        if prefix.is_empty() {
            return Ok(all);
        }
        // 过滤出属于 project_path 子目录的文件
        let normalized_prefix = prefix.replace('\\', "/");
        return Ok(all.into_iter().filter(|f| f.path.starts_with(&normalized_prefix)).collect());
    }

    // 扫描直接子目录
    let mut all_statuses = Vec::new();
    let entries = std::fs::read_dir(&project).map_err(|e| e.to_string())?;

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let sub_dir = entry.path();
        if let Ok(repo) = Repository::discover(&sub_dir) {
            let sub_name = entry.file_name().to_string_lossy().to_string();
            let statuses = collect_repo_status(&repo, &sub_name)?;
            all_statuses.extend(statuses);
        }
    }

    Ok(all_statuses)
}
```

- [ ] **Step 3: 验证编译通过**

运行：`cd src-tauri && cargo build 2>&1`

预期：编译成功。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/git.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: implement get_git_status command with multi-repo discovery"
```

---

## Task 3: 实现 get_git_diff 命令

**Files:**
- Modify: `src-tauri/src/git.rs`

- [ ] **Step 1: 实现 get_git_diff 命令**

替换 `get_git_diff` 函数：

```rust
const MAX_DIFF_SIZE: u64 = 1_048_576; // 1MB

#[tauri::command]
pub fn get_git_diff(project_path: String, file_path: String) -> Result<GitDiffResult, String> {
    let project = PathBuf::from(&project_path);
    let abs_path = project.join(&file_path);

    // 定位所属仓库
    let repo = Repository::discover(&abs_path)
        .map_err(|e| format!("无法定位 Git 仓库: {}", e))?;
    let repo_root = repo.workdir().ok_or("无法获取仓库工作目录")?;

    // 计算相对于仓库根的路径
    let rel_path = pathdiff::diff_paths(&abs_path, repo_root)
        .ok_or("无法计算相对路径")?;
    let rel_str = rel_path.to_string_lossy().replace('\\', "/");

    // 检查文件大小
    if abs_path.exists() {
        let metadata = std::fs::metadata(&abs_path).map_err(|e| e.to_string())?;
        if metadata.len() > MAX_DIFF_SIZE {
            return Ok(GitDiffResult {
                old_content: String::new(),
                new_content: String::new(),
                hunks: vec![],
                is_binary: false,
                too_large: true,
            });
        }
    }

    // 获取 HEAD 版本（旧内容）
    let old_content = get_head_content(&repo, &rel_str);

    // 获取工作区版本（新内容）
    let new_content = if abs_path.exists() {
        match std::fs::read(&abs_path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => {
                    return Ok(GitDiffResult {
                        old_content: String::new(),
                        new_content: String::new(),
                        hunks: vec![],
                        is_binary: true,
                        too_large: false,
                    });
                }
            },
            Err(e) => return Err(format!("无法读取文件: {}", e)),
        }
    } else {
        None // 文件已删除
    };

    let old = old_content.unwrap_or_default();
    let new = new_content.unwrap_or_default();

    // 生成 hunk 信息
    let hunks = compute_diff_hunks(&old, &new);

    Ok(GitDiffResult {
        old_content: old,
        new_content: new,
        hunks,
        is_binary: false,
        too_large: false,
    })
}

/// 从 HEAD commit 读取文件内容
fn get_head_content(repo: &Repository, rel_path: &str) -> Option<String> {
    let head = repo.head().ok()?;
    let commit = head.peel_to_commit().ok()?;
    let tree = commit.tree().ok()?;
    let entry = tree.get_path(Path::new(rel_path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;

    if blob.is_binary() {
        return None;
    }

    String::from_utf8(blob.content().to_vec()).ok()
}

/// 使用简单的行级 diff 算法生成 hunk
fn compute_diff_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // 行数过大时退化为全量替换，防止 O(n*m) LCS 的内存溢出
    // 3000 * 3000 * 4 bytes ≈ 36MB，可接受上限
    if (old_lines.len() as u64) * (new_lines.len() as u64) > 10_000_000 {
        let mut lines = Vec::new();
        for (i, line) in old_lines.iter().enumerate() {
            lines.push(DiffLine {
                kind: "delete".to_string(),
                content: line.to_string(),
                old_lineno: Some(i as u32 + 1),
                new_lineno: None,
            });
        }
        for (i, line) in new_lines.iter().enumerate() {
            lines.push(DiffLine {
                kind: "add".to_string(),
                content: line.to_string(),
                old_lineno: None,
                new_lineno: Some(i as u32 + 1),
            });
        }
        return vec![DiffHunk {
            old_start: 1,
            old_lines: old_lines.len() as u32,
            new_start: 1,
            new_lines: new_lines.len() as u32,
            lines,
        }];
    }

    let diff_results = simple_diff(&old_lines, &new_lines);
    let mut lines = Vec::new();

    for item in &diff_results {
        match item {
            DiffItem::Equal(old_line, new_line, text) => {
                lines.push(DiffLine {
                    kind: "context".to_string(),
                    content: text.to_string(),
                    old_lineno: Some(*old_line as u32 + 1),
                    new_lineno: Some(*new_line as u32 + 1),
                });
            }
            DiffItem::Delete(old_line, text) => {
                lines.push(DiffLine {
                    kind: "delete".to_string(),
                    content: text.to_string(),
                    old_lineno: Some(*old_line as u32 + 1),
                    new_lineno: None,
                });
            }
            DiffItem::Insert(new_line, text) => {
                lines.push(DiffLine {
                    kind: "add".to_string(),
                    content: text.to_string(),
                    old_lineno: None,
                    new_lineno: Some(*new_line as u32 + 1),
                });
            }
        }
    }

    if lines.is_empty() {
        return vec![];
    }

    // 将所有行打包成一个 hunk（简化实现）
    // 后续可优化为按上下文行分割多个 hunk
    vec![DiffHunk {
        old_start: 1,
        old_lines: old_lines.len() as u32,
        new_start: 1,
        new_lines: new_lines.len() as u32,
        lines,
    }]
}

enum DiffItem<'a> {
    Equal(usize, usize, &'a str),
    Delete(usize, &'a str),
    Insert(usize, &'a str),
}

/// 基于 LCS 的简单 diff 算法
fn simple_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffItem<'a>> {
    let n = old.len();
    let m = new.len();

    // 构建 LCS 表
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // 回溯生成 diff
    let mut result = Vec::new();
    let mut i = n;
    let mut j = m;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            result.push(DiffItem::Equal(i - 1, j - 1, old[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            result.push(DiffItem::Insert(j - 1, new[j - 1]));
            j -= 1;
        } else {
            result.push(DiffItem::Delete(i - 1, old[i - 1]));
            i -= 1;
        }
    }

    result.reverse();
    result
}
```

- [ ] **Step 2: 验证编译通过**

运行：`cd src-tauri && cargo build 2>&1`

预期：编译成功。

- [ ] **Step 3: 提交**

```bash
git add src-tauri/src/git.rs
git commit -m "feat: implement get_git_diff command with LCS-based diff"
```

---

## Task 4: 添加前端 Git 类型定义

**Files:**
- Modify: `src/types.ts`

- [ ] **Step 1: 在 types.ts 末尾添加 Git 类型**

在 `src/types.ts` 的 `FsChangePayload` 接口后（第 122 行之后）添加：

```typescript

// === Git 状态 ===

export type GitStatusType = 'modified' | 'added' | 'deleted' | 'renamed' | 'untracked' | 'conflicted';

export interface GitFileStatus {
  path: string;
  oldPath?: string;
  status: GitStatusType;
  statusLabel: string; // "M", "A", "D", "R", "?", "C"
}

export interface DiffHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  lines: DiffLine[];
}

export interface DiffLine {
  kind: 'add' | 'delete' | 'context';
  content: string;
  oldLineno?: number;
  newLineno?: number;
}

export interface GitDiffResult {
  oldContent: string;
  newContent: string;
  hunks: DiffHunk[];
  isBinary: boolean;
  tooLarge: boolean;
}
```

- [ ] **Step 2: 提交**

```bash
git add src/types.ts
git commit -m "feat: add Git-related TypeScript type definitions"
```

---

## Task 5: 实现 DiffModal 组件

**Files:**
- Create: `src/components/DiffModal.tsx`

- [ ] **Step 1: 创建 DiffModal 组件**

创建 `src/components/DiffModal.tsx`。

组件结构：
- Props: `{ open, onClose, projectPath, status }`
- 状态: `viewMode` (side-by-side | inline), `diffResult` (loading | data | error), `loading`
- 打开时调用 `invoke('get_git_diff')` 获取数据
- ESC 关闭
- 工具栏：文件名 + 状态标记 + 视图切换按钮
- 两种渲染模式：SideBySideView 和 InlineView

```tsx
import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { GitFileStatus, GitDiffResult, DiffLine } from '../types';

interface DiffModalProps {
  open: boolean;
  onClose: () => void;
  projectPath: string;
  status: GitFileStatus;
}

type ViewMode = 'side-by-side' | 'inline';

// ─── InlineView ───

function InlineView({ hunks }: { hunks: GitDiffResult['hunks'] }) {
  return (
    <div className="font-mono text-sm leading-6">
      {hunks.map((hunk, hi) => (
        <div key={hi}>
          {hunk.lines.map((line, li) => (
            <div
              key={li}
              className={`flex ${
                line.kind === 'add'
                  ? 'bg-[rgba(60,180,60,0.12)]'
                  : line.kind === 'delete'
                  ? 'bg-[rgba(220,60,60,0.12)]'
                  : ''
              }`}
            >
              <span className="w-12 text-right pr-2 text-[var(--text-muted)] select-none flex-shrink-0 opacity-50">
                {line.kind === 'add' ? '+' : line.kind === 'delete' ? '-' : (line.oldLineno ?? '')}
              </span>
              <span
                className={`flex-1 whitespace-pre px-2 ${
                  line.kind === 'add'
                    ? 'text-green-400'
                    : line.kind === 'delete'
                    ? 'text-red-400'
                    : 'text-[var(--text-primary)]'
                }`}
              >
                {line.content}
              </span>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

// ─── SideBySideView ───

function SideBySideView({ hunks }: { hunks: GitDiffResult['hunks'] }) {
  // 将 hunk lines 拆分为左右两列
  const rows: { left?: DiffLine; right?: DiffLine }[] = [];

  for (const hunk of hunks) {
    let i = 0;
    while (i < hunk.lines.length) {
      const line = hunk.lines[i];
      if (line.kind === 'context') {
        rows.push({ left: line, right: line });
        i++;
      } else if (line.kind === 'delete') {
        // 收集连续 delete，然后配对连续 add
        const deletes: DiffLine[] = [];
        while (i < hunk.lines.length && hunk.lines[i].kind === 'delete') {
          deletes.push(hunk.lines[i]);
          i++;
        }
        const adds: DiffLine[] = [];
        while (i < hunk.lines.length && hunk.lines[i].kind === 'add') {
          adds.push(hunk.lines[i]);
          i++;
        }
        const maxLen = Math.max(deletes.length, adds.length);
        for (let j = 0; j < maxLen; j++) {
          rows.push({
            left: deletes[j] ?? undefined,
            right: adds[j] ?? undefined,
          });
        }
      } else if (line.kind === 'add') {
        rows.push({ left: undefined, right: line });
        i++;
      } else {
        i++;
      }
    }
  }

  const renderCell = (line: DiffLine | undefined, side: 'left' | 'right') => {
    if (!line) {
      return (
        <div className="flex h-full bg-[var(--bg-base)] opacity-30">
          <span className="w-12 flex-shrink-0" />
          <span className="flex-1" />
        </div>
      );
    }
    const isAdd = line.kind === 'add';
    const isDel = line.kind === 'delete';
    return (
      <div
        className={`flex ${
          isAdd ? 'bg-[rgba(60,180,60,0.12)]' : isDel ? 'bg-[rgba(220,60,60,0.12)]' : ''
        }`}
      >
        <span className="w-12 text-right pr-2 text-[var(--text-muted)] select-none flex-shrink-0 opacity-50">
          {side === 'left' ? (line.oldLineno ?? '') : (line.newLineno ?? '')}
        </span>
        <span
          className={`flex-1 whitespace-pre px-2 ${
            isAdd ? 'text-green-400' : isDel ? 'text-red-400' : 'text-[var(--text-primary)]'
          }`}
        >
          {line.content}
        </span>
      </div>
    );
  };

  return (
    <div className="flex font-mono text-sm leading-6 h-full">
      <div className="flex-1 overflow-auto border-r border-[var(--border-subtle)]">
        {rows.map((row, i) => (
          <div key={i}>{renderCell(row.left, 'left')}</div>
        ))}
      </div>
      <div className="flex-1 overflow-auto">
        {rows.map((row, i) => (
          <div key={i}>{renderCell(row.right, 'right')}</div>
        ))}
      </div>
    </div>
  );
}

// ─── DiffModal ───

export function DiffModal({ open, onClose, projectPath, status }: DiffModalProps) {
  const [viewMode, setViewMode] = useState<ViewMode>('side-by-side');
  const [diffResult, setDiffResult] = useState<GitDiffResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    if (!open) return;
    setLoading(true);
    setError('');
    setDiffResult(null);

    invoke<GitDiffResult>('get_git_diff', {
      projectPath,
      filePath: status.path,
    })
      .then(setDiffResult)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, projectPath, status.path]);

  useEffect(() => {
    if (!open) return;
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [open, onClose]);

  if (!open) return null;

  const fileName = status.path.split('/').pop() ?? status.path;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative flex flex-col overflow-hidden bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-2xl animate-slide-in"
        style={{ width: '90vw', height: '80vh' }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* 工具栏 */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2">
            <span className="text-base font-medium text-[var(--accent)]">{fileName}</span>
            <span className="text-sm text-[var(--text-muted)] truncate max-w-[300px]">
              {status.path}
            </span>
            <span className="px-2 py-0.5 text-xs rounded bg-[var(--bg-elevated)] text-[var(--text-muted)] border border-[var(--border-subtle)]">
              {status.statusLabel}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden">
              <button
                className={`px-3 py-1 text-sm transition-colors ${
                  viewMode === 'side-by-side'
                    ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                    : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                }`}
                onClick={() => setViewMode('side-by-side')}
              >
                并排
              </button>
              <button
                className={`px-3 py-1 text-sm transition-colors ${
                  viewMode === 'inline'
                    ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                    : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                }`}
                onClick={() => setViewMode('inline')}
              >
                内联
              </button>
            </div>
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none ml-2"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 内容区 */}
        <div className="flex-1 overflow-auto bg-[var(--bg-base)]">
          {loading && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              加载中...
            </div>
          )}
          {error && (
            <div className="flex items-center justify-center h-full text-[var(--color-error)]">
              {error}
            </div>
          )}
          {diffResult && diffResult.isBinary && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              二进制文件，不支持 diff 预览
            </div>
          )}
          {diffResult && diffResult.tooLarge && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              文件过大（&gt;1MB），不支持 diff 预览
            </div>
          )}
          {diffResult && !diffResult.isBinary && !diffResult.tooLarge && (
            viewMode === 'side-by-side'
              ? <SideBySideView hunks={diffResult.hunks} />
              : <InlineView hunks={diffResult.hunks} />
          )}
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: 验证前端编译通过**

运行：`npm run build 2>&1`

预期：编译成功（DiffModal 尚未被引用，tree-shaking 不会报错）。

- [ ] **Step 3: 提交**

```bash
git add src/components/DiffModal.tsx
git commit -m "feat: add DiffModal component with side-by-side and inline views"
```

---

## Task 6: 在 FileTree 中集成 Git 状态和 DiffModal

**Files:**
- Modify: `src/components/FileTree.tsx`

这是最关键的改动，需要修改 FileTree 和 TreeNode。

- [ ] **Step 1: 添加 imports 和 git status 状态管理**

在 `FileTree.tsx` 顶部：

1. 在 import 的类型中追加 `GitFileStatus`
2. 新增 `DiffModal` 的 import
3. 新增 `useRef` 的 import

修改 import 区域：

```typescript
import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { revealItemInDir, openPath } from '@tauri-apps/plugin-opener';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useAppStore } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { showContextMenu } from '../utils/contextMenu';
import { showPrompt } from '../utils/prompt';
import { DiffModal } from './DiffModal';
import type { FileEntry, FsChangePayload, GitFileStatus, PtyOutputPayload } from '../types';
```

- [ ] **Step 2: 修改 TreeNodeProps 接口和 TreeNode 组件**

1. 扩展 `TreeNodeProps`，添加 `gitStatusMap` 和 `onViewDiff` 回调
2. 在 TreeNode 渲染中添加状态标记
3. 在右键菜单中添加"查看变更"选项

新的 `TreeNodeProps` 和 `TreeNode`：

```typescript
interface TreeNodeProps {
  entry: FileEntry;
  projectRoot: string;
  depth: number;
  gitStatusMap: Map<string, GitFileStatus>;
  onViewDiff: (status: GitFileStatus) => void;
}
```

在 `TreeNode` 函数签名中解构新 props：

```typescript
function TreeNode({ entry, projectRoot, depth, gitStatusMap, onViewDiff }: TreeNodeProps) {
```

找到 TreeNode 中渲染文件名的 `<span className="truncate">{entry.name}</span>` 行（原第 131 行），改为：

```tsx
<span className="truncate">{entry.name}</span>
{(() => {
  const rel = getRelativePath(entry.path, projectRoot).replace(/\\/g, '/');
  const fileStatus = gitStatusMap.get(rel);
  if (fileStatus) {
    return (
      <span className="ml-1 text-xs text-[var(--text-muted)] opacity-60 flex-shrink-0">
        {fileStatus.statusLabel}
      </span>
    );
  }
  // 文件夹聚合：检查是否有子文件变更
  if (entry.isDir) {
    const prefix = rel.endsWith('/') ? rel : rel + '/';
    const PRIORITY: Record<string, number> = { C: 6, D: 5, M: 4, A: 3, R: 2, '?': 1 };
    let bestLabel = '';
    let bestPriority = 0;
    for (const [path, s] of gitStatusMap) {
      if (path.startsWith(prefix)) {
        const p = PRIORITY[s.statusLabel] ?? 0;
        if (p > bestPriority) {
          bestPriority = p;
          bestLabel = s.statusLabel;
        }
      }
    }
    if (bestLabel) {
      return (
        <span className="ml-1 text-xs text-[var(--text-muted)] opacity-40 flex-shrink-0">
          {bestLabel}
        </span>
      );
    }
  }
  return null;
})()}
```

在 TreeNode 的右键菜单中（`onContextMenu` handler），在 `showContextMenu` 调用之前，检查 git status 并添加菜单项：

在构建 `items` 数组后、调用 `showContextMenu` 之前，添加：

```typescript
// 查看变更菜单项
const relForGit = getRelativePath(entry.path, projectRoot).replace(/\\/g, '/');
const entryGitStatus = gitStatusMap.get(relForGit);
if (entryGitStatus && !entry.isDir) {
  items.push({ separator: true });
  items.push({
    label: '查看变更',
    onClick: () => onViewDiff(entryGitStatus),
  });
}
```

在 TreeNode 中点击文件的 handler（`onClick={handleToggle}`）添加 diff 入口 — 修改 `handleToggle` 让非目录文件如果有 git 变更则打开 diff：

```typescript
const handleToggle = useCallback(async () => {
  if (!entry.isDir) {
    // 有 git 变更则打开 diff
    const rel = getRelativePath(entry.path, projectRoot).replace(/\\/g, '/');
    const fileStatus = gitStatusMap.get(rel);
    if (fileStatus) {
      onViewDiff(fileStatus);
    }
    return;
  }
  if (!expanded) {
    await loadChildren();
    invoke('watch_directory', { path: entry.path, projectPath: projectRoot });
  } else {
    invoke('unwatch_directory', { path: entry.path });
  }
  setExpanded(!expanded);
}, [entry, expanded, loadChildren, projectRoot, gitStatusMap, onViewDiff]);
```

递归渲染子节点时传递新 props：

```tsx
{expanded &&
  children.map((child) => (
    <TreeNode
      key={child.path}
      entry={child}
      projectRoot={projectRoot}
      depth={depth + 1}
      gitStatusMap={gitStatusMap}
      onViewDiff={onViewDiff}
    />
  ))}
```

- [ ] **Step 3: 修改 FileTree 组件，添加 git status 获取和 DiffModal**

在 `FileTree` 组件内部添加：

1. git status state 和 diff modal state
2. 加载 git status 的 useEffect
3. fs-change 防抖刷新
4. pty-output 启发式检测
5. DiffModal 渲染

在 `FileTree` 函数中，在 `rootEntries` state 之后添加：

```typescript
const [gitStatusMap, setGitStatusMap] = useState<Map<string, GitFileStatus>>(new Map());
const [diffTarget, setDiffTarget] = useState<GitFileStatus | null>(null);
const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

// 加载 git status
const loadGitStatus = useCallback(() => {
  if (!project) return;
  invoke<GitFileStatus[]>('get_git_status', { projectPath: project.path })
    .then((statuses) => {
      const map = new Map<string, GitFileStatus>();
      for (const s of statuses) map.set(s.path, s);
      setGitStatusMap(map);
    })
    .catch(() => setGitStatusMap(new Map()));
}, [project?.path]);

// 项目切换时加载 git status
useEffect(() => {
  loadGitStatus();
}, [loadGitStatus]);

// 防抖刷新 git status
const debouncedRefresh = useCallback(() => {
  if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
  refreshTimerRef.current = setTimeout(loadGitStatus, 500);
}, [loadGitStatus]);

// fs-change 触发刷新
useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
  if (project && payload.projectPath === project.path) {
    debouncedRefresh();
  }
}, [project?.path, debouncedRefresh]));

// pty-output 启发式检测 git 命令
const GIT_PATTERNS = [/create mode/, /Switched to/, /Already up to date/, /insertions?\(\+\)/, /deletions?\(-\)/];
useTauriEvent<PtyOutputPayload>('pty-output', useCallback((payload: PtyOutputPayload) => {
  if (GIT_PATTERNS.some((p) => p.test(payload.data))) {
    debouncedRefresh();
  }
}, [debouncedRefresh]));

const handleViewDiff = useCallback((status: GitFileStatus) => {
  setDiffTarget(status);
}, []);
```

修改 `rootEntries.map` 的 TreeNode 调用，传递新 props：

```tsx
{rootEntries.map((entry) => (
  <TreeNode
    key={entry.path}
    entry={entry}
    projectRoot={project.path}
    depth={0}
    gitStatusMap={gitStatusMap}
    onViewDiff={handleViewDiff}
  />
))}
```

在 FileTree 组件的 return JSX 末尾（`</div>` 闭合标签之前），添加 DiffModal：

```tsx
{diffTarget && (
  <DiffModal
    open={!!diffTarget}
    onClose={() => setDiffTarget(null)}
    projectPath={project.path}
    status={diffTarget}
  />
)}
```

- [ ] **Step 4: 验证前端编译通过**

运行：`npm run build 2>&1`

预期：编译成功。

- [ ] **Step 5: 提交**

```bash
git add src/components/FileTree.tsx
git commit -m "feat: integrate git status display and diff modal into file tree"
```

---

## Task 7: 端到端测试验证

**Files:** 无改动，纯测试

- [ ] **Step 1: 启动开发环境**

运行：`npm run tauri dev`

- [ ] **Step 2: 手动测试 git status 显示**

1. 添加一个已有 git 仓库的项目
2. 确认文件树中修改过的文件名右侧显示 `M`/`A`/`?` 等标记
3. 确认文件夹聚合显示正确

- [ ] **Step 3: 手动测试 diff 查看**

1. 点击一个有 `M` 标记的文件，确认 DiffModal 弹出
2. 验证并排视图正确（左旧右新，红绿高亮）
3. 切换到内联视图，验证 +/- 标记正确
4. 按 ESC 关闭 Modal
5. 右键有变更的文件，确认"查看变更"菜单项存在并可用

- [ ] **Step 4: 测试边界情况**

1. 添加一个非 git 仓库的项目 → 文件树无标记，功能正常
2. 在终端中执行 `git add` / `git commit` → 文件树标记自动刷新

- [ ] **Step 5: 提交最终调整（如有）**

如果测试中发现需要微调的问题，修复后提交。
