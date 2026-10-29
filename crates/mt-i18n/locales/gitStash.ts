/**
 * 「贮藏（git stash）」：贮藏列表弹窗（应用 / 弹出 / 删除 / 全部清除 / 从贮藏新建分支 /
 * 查看内容）、新建贮藏弹窗，以及检出被本地改动挡住时的「贮藏后检出」确认框。
 * GPUI 版新增的能力，原版没有对应文案。分支名校验文案复用 `gitBranch.name*`。
 */
export const gitStash = {
  zh: {
    title: "贮藏",
    empty: "没有贮藏",
    loadFailed: "读取贮藏失败：{detail}",
    stashChanges: "贮藏当前改动…",
    refresh: "刷新",
    clearAll: "全部清除…",
    footerHint: "↑↓ 切换 · Enter 应用 · Delete 删除",
    untracked: "未跟踪",
    noMessage: "（无消息）",
    copyHash: "复制哈希",

    // ── 所选贮藏的操作 ──
    action: {
      apply: "应用",
      applyTip: "把这个贮藏的改动应用到工作区，贮藏保留（Enter）",
      pop: "弹出",
      popTip: "应用后删除这个贮藏；出现冲突时贮藏保留",
      reinstateIndex: "恢复暂存区状态",
      reinstateIndexTip: "连贮藏时的暂存状态一起恢复（--index）",
      newBranch: "从此新建分支…",
      newBranchTip: "在贮藏时的提交上新建并检出分支，再应用这个贮藏",
      drop: "删除",
      dropTip: "删除这个贮藏（Delete）",
    },
    busy: {
      apply: "正在应用…",
      pop: "正在弹出…",
      drop: "正在删除…",
      clear: "正在清除…",
    },
    result: {
      applied: "已应用 {reference}",
      popped: "已弹出 {reference}",
      dropped: "已删除 {reference}",
      cleared: "已清除全部贮藏",
      branched: "已从 {reference} 新建并检出分支 {name}",
      conflicts: "出现冲突，请在更改区解决；贮藏已保留",
      failed: "操作没有完成，git 的原文如下：",
      failedTitle: "贮藏操作失败",
      moreFiles: "…以及另外 {count} 个",
    },

    // ── 改动文件 ──
    files: {
      header: "改动的文件 · {count}",
      loading: "正在读取改动…",
      failed: "读取改动失败：{detail}",
      empty: "这个贮藏里没有改动",
      selectHint: "选择一个文件查看改动",
      untrackedTip: "未跟踪文件（贮藏时一并收起）",
    },

    // ── 危险操作确认 ──
    confirm: {
      cancel: "取消",
      dropTitle: "删除贮藏",
      dropMessage: "确定删除 {reference}「{message}」吗？\n删除后不能从列表恢复。",
      dropOk: "删除",
      clearTitle: "清除全部贮藏",
      clearMessage: "确定删除全部 {count} 个贮藏吗？\n此操作不可恢复。",
      clearOk: "全部清除",
    },

    // ── 从贮藏新建分支 ──
    branch: {
      title: "从贮藏新建分支",
      base: "基于 {reference}：{message}",
      nameLabel: "分支名",
      ok: "创建并检出",
      working: "正在创建…",
    },

    // ── 新建贮藏 ──
    save: {
      title: "贮藏改动",
      repo: "仓库：{name}",
      messageLabel: "消息（可选）",
      placeholder: "留空则用 git 的默认消息（WIP on …）",
      keepIndex: "保留暂存区（--keep-index）",
      includeUntracked: "包含未跟踪文件",
      ok: "贮藏",
      working: "正在贮藏…",
      nothing: "没有可贮藏的改动",
      saved: "已贮藏为 {reference}",
      failedTitle: "贮藏失败",
    },

    // ── 贮藏后检出（Smart Checkout）──
    checkout: {
      title: "检出 {branch}",
      lead: "以下本地改动会被检出覆盖，git 拒绝了检出：",
      leadUnknown: "工作区里的本地改动挡住了检出。",
      explain: "「贮藏后检出」会先把全部本地改动（含未跟踪文件）贮藏起来，检出 {branch} 后再自动恢复；恢复时如有冲突，贮藏会保留。",
      ok: "贮藏后检出",
      working: "正在检出…",
      done: "已检出 {branch}",
      restored: "已检出 {branch}，本地改动已恢复",
      conflicts: "已检出 {branch}，但恢复本地改动时出现冲突：请在更改区解决；贮藏已保留（{reference}）",
      restoreFailed: "已检出 {branch}，但恢复本地改动失败；贮藏已保留（{reference}），可在贮藏列表里手动应用。git 的原文：",
      failedTitle: "检出失败",
      gotIt: "知道了",
    },
  },
  en: {
    title: "Stashes",
    empty: "No stashes",
    loadFailed: "Failed to load stashes: {detail}",
    stashChanges: "Stash Changes…",
    refresh: "Refresh",
    clearAll: "Clear All…",
    footerHint: "↑↓ Switch · Enter Apply · Delete Drop",
    untracked: "Untracked",
    noMessage: "(no message)",
    copyHash: "Copy Hash",

    action: {
      apply: "Apply",
      applyTip: "Apply this stash to the working tree and keep it (Enter)",
      pop: "Pop",
      popTip: "Apply, then drop this stash; it is kept if there are conflicts",
      reinstateIndex: "Restore index",
      reinstateIndexTip: "Also restore what was staged when stashing (--index)",
      newBranch: "New Branch from Stash…",
      newBranchTip: "Create and check out a branch at the stash's base commit, then apply this stash",
      drop: "Drop",
      dropTip: "Drop this stash (Delete)",
    },
    busy: {
      apply: "Applying…",
      pop: "Popping…",
      drop: "Dropping…",
      clear: "Clearing…",
    },
    result: {
      applied: "Applied {reference}",
      popped: "Popped {reference}",
      dropped: "Dropped {reference}",
      cleared: "Cleared all stashes",
      branched: "Created and checked out {name} from {reference}",
      conflicts: "Conflicts occurred. Resolve them in Changes; the stash was kept",
      failed: "The operation did not complete. Git said:",
      failedTitle: "Stash operation failed",
      moreFiles: "…and {count} more",
    },

    files: {
      header: "Changed files · {count}",
      loading: "Loading changes…",
      failed: "Failed to load changes: {detail}",
      empty: "This stash has no changes",
      selectHint: "Select a file to see its changes",
      untrackedTip: "Untracked file (stashed along with the changes)",
    },

    confirm: {
      cancel: "Cancel",
      dropTitle: "Drop Stash",
      dropMessage: "Drop {reference} \"{message}\"?\nIt can't be restored from the list afterwards.",
      dropOk: "Drop",
      clearTitle: "Clear All Stashes",
      clearMessage: "Drop all {count} stashes?\nThis cannot be undone.",
      clearOk: "Clear All",
    },

    branch: {
      title: "New Branch from Stash",
      base: "Based on {reference}: {message}",
      nameLabel: "Branch name",
      ok: "Create and Checkout",
      working: "Creating…",
    },

    save: {
      title: "Stash Changes",
      repo: "Repository: {name}",
      messageLabel: "Message (optional)",
      placeholder: "Leave empty to use Git's default message (WIP on …)",
      keepIndex: "Keep index (--keep-index)",
      includeUntracked: "Include untracked files",
      ok: "Stash",
      working: "Stashing…",
      nothing: "No local changes to stash",
      saved: "Stashed as {reference}",
      failedTitle: "Stash failed",
    },

    checkout: {
      title: "Checkout {branch}",
      lead: "These local changes would be overwritten, so Git refused to check out:",
      leadUnknown: "Local changes in the working tree block the checkout.",
      explain: "\"Stash and Checkout\" stashes all local changes (including untracked files), checks out {branch}, then restores them; the stash is kept if restoring conflicts.",
      ok: "Stash and Checkout",
      working: "Checking out…",
      done: "Checked out {branch}",
      restored: "Checked out {branch}; local changes restored",
      conflicts: "Checked out {branch}, but restoring local changes caused conflicts. Resolve them in Changes; the stash was kept ({reference})",
      restoreFailed: "Checked out {branch}, but local changes could not be restored. The stash was kept ({reference}); apply it manually from the stash list. Git said:",
      failedTitle: "Checkout failed",
      gotIt: "Got It",
    },
  },
} as const;
