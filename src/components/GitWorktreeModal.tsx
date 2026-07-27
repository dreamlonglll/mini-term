import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { Modal } from './Modal';
import { useAppStore, genId } from '../store';
import { newTerminal } from '../utils/paneActions';
import {
  disposeProjectTerminals,
  findProjectByPath,
  normalizePath,
  removeProjectWithCleanup,
} from '../utils/projectActions';
import { useT } from '../i18n';
import type { BranchInfo, WorktreeInfo } from '../types';

interface Props {
  /** 目标仓库路径;null = 弹窗关闭 */
  repoPath: string | null;
  onClose: () => void;
  /** worktree 集合变化(新建/删除/清理)后通知外层刷新仓库列表 */
  onChanged: () => void;
  /** 「开终端」的目标项目;缺省用当前激活项目(从项目右键菜单打开时应传右键的那个项目) */
  projectId?: string;
}

/** 分支名 → 可用作目录名的片段(worktree 默认路径建议用) */
function sanitizeBranchForDir(branch: string): string {
  return branch.replace(/[\\/:*?"<>|\s]+/g, '-').replace(/^-+|-+$/g, '') || 'worktree';
}

const badgeCls =
  'shrink-0 text-xs leading-[16px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]';

/**
 * Worktree 管理弹窗:列出主工作区 + 全部 linked worktree,支持新建(现有分支 /
 * 新建分支)、删除(可强制)、清理失效条目、在终端打开、一键添加为项目。
 */
export function GitWorktreeModal({ repoPath, onClose, onChanged, projectId }: Props) {
  const t = useT();
  // 订阅 projects:worktree 行的「已是项目」标识要跟着增删项目即时变化
  const projects = useAppStore((s) => s.config.projects);

  const [worktrees, setWorktrees] = useState<WorktreeInfo[] | null>(null);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
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

  // 删除确认
  const [removeTarget, setRemoveTarget] = useState<WorktreeInfo | null>(null);
  const [removeForce, setRemoveForce] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);

  const [pruning, setPruning] = useState(false);

  const load = useCallback(async () => {
    if (!repoPath) return;
    setLoadError(null);
    try {
      const [wts, brs] = await Promise.all([
        invoke<WorktreeInfo[]>('list_worktrees', { repoPath }),
        invoke<BranchInfo[]>('get_repo_branches', { repoPath }),
      ]);
      setWorktrees(wts);
      setBranches(brs);
    } catch (e) {
      setWorktrees([]);
      setLoadError(e instanceof Error ? e.message : String(e));
    }
  }, [repoPath]);

  // 打开时重置表单并加载
  useEffect(() => {
    if (!repoPath) return;
    setWorktrees(null);
    setBranches([]);
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
    setRemoveTarget(null);
    load();
  }, [repoPath, load]);

  // 已被某个工作区检出的分支不能再次检出
  const checkedOutBranches = useMemo(
    () => new Set((worktrees ?? []).filter((w) => w.branch).map((w) => w.branch!)),
    [worktrees],
  );
  const availableBranches = useMemo(
    () => branches.filter((b) => !b.isRemote && !checkedOutBranches.has(b.name)),
    [branches, checkedOutBranches],
  );

  // worktree 命令统一对主仓库执行:从 worktree 条目打开弹窗再删除它自己时,
  // 若以该 worktree 为 cwd 跑 `git worktree remove`,git 会拒绝「删除当前所在工作区」
  const mainRepoPath = useMemo(
    () => worktrees?.find((w) => w.isMain)?.path ?? repoPath,
    [worktrees, repoPath],
  );

  const sep = repoPath?.includes('\\') ? '\\' : '/';
  const repoName = useMemo(
    () => repoPath?.split(/[\\/]/).filter(Boolean).pop() ?? '',
    [repoPath],
  );

  // 默认路径建议:仓库同级的 `<仓库名>-<分支>`;用户手动改过就不再跟随
  const effectiveBranch = mode === 'existing' ? selBranch : newBranch;
  useEffect(() => {
    if (!repoPath || pathEdited) return;
    const parent = repoPath.slice(0, Math.max(0, repoPath.length - repoName.length - 1));
    if (!parent) return;
    setWtPath(`${parent}${sep}${repoName}-${sanitizeBranchForDir(effectiveBranch)}`);
  }, [repoPath, repoName, sep, effectiveBranch, pathEdited]);

  const handleBrowse = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    // 选中的是父目录:worktree 目录本身必须是新路径,自动拼上分支名子目录
    setWtPath(`${selected as string}${sep}${repoName}-${sanitizeBranchForDir(effectiveBranch)}`);
    setPathEdited(true);
  }, [sep, repoName, effectiveBranch]);

  const switchToProjectAt = useCallback((path: string, fallbackName: string) => {
    const { addProject, setActiveProject, activeProjectId, config } = useAppStore.getState();
    const existing = findProjectByPath(path);
    if (existing) {
      setActiveProject(existing.id);
      return;
    }
    // 挂为子项目:主仓库对应的项目优先;主仓库不是项目时回落到当前激活项目
    // (Worktree 面板就是从它打开的)。都没有则成为普通顶层项目。
    const parent =
      (mainRepoPath ? findProjectByPath(mainRepoPath) : undefined)
      ?? config.projects.find((p) => p.id === activeProjectId);
    const id = genId();
    const name = path.split(/[\\/]/).filter(Boolean).pop() || fallbackName;
    addProject({ id, name, path }, parent?.id);
    invoke('save_config', { config: useAppStore.getState().config });
    setActiveProject(id);
  }, [mainRepoPath]);

  const handleCreate = useCallback(async () => {
    const branch = (mode === 'existing' ? selBranch : newBranch).trim();
    const path = wtPath.trim();
    if (!mainRepoPath || !branch || !path || creating) return;
    setCreating(true);
    setCreateError(null);
    try {
      await invoke('add_worktree', {
        repoPath: mainRepoPath,
        worktreePath: path,
        branch,
        createBranch: mode === 'new',
        base: mode === 'new' && baseBranch ? baseBranch : null,
      });
      onChanged();
      if (addAsProject) {
        switchToProjectAt(path, branch);
        onClose();
      } else {
        setNewBranch('');
        setSelBranch('');
        setPathEdited(false);
        await load();
      }
    } catch (e) {
      setCreateError(e instanceof Error ? e.message : String(e));
    } finally {
      setCreating(false);
    }
  }, [mainRepoPath, mode, selBranch, newBranch, baseBranch, wtPath, creating, addAsProject, onChanged, onClose, switchToProjectAt, load]);

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
    if (!mainRepoPath || !removeTarget || removing) return;
    setRemoving(true);
    setRemoveError(null);
    // 指向该目录的项目先关终端:Windows 下 shell 占着目录会让删除失败。
    // 项目本身留到 git 成功后再移除,失败时项目还在(终端呈断开态,可重开)。
    const project = findProjectByPath(removeTarget.path);
    if (project) disposeProjectTerminals(project.id);
    try {
      await invoke('remove_worktree', {
        repoPath: mainRepoPath,
        worktreePath: removeTarget.path,
        force: removeForce,
      });
      // worktree 已删,指向它的项目一并移除,不留断链项目
      if (project) removeProjectWithCleanup(project.id);
      setRemoveTarget(null);
      onChanged();
      await load();
    } catch (e) {
      setRemoveError(e instanceof Error ? e.message : String(e));
    } finally {
      setRemoving(false);
    }
  }, [mainRepoPath, removeTarget, removeForce, removing, onChanged, load]);

  const handlePrune = useCallback(async () => {
    if (!mainRepoPath || pruning) return;
    setPruning(true);
    try {
      await invoke('prune_worktrees', { repoPath: mainRepoPath });
      onChanged();
      await load();
    } catch {
      // prune 失败无害:下次打开重试即可
    } finally {
      setPruning(false);
    }
  }, [mainRepoPath, pruning, onChanged, load]);

  const hasInvalid = (worktrees ?? []).some((w) => !w.isValid);
  const removeTargetProject = removeTarget ? findProjectByPath(removeTarget.path) : undefined;

  const actionBtnCls =
    'shrink-0 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors';

  return (
    <Modal
      open={!!repoPath}
      onClose={onClose}
      title={t('worktree.title', { name: repoName })}
      panelClassName="w-[600px] max-h-[85vh]"
    >
      <div className="flex-1 min-h-0 overflow-y-auto p-4 select-none">
        {/* 工作区列表 */}
        {worktrees === null ? (
          <div className="text-sm text-[var(--text-muted)] py-4 text-center">{t('worktree.loading')}</div>
        ) : loadError ? (
          <div className="text-sm text-[var(--color-error)] py-2 break-all">{loadError}</div>
        ) : (
          <div className="space-y-0.5">
            {worktrees.map((wt) => {
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
                          switchToProjectAt(wt.path, wt.name);
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
                            setRemoveTarget(wt);
                          }}
                        >
                          {t('worktree.remove')}
                        </button>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
            {hasInvalid && (
              <div className="flex justify-end pt-1">
                <button
                  className="text-xs px-2 py-1 rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
                  disabled={pruning}
                  onClick={handlePrune}
                >
                  {pruning ? t('worktree.pruning') : t('worktree.prune')}
                </button>
              </div>
            )}
          </div>
        )}

        {/* 新建 worktree */}
        <div className="mt-4 pt-3 border-t border-[var(--border-subtle)]">
          <div className="flex items-center gap-3 mb-2.5">
            <span className="text-sm font-medium text-[var(--text-primary)]">{t('worktree.createTitle')}</span>
            <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
              {(['existing', 'new'] as const).map((m) => (
                <button
                  key={m}
                  className={`px-2.5 py-1 transition-colors ${
                    mode === m
                      ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                      : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                  }`}
                  onClick={() => { setMode(m); setCreateError(null); }}
                >
                  {m === 'existing' ? t('worktree.modeExisting') : t('worktree.modeNew')}
                </button>
              ))}
            </div>
          </div>

          <div className="space-y-2">
            {mode === 'existing' ? (
              availableBranches.length === 0 ? (
                <div className="text-xs text-[var(--text-muted)] py-1">{t('worktree.noBranchAvailable')}</div>
              ) : (
                <select
                  value={selBranch}
                  onChange={(e) => setSelBranch(e.target.value)}
                  className="w-full px-2.5 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-sm text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none font-mono"
                >
                  <option value="">{t('worktree.selectBranch')}</option>
                  {availableBranches.map((b) => (
                    <option key={b.name} value={b.name}>{b.name}</option>
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
                  {branches.map((b) => (
                    <option key={b.name} value={b.name}>{b.name}</option>
                  ))}
                </select>
              </div>
            )}

            <div className="flex gap-2">
              <input
                value={wtPath}
                onChange={(e) => { setWtPath(e.target.value); setPathEdited(true); }}
                placeholder={t('worktree.pathPlaceholder')}
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

            <label className="flex items-center gap-1.5 text-xs text-[var(--text-secondary)] cursor-pointer w-fit">
              <input
                type="checkbox"
                checked={addAsProject}
                onChange={(e) => setAddAsProject(e.target.checked)}
                className="accent-[var(--accent)]"
              />
              {t('worktree.addAsProjectAfterCreate')}
            </label>

            {createError && (
              <div className="text-xs text-[var(--color-error)] break-all whitespace-pre-wrap">{createError}</div>
            )}

            <div className="flex justify-end">
              <button
                className="px-3 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-white hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
                disabled={creating || !wtPath.trim() || !(mode === 'existing' ? selBranch : newBranch.trim())}
                onClick={handleCreate}
              >
                {creating ? t('worktree.creating') : t('worktree.create')}
              </button>
            </div>
          </div>
        </div>
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
            {t('worktree.removeConfirmMessage', { name: removeTarget?.name ?? '' })}
            <div className="text-[var(--text-muted)] mt-1 font-mono">{removeTarget?.path}</div>
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
