import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { ask, message } from '@tauri-apps/plugin-dialog';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useAppStore, isExpanded, toggleExpandedDir } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { showContextMenu } from '../utils/contextMenu';
import { showPrompt } from '../utils/prompt';
import { isAiPty } from '../utils/terminalCache';
import { MOD_LABEL } from '../utils/platform';
import { DiffModal } from './DiffModal';
import { FileViewerModal } from './FileViewerModal';
import { initFileDrag } from '../utils/fileDragState';
import { getFileTreeCache, setFileTreeCache } from '../utils/projectDataCache';
import { useT } from '../i18n';
import type { FileEntry, FsChangePayload, GitFileStatus, PtyOutputPayload } from '../types';

interface TreeNodeProps {
  entry: FileEntry;
  projectRoot: string;
  depth: number;
  gitStatusMap: Map<string, GitFileStatus>;
  onViewDiff: (status: GitFileStatus) => void;
  onViewFile: (path: string) => void;
}

function getRelativePath(targetPath: string, rootPath: string) {
  const normalize = (value: string) => value.replace(/[\\/]+/g, '/').replace(/\/$/, '');
  const normalizedRoot = normalize(rootPath);
  const normalizedTarget = normalize(targetPath);
  const sep = rootPath.includes('\\') ? '\\' : '/';

  if (normalizedTarget === normalizedRoot) return '.';
  if (!normalizedTarget.startsWith(`${normalizedRoot}/`)) return targetPath;

  return normalizedTarget.slice(normalizedRoot.length + 1).replace(/\//g, sep);
}

function TreeNode({ entry, projectRoot, depth, gitStatusMap, onViewDiff, onViewFile }: TreeNodeProps) {
  const t = useT();
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const [expanded, setExpanded] = useState(() =>
    activeProjectId ? isExpanded(activeProjectId, entry.path) : false
  );
  const [children, setChildren] = useState<FileEntry[]>([]);

  const loadChildren = useCallback(async () => {
    const entries = await invoke<FileEntry[]>('list_directory', {
      projectRoot,
      path: entry.path,
    });
    setChildren(entries);
  }, [entry.path, projectRoot]);

  // 恢复时(初始即展开)加载一次子节点
  useEffect(() => {
    if (expanded && entry.isDir) {
      loadChildren();
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps -- 仅在 mount 时按持久化展开态恢复一次

  // 目录监听生命周期:展开时注册 watcher,折叠 / 组件卸载 / 路径变化时自动注销。
  // 旧实现只在手动折叠当前节点时 unwatch,父级折叠或切换项目导致后代节点直接 unmount
  // 时其 watcher 永不释放,会持续累积 OS 文件监听句柄(inotify / ReadDirectoryChangesW)。
  useEffect(() => {
    if (!entry.isDir || !expanded) return;
    invoke('watch_directory', { path: entry.path, projectPath: projectRoot }).catch(() => {});
    return () => {
      invoke('unwatch_directory', { path: entry.path }).catch(() => {});
    };
  }, [expanded, entry.isDir, entry.path, projectRoot]);

  const handleToggle = useCallback(async () => {
    if (!entry.isDir) {
      onViewFile(entry.path);
      return;
    }
    const next = !expanded;
    // 展开前先加载子节点避免空帧;watcher 的注册/注销由上面的监听生命周期 effect
    // 跟随 expanded 状态自动处理(含 unmount 时的释放),此处不再手动 watch/unwatch。
    if (next) {
      await loadChildren();
    }
    setExpanded(next);
    if (activeProjectId) {
      toggleExpandedDir(activeProjectId, entry.path, next);
    }
  }, [entry, expanded, loadChildren, onViewFile, activeProjectId]);

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (expanded && payload.path.startsWith(entry.path)) {
      loadChildren();
    }
  }, [expanded, entry.path, loadChildren]));

  return (
    <div>
      <div
        data-file-item
        className={`flex items-center gap-1 py-[3px] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-base transition-colors duration-100 ${
          entry.ignored ? 'text-[var(--text-muted)] opacity-50' : entry.isDir ? 'text-[var(--color-folder)]' : 'text-[var(--color-file)]'
        }`}
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        onClick={handleToggle}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const relativePath = getRelativePath(entry.path, projectRoot);
          const items: Parameters<typeof showContextMenu>[2] = [
            {
              label: t('fileTree.menu.copyRelativePath'),
              onClick: () => writeText(relativePath),
            },
            {
              label: t('fileTree.menu.copyAbsolutePath'),
              onClick: () => writeText(entry.path),
            },
            { separator: true },
            {
              label: t('fileTree.menu.revealInFolder'),
              onClick: () => revealItemInDir(entry.path),
            },
          ];
          if (!entry.isDir) {
            items.unshift({
              label: t('fileTree.menu.openWithDefault'),
              onClick: () => invoke('open_path_with_default_app', { path: entry.path }),
            });
          }
          items.push({ separator: true });
          items.push({
            label: t('fileTree.menu.rename'),
            onClick: async () => {
              const newName = await showPrompt(t('fileTree.prompt.renameTitle'), t('fileTree.prompt.renameMessage'), entry.name);
              if (!newName?.trim() || newName.trim() === entry.name) return;
              try {
                await invoke('rename_entry', { projectRoot, oldPath: entry.path, newName: newName.trim() });
                loadChildren();
              } catch (err) {
                console.error('重命名失败:', err);
                await message(t('fileTree.dialog.renameFailedMessage', { error: String(err) }), { title: t('fileTree.dialog.renameFailedTitle'), kind: 'error' });
              }
            },
          });
          items.push({
            label: t('fileTree.menu.delete'),
            onClick: async () => {
              const confirmed = await ask(
                entry.isDir
                  ? t('fileTree.dialog.deleteConfirmFolder', { name: entry.name })
                  : t('fileTree.dialog.deleteConfirmFile', { name: entry.name }),
                {
                  title: entry.isDir ? t('fileTree.dialog.deleteFolderTitle') : t('fileTree.dialog.deleteFileTitle'),
                  kind: 'warning',
                  okLabel: t('fileTree.dialog.deleteOk'),
                  cancelLabel: t('fileTree.dialog.deleteCancel'),
                },
              );
              if (!confirmed) return;
              try {
                await invoke('delete_entry', { projectRoot, path: entry.path });
              } catch (err) {
                console.error('删除失败:', err);
                await message(t('fileTree.dialog.deleteFailedMessage', { error: String(err) }), { title: t('fileTree.dialog.deleteFailedTitle'), kind: 'error' });
              }
            },
          });
          if (entry.isDir) {
            items.push({ separator: true });
            items.push({
              label: t('fileTree.menu.newFile'),
              onClick: async () => {
                const name = await showPrompt(t('fileTree.prompt.newFileTitle'), t('fileTree.prompt.newFileMessage'));
                if (!name?.trim()) return;
                const sep = entry.path.includes('/') ? '/' : '\\';
                await invoke('create_file', { projectRoot, path: `${entry.path}${sep}${name.trim()}` });
                if (!expanded) handleToggle();
                else loadChildren();
              },
            });
            items.push({
              label: t('fileTree.menu.newFolder'),
              onClick: async () => {
                const name = await showPrompt(t('fileTree.prompt.newFolderTitle'), t('fileTree.prompt.newFolderMessage'));
                if (!name?.trim()) return;
                const sep = entry.path.includes('/') ? '/' : '\\';
                await invoke('create_directory', { projectRoot, path: `${entry.path}${sep}${name.trim()}` });
                if (!expanded) handleToggle();
                else loadChildren();
              },
            });
          }
          // 查看变更菜单项
          const relForGit = getRelativePath(entry.path, projectRoot).replace(/\\/g, '/');
          const entryGitStatus = gitStatusMap.get(relForGit);
          if (entryGitStatus && !entry.isDir) {
            items.push({ separator: true });
            items.push({
              label: t('fileTree.menu.viewDiff'),
              onClick: () => onViewDiff(entryGitStatus),
            });
          }
          showContextMenu(e.clientX, e.clientY, items);
        }}
        onMouseDown={(e) => {
          if (e.button === 0) initFileDrag(entry.path, e.clientX, e.clientY);
        }}
      >
        {entry.isDir && (
          <span className="text-[13px] w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
            style={{ transform: expanded ? 'rotate(0deg)' : 'rotate(-90deg)', display: 'inline-block' }}>
            ▾
          </span>
        )}
        {!entry.isDir && <span className="w-3 text-center text-[var(--text-muted)] text-xs">·</span>}
        <span className="truncate" title={entry.name}>{entry.name}</span>
        {(() => {
          const rel = getRelativePath(entry.path, projectRoot).replace(/\\/g, '/');
          const fileStatus = gitStatusMap.get(rel);
          const GIT_COLORS: Record<string, string> = {
            M: 'text-[var(--color-warning)]',
            A: 'text-[var(--color-success)]',
            D: 'text-[var(--color-error)]',
            R: 'text-[var(--color-info)]',
            '?': 'text-[var(--color-success)]',
            C: 'text-[var(--color-error)]',
          };
          if (fileStatus) {
            return (
              <span className={`ml-1.5 text-xs font-bold flex-shrink-0 ${GIT_COLORS[fileStatus.statusLabel] ?? 'text-[var(--text-muted)]'}`}>
                {fileStatus.statusLabel}
              </span>
            );
          }
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
                <span className={`ml-1.5 text-xs font-bold flex-shrink-0 opacity-70 ${GIT_COLORS[bestLabel] ?? 'text-[var(--text-muted)]'}`}>
                  {bestLabel}
                </span>
              );
            }
          }
          return null;
        })()}
      </div>

      {expanded &&
        children.map((child) => (
          <TreeNode
            key={child.path}
            entry={child}
            projectRoot={projectRoot}
            depth={depth + 1}
            gitStatusMap={gitStatusMap}
            onViewDiff={onViewDiff}
            onViewFile={onViewFile}
          />
        ))}
    </div>
  );
}

export function FileTree() {
  const t = useT();
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const config = useAppStore((s) => s.config);
  const setSearchModalOpen = useAppStore((s) => s.setSearchModalOpen);
  const project = config.projects.find((p) => p.id === activeProjectId);

  const handleOpenInEditor = useCallback(async (editorName?: string) => {
    if (!project) return;
    if (!config.editors.length) {
      await message(
        t('fileTree.dialog.noEditorMessage'),
        { title: t('fileTree.dialog.noEditorTitle'), kind: 'warning' },
      );
      return;
    }
    try {
      await invoke('open_in_editor', {
        path: project.path,
        editorName: editorName ?? null,
      });
    } catch (err) {
      const detail = typeof err === 'string' ? err : String(err);
      console.error('打开编辑器失败:', err);
      await message(detail, { title: t('fileTree.dialog.openEditorFailedTitle'), kind: 'error' });
    }
  }, [project, config.editors, t]);

  const handleSwitchAndOpen = useCallback((editorName: string) => {
    const newConfig = { ...config, defaultEditor: editorName };
    useAppStore.getState().setConfig(newConfig);
    invoke('save_config', { config: newConfig });
    handleOpenInEditor(editorName);
  }, [config, handleOpenInEditor]);

  const [rootEntries, setRootEntries] = useState<FileEntry[]>(() => {
    return (project ? getFileTreeCache(project.path) : undefined)?.rootEntries ?? [];
  });
  const [gitStatusMap, setGitStatusMap] = useState<Map<string, GitFileStatus>>(() => {
    return (project ? getFileTreeCache(project.path) : undefined)?.gitStatusMap ?? new Map();
  });
  const [loading, setLoading] = useState(() => !project || !getFileTreeCache(project.path));
  const [loadError, setLoadError] = useState<string | null>(null);
  const [diffTarget, setDiffTarget] = useState<GitFileStatus | null>(null);
  const [viewFilePath, setViewFilePath] = useState<string | null>(null);
  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const rootEntriesRef = useRef(rootEntries);
  rootEntriesRef.current = rootEntries;
  const gitStatusMapRef = useRef(gitStatusMap);
  gitStatusMapRef.current = gitStatusMap;

  const loadGitStatus = useCallback(() => {
    if (!project) return;
    invoke<GitFileStatus[]>('get_git_status', { projectPath: project.path })
      .then((statuses) => {
        const map = new Map<string, GitFileStatus>();
        for (const s of statuses) map.set(s.path, s);
        setGitStatusMap(map);
        gitStatusMapRef.current = map;
        setFileTreeCache(project.path, {
          rootEntries: rootEntriesRef.current,
          gitStatusMap: map,
        });
      })
      .catch(() => setGitStatusMap(new Map()));
  }, [project?.path]);

  const debouncedRefresh = useCallback(() => {
    if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
    refreshTimerRef.current = setTimeout(loadGitStatus, 500);
  }, [loadGitStatus]);

  const loadRootEntries = useCallback(() => {
    if (!project) return;
    const projectPath = project.path;
    setLoadError(null);
    if (rootEntriesRef.current.length === 0) setLoading(true);
    invoke<FileEntry[]>('list_directory', {
      projectRoot: projectPath,
      path: projectPath,
    }).then((entries) => {
      setRootEntries(entries);
      rootEntriesRef.current = entries;
      setLoading(false);
      setLoadError(null);
      setFileTreeCache(project.path, {
        rootEntries: entries,
        gitStatusMap: gitStatusMapRef.current,
      });
    }).catch((err) => {
      setLoading(false);
      setLoadError(typeof err === 'string' ? err : String(err));
    });
  }, [project?.path]);

  useEffect(() => {
    if (!project) {
      setRootEntries([]);
      setGitStatusMap(new Map());
      setLoading(false);
      setLoadError(null);
      return;
    }
    let cancelled = false;
    const projectPath = project.path;
    const cached = getFileTreeCache(projectPath);
    if (cached) {
      setRootEntries(cached.rootEntries);
      rootEntriesRef.current = cached.rootEntries;
      setGitStatusMap(cached.gitStatusMap);
      gitStatusMapRef.current = cached.gitStatusMap;
    } else {
      setRootEntries([]);
      setGitStatusMap(new Map());
    }
    setLoading(!cached);
    setLoadError(null);
    setDiffTarget(null);
    setViewFilePath(null);
    const listPromise = invoke<FileEntry[]>('list_directory', { projectRoot: projectPath, path: projectPath });
    const statusPromise = invoke<GitFileStatus[]>('get_git_status', { projectPath }).catch(() => [] as GitFileStatus[]);
    Promise.all([listPromise, statusPromise]).then(([entries, statuses]) => {
      if (cancelled) return;
      const map = new Map<string, GitFileStatus>();
      for (const s of statuses) map.set(s.path, s);
      setRootEntries(entries);
      rootEntriesRef.current = entries;
      setGitStatusMap(map);
      gitStatusMapRef.current = map;
      setLoading(false);
      setFileTreeCache(projectPath, { rootEntries: entries, gitStatusMap: map });
    }).catch((err) => {
      if (cancelled) return;
      setLoading(false);
      setLoadError(typeof err === 'string' ? err : String(err));
    });
    invoke('watch_directory', { path: projectPath, projectPath });
    return () => {
      cancelled = true;
      invoke('unwatch_directory', { path: projectPath });
    };
  }, [project?.path]);

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (!project) return;
    // notify 在 NonRecursive watcher 上 emit 的 payload.path 是发生变化的文件,
    // 而不是被 watch 的目录本身。原条件 `payload.path === project.path` 永远不匹配,
    // 导致根目录下重命名/新建/删除后文件列表不刷新。
    // 改为「变化发生在项目根目录的直接子级」时刷新根列表;子目录变化由各 TreeNode 自己处理。
    const normalize = (p: string) => p.replace(/[\\/]+/g, '/').replace(/\/+$/, '');
    const changed = normalize(payload.path);
    const root = normalize(project.path);
    if (!changed.startsWith(root + '/')) return;
    const rest = changed.slice(root.length + 1);
    if (!rest.includes('/')) {
      loadRootEntries();
    }
  }, [project?.path, loadRootEntries]));

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (project && payload.projectPath === project.path) {
      debouncedRefresh();
    }
  }, [project?.path, debouncedRefresh]));

  const GIT_PATTERNS = [/create mode/, /Switched to/, /Already up to date/, /insertions?\(\+\)/, /deletions?\(-\)/];
  useTauriEvent<PtyOutputPayload>('pty-output', useCallback((payload: PtyOutputPayload) => {
    if (isAiPty(payload.ptyId)) return;
    if (GIT_PATTERNS.some((p) => p.test(payload.data))) {
      debouncedRefresh();
    }
  }, [debouncedRefresh]));

  const handleViewDiff = useCallback((status: GitFileStatus) => {
    setDiffTarget(status);
  }, []);

  const handleViewFile = useCallback((path: string) => {
    setViewFilePath(path);
  }, []);

  const handleRootContextMenu = useCallback((e: React.MouseEvent) => {
    if (!project) return;
    e.preventDefault();
    const sep = project.path.includes('/') ? '/' : '\\';
    showContextMenu(e.clientX, e.clientY, [
      {
        label: t('fileTree.menu.newFile'),
        onClick: async () => {
          const name = await showPrompt(t('fileTree.prompt.newFileTitle'), t('fileTree.prompt.newFileMessage'));
          if (!name?.trim()) return;
          await invoke('create_file', { projectRoot: project.path, path: `${project.path}${sep}${name.trim()}` });
          loadRootEntries();
        },
      },
      {
        label: t('fileTree.menu.newFolder'),
        onClick: async () => {
          const name = await showPrompt(t('fileTree.prompt.newFolderTitle'), t('fileTree.prompt.newFolderMessage'));
          if (!name?.trim()) return;
          await invoke('create_directory', { projectRoot: project.path, path: `${project.path}${sep}${name.trim()}` });
          loadRootEntries();
        },
      },
    ]);
  }, [project, loadRootEntries, t]);

  if (!project) {
    return (
      <div className="h-full bg-[var(--bg-surface)] flex items-center justify-center text-[var(--text-muted)] text-base">
        {t('fileTree.empty.selectProject')}
      </div>
    );
  }

  return (
    <div data-panel className="h-full bg-[var(--bg-surface)] flex flex-col border-l border-[var(--border-subtle)] select-none">
      <div data-panel-header className="px-3 pt-3 pb-1.5 flex items-center justify-between gap-2 flex-shrink-0">
        <span className="text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium truncate">
          Files — {project.name}
        </span>
        <div className="flex items-center flex-shrink-0 gap-1">
          <button
            type="button"
            onClick={() => setSearchModalOpen(true)}
            title={t('fileTree.header.searchTitle', { mod: MOD_LABEL })}
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-sm leading-none px-1.5 py-0.5 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
          >
            ⌕
          </button>
          <button
            type="button"
            onClick={() => {
              loadRootEntries();
              loadGitStatus();
            }}
            title={t('fileTree.header.refresh')}
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-sm leading-none px-1.5 py-0.5 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
          >
            ↻
          </button>
          {config.editors.length > 0 && (
            <div className="flex items-center">
              <button
                type="button"
                onClick={() => handleOpenInEditor()}
                title={t('fileTree.header.openWithEditor', { editor: config.editors.find((e) => e.name === config.defaultEditor)?.name ?? config.editors[0]?.name ?? t('fileTree.header.editorFallback') })}
                className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-xs leading-none px-1.5 py-0.5 rounded-l-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
              >
                {config.editors.find((e) => e.name === config.defaultEditor)?.name ?? config.editors[0]?.name}
              </button>
              {config.editors.length > 1 && (
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    const rect = e.currentTarget.getBoundingClientRect();
                    showContextMenu(rect.left, rect.bottom + 4, config.editors.map((editor) => ({
                      label: editor.name + (editor.name === (config.defaultEditor ?? config.editors[0]?.name) ? ' (*)' : ''),
                      onClick: () => handleSwitchAndOpen(editor.name),
                    })));
                  }}
                  title={t('fileTree.menu.chooseOtherEditor')}
                  className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-xs leading-none pl-0.5 pr-1 py-0.5 rounded-r-[var(--radius-sm)] hover:bg-[var(--border-subtle)] border-l border-[var(--border-subtle)]"
                >
                  <svg width="8" height="8" viewBox="0 0 8 8" fill="currentColor" aria-hidden="true">
                    <path d="M1.5 3L4 5.5L6.5 3" stroke="currentColor" strokeWidth="1.2" fill="none" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                </button>
              )}
            </div>
          )}
        </div>
      </div>
      <div className="flex-1 min-h-0 overflow-y-auto px-1" onContextMenu={handleRootContextMenu}>
        {loading && rootEntries.length === 0 ? (
          <div className="flex items-center justify-center py-8 text-[var(--text-muted)] text-sm">
            {t('fileTree.empty.loading')}
          </div>
        ) : loadError && rootEntries.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-2 py-8 px-3 text-center text-sm">
            <div className="text-[var(--text-muted)] truncate max-w-full" title={loadError}>
              {t('fileTree.empty.loadFailed')}
            </div>
            <button
              type="button"
              onClick={loadRootEntries}
              className="px-2 py-1 rounded-[var(--radius-sm)] text-[var(--accent)] hover:bg-[var(--border-subtle)] transition-colors"
            >
              {t('fileTree.empty.retry')}
            </button>
          </div>
        ) : (
          <>
            {loadError && (
              <div className="px-2 py-1 text-xs text-[var(--text-muted)] truncate" title={loadError}>
                {t('fileTree.empty.refreshFailed')}
              </div>
            )}
            {rootEntries.map((entry) => (
              <TreeNode
                key={entry.path}
                entry={entry}
                projectRoot={project.path}
                depth={0}
                gitStatusMap={gitStatusMap}
                onViewDiff={handleViewDiff}
                onViewFile={handleViewFile}
              />
            ))}
          </>
        )}
      </div>
      {viewFilePath && project && (
        <FileViewerModal
          open={!!viewFilePath}
          onClose={() => setViewFilePath(null)}
          filePath={viewFilePath}
          projectRoot={project.path}
        />
      )}
      {diffTarget && (
        <DiffModal
          open={!!diffTarget}
          onClose={() => setDiffTarget(null)}
          projectPath={project.path}
          status={diffTarget}
        />
      )}
    </div>
  );
}
