import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { Modal } from './Modal';
import { useAppStore, genId, saveConfigToDisk } from '../store';
import { newTerminal } from '../utils/paneActions';
import {
  disposeProjectTerminals,
  findProjectByPath,
  normalizePath,
  removeProjectWithCleanup,
} from '../utils/projectActions';
import { useT } from '../i18n';
import type { BranchInfo, GitRepoInfo, WorktreeInfo } from '../types';

interface Props {
  /** 目标路径;null = 弹窗关闭。可以是仓库根,也可以是尚未确定仓库的项目根目录 */
  repoPath: string | null;
  /**
   * true = 目标目录自身不是仓库时,按 Git 面板同一套逻辑向下发现子仓库;
   * 发现多个时可勾选若干个仓库批量新建 worktree。
   */
  discoverRepos?: boolean;
  /** worktree 集合变化(新建/删除/清理)后通知外层刷新仓库列表 */
  onChanged: () => void;
  onClose: () => void;
  /** 「开终端」的目标项目;缺省用当前激活项目(从项目右键菜单打开时应传右键的那个项目) */
  projectId?: string;
}

/** 一个仓库(按主工作区归并:主仓库与它的 linked worktree 只算一组) */
interface RepoGroup {
  /** 归并键 = 主仓库路径归一化后的形式 */
  key: string;
  /** 主仓库目录名(worktree 目录建议名的前缀) */
  name: string;
  /** git 命令的执行路径:worktree 增删必须落在主仓库上 */
  mainPath: string;
  worktrees: WorktreeInfo[];
  /** list_worktrees 失败(仓库损坏等),该组只展示错误 */
  error?: string;
}

/** 分支名 → 可用作目录名的片段(worktree 默认路径建议用) */
function sanitizeBranchForDir(branch: string): string {
  return branch.replace(/[\\/:*?"<>|\s]+/g, '-').replace(/^-+|-+$/g, '') || 'worktree';
}

function baseName(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? '';
}

function parentDir(p: string): string {
  const idx = Math.max(p.lastIndexOf('\\'), p.lastIndexOf('/'));
  return idx > 0 ? p.slice(0, idx) : '';
}

function joinPath(parent: string, child: string, sep: string): string {
  return `${parent.replace(/[\\/]+$/, '')}${sep}${child}`;
}

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const badgeCls =
  'shrink-0 text-xs leading-[16px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]';

/**
 * Worktree 管理弹窗:列出主工作区 + 全部 linked worktree,支持新建(现有分支 /
 * 新建分支)、删除(可强制)、清理失效条目、在终端打开、一键添加为项目。
 *
 * 目标目录自身不是仓库时(`discoverRepos`),向下发现子仓库;发现多个则每个仓库
 * 一组、组头可勾选,新建时对全部勾选的仓库各建一个 worktree。
 */
export function GitWorktreeModal({ repoPath, discoverRepos, onClose, onChanged, projectId }: Props) {
  const t = useT();
  // 订阅 projects:worktree 行的「已是项目」标识要跟着增删项目即时变化
  const projects = useAppStore((s) => s.config.projects);

  const [groups, setGroups] = useState<RepoGroup[] | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<string[]>([]);
  const [branchesByRepo, setBranchesByRepo] = useState<Map<string, BranchInfo[]>>(new Map());
  const [loadError, setLoadError] = useState<string | null>(null);

  // 新建表单
  const [mode, setMode] = useState<'existing' | 'new'>('existing');
  const [selBranch, setSelBranch] = useState('');
  const [newBranch, setNewBranch] = useState('');
  const [baseBranch, setBaseBranch] = useState('');
  const [wtPath, setWtPath] = useState('');
  const [pathEdited, setPathEdited] = useState(false);
  const [addAsProject, setAddAsProject] = useState(true);
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  /** 批量创建的逐仓库结果(仅在有失败时保留展示) */
  const [createResults, setCreateResults] = useState<{ key: string; name: string; error: string }[] | null>(null);

  // 删除确认(要带上所属仓库:git 命令跑在该组的主仓库上)
  const [removeTarget, setRemoveTarget] = useState<{ wt: WorktreeInfo; mainPath: string } | null>(null);
  const [removeForce, setRemoveForce] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);

  const [pruningKey, setPruningKey] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!repoPath) return;
    setLoadError(null);
    let repoPaths: string[];
    if (discoverRepos) {
      try {
        const repos = await invoke<GitRepoInfo[]>('discover_git_repos', { projectPath: repoPath });
        repoPaths = repos.map((r) => r.path);
      } catch (e) {
        setGroups([]);
        setLoadError(errMsg(e));
        return;
      }
    } else {
      repoPaths = [repoPath];
    }
    if (repoPaths.length === 0) {
      setGroups([]);
      return;
    }

    const loaded: { path: string; worktrees: WorktreeInfo[] | null; error?: string }[] =
      await Promise.all(
        repoPaths.map(async (p) => {
          try {
            return { path: p, worktrees: await invoke<WorktreeInfo[]>('list_worktrees', { repoPath: p }) };
          } catch (e) {
            return { path: p, worktrees: null, error: errMsg(e) };
          }
        }),
      );
    // 单仓库时沿用旧行为:加载失败即整体报错,不显示空壳分组
    if (loaded.length === 1 && loaded[0].error) {
      setGroups([]);
      setLoadError(loaded[0].error);
      return;
    }

    // 按主工作区归并:扫描可能同时发现主仓库与它在项目目录内的 worktree,
    // 两者的 list_worktrees 结果完全相同,合成一组才不会重复展示。
    const byMain = new Map<string, RepoGroup>();
    for (const item of loaded) {
      if (!item.worktrees) {
        const key = normalizePath(item.path);
        if (!byMain.has(key)) {
          byMain.set(key, {
            key,
            name: baseName(item.path),
            mainPath: item.path,
            worktrees: [],
            error: item.error ?? 'unknown error',
          });
        }
        continue;
      }
      const main = item.worktrees.find((w) => w.isMain);
      const mainPath = main?.path ?? item.path;
      const key = normalizePath(mainPath);
      if (byMain.has(key)) continue;
      byMain.set(key, {
        key,
        name: main?.name ?? baseName(mainPath),
        mainPath,
        worktrees: item.worktrees,
      });
    }
    const next = [...byMain.values()];
    setGroups(next);
    // 只剩一个仓库时自动勾选;重新加载保留原有勾选(已消失的键剔除)
    setSelectedKeys((prev) => {
      const kept = prev.filter((k) => next.some((g) => g.key === k));
      if (kept.length > 0) return kept;
      return next.length === 1 && !next[0].error ? [next[0].key] : [];
    });
  }, [repoPath, discoverRepos]);

  // 打开时重置表单并加载
  useEffect(() => {
    if (!repoPath) return;
    setGroups(null);
    setSelectedKeys([]);
    setBranchesByRepo(new Map());
    setLoadError(null);
    setMode('existing');
    setSelBranch('');
    setNewBranch('');
    setBaseBranch('');
    setWtPath('');
    setPathEdited(false);
    setAddAsProject(true);
    setCreating(false);
    setCreateError(null);
    setCreateResults(null);
    setRemoveTarget(null);
    load();
  }, [repoPath, load]);

  const selectedGroups = useMemo(
    () => (groups ?? []).filter((g) => !g.error && selectedKeys.includes(g.key)),
    [groups, selectedKeys],
  );
  const multiRepo = (groups?.length ?? 0) > 1;
  /** 多个仓库被勾选:路径输入框语义变成「父目录」,各仓库自动拼子目录 */
  const multiTarget = selectedGroups.length > 1;

  // 勾选的仓库按需拉分支(未勾选的仓库不必付出 git 调用成本)
  useEffect(() => {
    const missing = selectedGroups.filter((g) => !branchesByRepo.has(g.key));
    if (missing.length === 0) return;
    let cancelled = false;
    Promise.all(
      missing.map(async (g) => {
        try {
          return [g.key, await invoke<BranchInfo[]>('get_repo_branches', { repoPath: g.mainPath })] as const;
        } catch {
          // 失败也要落一条空记录,否则 effect 会反复重试
          return [g.key, [] as BranchInfo[]] as const;
        }
      }),
    ).then((pairs) => {
      if (cancelled) return;
      setBranchesByRepo((prev) => {
        const next = new Map(prev);
        for (const [k, v] of pairs) next.set(k, v);
        return next;
      });
    });
    return () => {
      cancelled = true;
    };
  }, [selectedGroups, branchesByRepo]);

  const branchesReady = selectedGroups.length > 0
    && selectedGroups.every((g) => branchesByRepo.has(g.key));

  /** 可检出分支:勾选的仓库都有、且在任一仓库都未被占用的本地分支(取交集) */
  const availableBranches = useMemo(() => {
    if (!branchesReady) return [];
    const perRepo = selectedGroups.map((g) => {
      const checkedOut = new Set(g.worktrees.filter((w) => w.branch).map((w) => w.branch!));
      return new Set(
        (branchesByRepo.get(g.key) ?? [])
          .filter((b) => !b.isRemote && !checkedOut.has(b.name))
          .map((b) => b.name),
      );
    });
    const first = branchesByRepo.get(selectedGroups[0].key) ?? [];
    return first
      .filter((b) => !b.isRemote)
      .map((b) => b.name)
      .filter((name) => perRepo.every((s) => s.has(name)));
  }, [branchesReady, selectedGroups, branchesByRepo]);

  /** 新分支起点:勾选的仓库都存在的分支(取交集,含远程) */
  const baseBranchOptions = useMemo(() => {
    if (!branchesReady) return [];
    const perRepo = selectedGroups.map(
      (g) => new Set((branchesByRepo.get(g.key) ?? []).map((b) => b.name)),
    );
    const first = branchesByRepo.get(selectedGroups[0].key) ?? [];
    return first.map((b) => b.name).filter((name) => perRepo.every((s) => s.has(name)));
  }, [branchesReady, selectedGroups, branchesByRepo]);

  const sep = (repoPath?.includes('\\') ?? false) ? '\\' : '/';
  const rootName = useMemo(() => baseName(repoPath ?? ''), [repoPath]);

  const effectiveBranch = mode === 'existing' ? selBranch : newBranch;

  /** 逐仓库的目标路径:单选=输入框原样;多选=父目录 + `<仓库名>-<分支>` */
  const targets = useMemo(() => {
    const path = wtPath.trim();
    if (!path) return [];
    return selectedGroups.map((g) => ({
      group: g,
      path: multiTarget
        ? joinPath(path, `${g.name}-${sanitizeBranchForDir(effectiveBranch)}`, sep)
        : path,
    }));
  }, [wtPath, selectedGroups, multiTarget, effectiveBranch, sep]);

  // 默认路径建议:单仓库 = 仓库同级的 `<仓库名>-<分支>`;多仓库 = 项目根目录(作父目录)。
  // 用户手动改过就不再跟随。
  useEffect(() => {
    if (!repoPath || pathEdited) return;
    if (selectedGroups.length === 0) {
      setWtPath('');
      return;
    }
    if (selectedGroups.length > 1) {
      setWtPath(repoPath);
      return;
    }
    const g = selectedGroups[0];
    const parent = parentDir(g.mainPath);
    if (!parent) return;
    setWtPath(joinPath(parent, `${g.name}-${sanitizeBranchForDir(effectiveBranch)}`, sep));
  }, [repoPath, selectedGroups, sep, effectiveBranch, pathEdited]);

  const handleBrowse = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const dir = selected as string;
    // 多仓库时选的就是父目录;单仓库时 worktree 目录本身必须是新路径,自动拼子目录
    setWtPath(
      multiTarget || selectedGroups.length === 0
        ? dir
        : joinPath(dir, `${selectedGroups[0].name}-${sanitizeBranchForDir(effectiveBranch)}`, sep),
    );
    setPathEdited(true);
  }, [sep, multiTarget, selectedGroups, effectiveBranch]);

  const toggleRepo = useCallback((key: string) => {
    setSelectedKeys((prev) => (prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]));
    setPathEdited(false);
    setSelBranch('');
    setBaseBranch('');
    setCreateError(null);
    setCreateResults(null);
  }, []);

  const toggleAll = useCallback(() => {
    const all = (groups ?? []).filter((g) => !g.error).map((g) => g.key);
    setSelectedKeys((prev) => (prev.length >= all.length ? [] : all));
    setPathEdited(false);
    setSelBranch('');
    setBaseBranch('');
    setCreateError(null);
    setCreateResults(null);
  }, [groups]);

  /**
   * 把某路径添加为项目(已存在则复用),挂在主仓库对应项目下作子项目;
   * 主仓库不是项目时回落到打开本弹窗的那个项目。返回项目 id。
   */
  const addProjectAt = useCallback((path: string, fallbackName: string, mainPath: string): string => {
    const { addProject, activeProjectId, config } = useAppStore.getState();
    const existing = findProjectByPath(path);
    if (existing) return existing.id;
    const ownerId = projectId ?? activeProjectId;
    const parent = findProjectByPath(mainPath) ?? config.projects.find((p) => p.id === ownerId);
    const id = genId();
    const name = baseName(path) || fallbackName;
    addProject({ id, name, path }, parent?.id);
    return id;
  }, [projectId]);

  const switchToProjectAt = useCallback((path: string, fallbackName: string, mainPath: string) => {
    const id = addProjectAt(path, fallbackName, mainPath);
    saveConfigToDisk();
    useAppStore.getState().setActiveProject(id);
  }, [addProjectAt]);

  const handleCreate = useCallback(async () => {
    const branch = (mode === 'existing' ? selBranch : newBranch).trim();
    if (!branch || targets.length === 0 || creating) return;
    setCreating(true);
    setCreateError(null);
    setCreateResults(null);
    try {
      const results = await Promise.all(
        targets.map(async (target) => {
          try {
            await invoke('add_worktree', {
              repoPath: target.group.mainPath,
              worktreePath: target.path,
              branch,
              createBranch: mode === 'new',
              base: mode === 'new' && baseBranch ? baseBranch : null,
            });
            return { target, error: null as string | null };
          } catch (e) {
            return { target, error: errMsg(e) };
          }
        }),
      );
      onChanged();
      // 分支集合已变(新建分支 / 分支被新工作区占用),缓存作废后按需重新拉取
      setBranchesByRepo(new Map());

      const created = results.filter((r) => !r.error);
      const failed = results.filter((r) => r.error);
      let firstNewProject: string | null = null;
      if (addAsProject && created.length > 0) {
        for (const r of created) {
          const id = addProjectAt(r.target.path, branch, r.target.group.mainPath);
          if (!firstNewProject) firstNewProject = id;
        }
        saveConfigToDisk();
      }

      if (failed.length > 0) {
        // 部分失败:留在弹窗里列出各仓库的错误,成功的那些已刷新进列表
        setCreateResults(
          failed.map((r) => ({ key: r.target.group.key, name: r.target.group.name, error: r.error! })),
        );
        await load();
        return;
      }
      if (addAsProject && firstNewProject) {
        useAppStore.getState().setActiveProject(firstNewProject);
        onClose();
        return;
      }
      setNewBranch('');
      setSelBranch('');
      setPathEdited(false);
      await load();
    } catch (e) {
      setCreateError(errMsg(e));
    } finally {
      setCreating(false);
    }
  }, [mode, selBranch, newBranch, baseBranch, targets, creating, addAsProject, onChanged, onClose, addProjectAt, load]);

  const handleOpenTerminal = useCallback((wt: WorktreeInfo) => {
    const targetProjectId = projectId ?? useAppStore.getState().activeProjectId;
    if (!targetProjectId) return;
    void newTerminal(targetProjectId, undefined, {
      cwd: wt.path,
      title: `⎇ ${wt.branch ?? wt.name}`,
    });
    onClose();
  }, [projectId, onClose]);

  const handleRemove = useCallback(async () => {
    if (!removeTarget || removing) return;
    setRemoving(true);
    setRemoveError(null);
    // 指向该目录的项目先关终端:Windows 下 shell 占着目录会让删除失败。
    // 项目本身留到 git 成功后再移除,失败时项目还在(终端呈断开态,可重开)。
    const project = findProjectByPath(removeTarget.wt.path);
    if (project) disposeProjectTerminals(project.id);
    try {
      await invoke('remove_worktree', {
        repoPath: removeTarget.mainPath,
        worktreePath: removeTarget.wt.path,
        force: removeForce,
      });
      // worktree 已删,指向它的项目一并移除,不留断链项目
      if (project) removeProjectWithCleanup(project.id);
      setRemoveTarget(null);
      onChanged();
      await load();
    } catch (e) {
      setRemoveError(errMsg(e));
    } finally {
      setRemoving(false);
    }
  }, [removeTarget, removeForce, removing, onChanged, load]);

  const handlePrune = useCallback(async (group: RepoGroup) => {
    if (pruningKey) return;
    setPruningKey(group.key);
    // prune 只清 git 侧的登记,指向失效 worktree 的项目也要一并移除,不留断链项目。
    // 以「目录确实已不存在」为准:isValid=false 但目录还在(元数据损坏)时项目保留。
    const invalidPaths = group.worktrees.filter((w) => !w.isValid).map((w) => w.path);
    try {
      await invoke('prune_worktrees', { repoPath: group.mainPath });
      if (invalidPaths.length > 0) {
        const existing = await invoke<string[]>('filter_directories', { paths: invalidPaths });
        const alive = new Set(existing);
        for (const path of invalidPaths) {
          if (alive.has(path)) continue;
          const project = findProjectByPath(path);
          if (project) removeProjectWithCleanup(project.id);
        }
      }
      onChanged();
      await load();
    } catch {
      // prune 失败无害:下次打开重试即可
    } finally {
      setPruningKey(null);
    }
  }, [pruningKey, onChanged, load]);

  const removeTargetProject = removeTarget ? findProjectByPath(removeTarget.wt.path) : undefined;
  const selectableCount = (groups ?? []).filter((g) => !g.error).length;

  const actionBtnCls =
    'shrink-0 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors';

  const renderWorktree = (wt: WorktreeInfo, group: RepoGroup) => {
    const isProject = projects.some(
      (p) => !p.sshConnectionId && normalizePath(p.path) === normalizePath(wt.path),
    );
    return (
      <div
        key={wt.path}
        className="group flex items-center gap-2 px-2 py-1.5 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]/60 transition-colors"
      >
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-1.5 min-w-0">
            <span className="text-sm font-medium text-[var(--text-primary)] truncate">{wt.name}</span>
            {wt.isMain && (
              <span className={badgeCls}>{t('worktree.mainRepo')}</span>
            )}
            {wt.branch && (
              <span className={badgeCls}>⎇ {wt.branch}</span>
            )}
            {!wt.isValid && (
              <span className="shrink-0 text-xs leading-[16px] px-1.5 rounded font-medium text-[var(--color-error)] bg-[var(--color-error)]/15">
                {t('worktree.invalid')}
              </span>
            )}
            {wt.isLocked && (
              <span className={badgeCls}>{t('worktree.locked')}</span>
            )}
            {isProject && (
              <span className="shrink-0 text-xs leading-[16px] px-1.5 rounded font-medium text-[var(--accent)] bg-[var(--accent-subtle)]">
                {t('worktree.isProject')}
              </span>
            )}
          </div>
          <div className="text-xs text-[var(--text-muted)] truncate" title={wt.path}>{wt.path}</div>
        </div>
        {wt.isValid && (
          <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
            <button className={actionBtnCls} onClick={() => handleOpenTerminal(wt)}>
              {t('worktree.openTerminal')}
            </button>
            <button
              className={actionBtnCls}
              onClick={() => {
                switchToProjectAt(wt.path, wt.name, group.mainPath);
                onClose();
              }}
            >
              {isProject ? t('worktree.switchToProject') : t('worktree.addAsProject')}
            </button>
            {!wt.isMain && (
              <button
                className="shrink-0 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--color-error)] hover:bg-[var(--color-error)]/10 transition-colors"
                onClick={() => {
                  setRemoveForce(false);
                  setRemoveError(null);
                  setRemoveTarget({ wt, mainPath: group.mainPath });
                }}
              >
                {t('worktree.remove')}
              </button>
            )}
          </div>
        )}
      </div>
    );
  };

  return (
    <Modal
      open={!!repoPath}
      onClose={onClose}
      title={t('worktree.title', { name: rootName })}
      panelClassName="w-[600px] max-h-[85vh]"
    >
      <div className="flex-1 min-h-0 overflow-y-auto p-4 select-none">
        {/* 工作区列表 */}
        {groups === null ? (
          <div className="text-sm text-[var(--text-muted)] py-4 text-center">{t('worktree.loading')}</div>
        ) : loadError ? (
          <div className="text-sm text-[var(--color-error)] py-2 break-all">{loadError}</div>
        ) : groups.length === 0 ? (
          <div className="text-sm text-[var(--text-muted)] py-4 text-center">{t('worktree.noRepoFound')}</div>
        ) : (
          <div className="space-y-2">
            {multiRepo && (
              <div className="flex items-center justify-between gap-2 pb-1">
                <span className="text-xs text-[var(--text-muted)]">
                  {t('worktree.reposFound', { count: groups.length })}
                </span>
                <button
                  className="text-xs px-2 py-0.5 rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
                  onClick={toggleAll}
                >
                  {selectedKeys.length >= selectableCount ? t('worktree.clearAll') : t('worktree.selectAll')}
                </button>
              </div>
            )}
            {groups.map((g) => {
              const selected = selectedKeys.includes(g.key);
              const hasInvalid = g.worktrees.some((w) => !w.isValid);
              return (
                <div key={g.key} className={multiRepo ? 'rounded-[var(--radius-sm)] border border-[var(--border-subtle)]' : ''}>
                  {multiRepo && (
                    <label
                      className={`flex items-center gap-2 px-2 py-1.5 cursor-pointer transition-colors ${
                        selected ? 'bg-[var(--accent-subtle)]' : 'hover:bg-[var(--border-subtle)]/60'
                      }`}
                    >
                      <input
                        type="checkbox"
                        checked={selected}
                        disabled={!!g.error}
                        onChange={() => toggleRepo(g.key)}
                        className="accent-[var(--accent)]"
                      />
                      <span className={`text-sm font-medium truncate ${selected ? 'text-[var(--accent)]' : 'text-[var(--text-primary)]'}`}>
                        {g.name}
                      </span>
                      <span className="text-xs text-[var(--text-muted)] truncate ml-auto" title={g.mainPath}>
                        {g.mainPath}
                      </span>
                    </label>
                  )}
                  {g.error ? (
                    <div className="px-2 py-1.5 text-xs text-[var(--color-error)] break-all">{g.error}</div>
                  ) : (
                    <div className={multiRepo ? 'px-1 pb-1 space-y-0.5' : 'space-y-0.5'}>
                      {g.worktrees.map((wt) => renderWorktree(wt, g))}
                    </div>
                  )}
                  {hasInvalid && (
                    <div className="flex justify-end px-1 pb-1">
                      <button
                        className="text-xs px-2 py-1 rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
                        disabled={pruningKey === g.key}
                        onClick={() => handlePrune(g)}
                      >
                        {pruningKey === g.key ? t('worktree.pruning') : t('worktree.prune')}
                      </button>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}

        {/* 新建 worktree */}
        {groups !== null && groups.length > 0 && (
          <div className="mt-4 pt-3 border-t border-[var(--border-subtle)]">
            <div className="flex items-center gap-3 mb-2.5">
              <span className="text-sm font-medium text-[var(--text-primary)]">{t('worktree.createTitle')}</span>
              {multiRepo && (
                <span className="text-xs text-[var(--text-muted)]">
                  {t('worktree.selectedCount', { count: selectedGroups.length })}
                </span>
              )}
              <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs ml-auto">
                {(['existing', 'new'] as const).map((m) => (
                  <button
                    key={m}
                    className={`px-2.5 py-1 transition-colors ${
                      mode === m
                        ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                        : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                    }`}
                    onClick={() => { setMode(m); setCreateError(null); setCreateResults(null); }}
                  >
                    {m === 'existing' ? t('worktree.modeExisting') : t('worktree.modeNew')}
                  </button>
                ))}
              </div>
            </div>

            <div className="space-y-2">
              {selectedGroups.length === 0 ? (
                <div className="text-xs text-[var(--text-muted)] py-1">{t('worktree.selectRepoHint')}</div>
              ) : mode === 'existing' ? (
                !branchesReady ? (
                  <div className="text-xs text-[var(--text-muted)] py-1">{t('worktree.loading')}</div>
                ) : availableBranches.length === 0 ? (
                  <div className="text-xs text-[var(--text-muted)] py-1">
                    {multiTarget ? t('worktree.noCommonBranch') : t('worktree.noBranchAvailable')}
                  </div>
                ) : (
                  <select
                    value={selBranch}
                    onChange={(e) => setSelBranch(e.target.value)}
                    className="w-full px-2.5 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-sm text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none font-mono"
                  >
                    <option value="">{t('worktree.selectBranch')}</option>
                    {availableBranches.map((name) => (
                      <option key={name} value={name}>{name}</option>
                    ))}
                  </select>
                )
              ) : (
                <div className="flex gap-2">
                  <input
                    value={newBranch}
                    onChange={(e) => setNewBranch(e.target.value)}
                    placeholder={t('worktree.newBranchPlaceholder')}
                    className="flex-1 min-w-0 px-2.5 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-sm text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none font-mono select-text"
                    spellCheck={false}
                  />
                  <select
                    value={baseBranch}
                    onChange={(e) => setBaseBranch(e.target.value)}
                    className="w-[180px] px-2.5 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-sm text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none font-mono"
                    title={t('worktree.baseBranchTitle')}
                  >
                    <option value="">{t('worktree.baseHead')}</option>
                    {baseBranchOptions.map((name) => (
                      <option key={name} value={name}>{name}</option>
                    ))}
                  </select>
                </div>
              )}

              {selectedGroups.length > 0 && (
                <>
                  <div className="flex gap-2">
                    <input
                      value={wtPath}
                      onChange={(e) => { setWtPath(e.target.value); setPathEdited(true); }}
                      placeholder={multiTarget ? t('worktree.parentPathPlaceholder') : t('worktree.pathPlaceholder')}
                      className="flex-1 min-w-0 px-2.5 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-sm text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none font-mono select-text"
                      spellCheck={false}
                    />
                    <button
                      className="shrink-0 px-2.5 py-1.5 text-sm rounded-[var(--radius-sm)] border border-[var(--border-default)] text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:border-[var(--accent)] transition-colors"
                      onClick={handleBrowse}
                    >
                      {t('worktree.browse')}
                    </button>
                  </div>

                  {/* 多仓库:逐个列出将要创建的目录,避免只看父目录心里没底 */}
                  {multiTarget && !!effectiveBranch.trim() && targets.length > 0 && (
                    <div className="text-xs text-[var(--text-muted)] font-mono space-y-0.5 pl-0.5">
                      {targets.map((target) => (
                        <div key={target.group.key} className="truncate" title={target.path}>{target.path}</div>
                      ))}
                    </div>
                  )}

                  <label className="flex items-center gap-1.5 text-xs text-[var(--text-secondary)] cursor-pointer w-fit">
                    <input
                      type="checkbox"
                      checked={addAsProject}
                      onChange={(e) => setAddAsProject(e.target.checked)}
                      className="accent-[var(--accent)]"
                    />
                    {t('worktree.addAsProjectAfterCreate')}
                  </label>
                </>
              )}

              {createError && (
                <div className="text-xs text-[var(--color-error)] break-all whitespace-pre-wrap">{createError}</div>
              )}
              {createResults && createResults.length > 0 && (
                <div className="text-xs text-[var(--color-error)] space-y-0.5 break-all whitespace-pre-wrap">
                  {createResults.map((r) => (
                    <div key={r.key}>{r.name}: {r.error}</div>
                  ))}
                </div>
              )}

              <div className="flex justify-end">
                <button
                  className="px-3 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-white hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
                  disabled={
                    creating
                    || targets.length === 0
                    || !(mode === 'existing' ? selBranch : newBranch.trim())
                  }
                  onClick={handleCreate}
                >
                  {creating
                    ? t('worktree.creating')
                    : multiTarget
                      ? t('worktree.createMulti', { count: selectedGroups.length })
                      : t('worktree.create')}
                </button>
              </div>
            </div>
          </div>
        )}
      </div>

      {/* 删除确认(嵌套弹窗;Esc 归栈顶,不会误关外层) */}
      <Modal
        open={!!removeTarget}
        onClose={() => { if (!removing) setRemoveTarget(null); }}
        align="center"
        ariaLabel={t('worktree.removeConfirmTitle')}
        panelClassName="w-[400px]"
        closeOnEscape={!removing}
      >
        <div className="p-5">
          <div className="text-sm font-medium text-[var(--text-primary)] mb-2">
            {t('worktree.removeConfirmTitle')}
          </div>
          <div className="text-xs text-[var(--text-secondary)] mb-2 break-all">
            {t('worktree.removeConfirmMessage', { name: removeTarget?.wt.name ?? '' })}
            <div className="text-[var(--text-muted)] mt-1 font-mono">{removeTarget?.wt.path}</div>
          </div>
          {removeTargetProject && (
            <div className="text-xs text-[var(--color-warning,#f59e0b)] mb-2">
              {t('worktree.removeAlsoProject', { name: removeTargetProject.name })}
            </div>
          )}
          <label className="flex items-center gap-1.5 text-xs text-[var(--text-secondary)] cursor-pointer w-fit mb-3">
            <input
              type="checkbox"
              checked={removeForce}
              onChange={(e) => setRemoveForce(e.target.checked)}
              className="accent-[var(--color-error)]"
            />
            {t('worktree.forceRemove')}
          </label>
          {removeError && (
            <div className="text-xs text-[var(--color-error)] break-all whitespace-pre-wrap mb-3">{removeError}</div>
          )}
          <div className="flex justify-end gap-2">
            <button
              className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
              disabled={removing}
              onClick={() => setRemoveTarget(null)}
            >
              {t('worktree.cancel')}
            </button>
            <button
              className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] bg-[var(--color-error)] text-white hover:opacity-90 transition-opacity disabled:opacity-50"
              disabled={removing}
              onClick={handleRemove}
            >
              {removing ? t('worktree.removing') : t('worktree.removeConfirm')}
            </button>
          </div>
        </div>
      </Modal>
    </Modal>
  );
}
