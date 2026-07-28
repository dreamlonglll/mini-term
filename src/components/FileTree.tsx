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
import { getFileTreeCache, setFileTreeCache, projectCacheKey } from '../utils/projectDataCache';
import { useT } from '../i18n';
import type { FileEntry, FsChangePayload, GitFileStatus, PtyOutputPayload } from '../types';

interface TreeNodeProps {
  entry: FileEntry;
  projectRoot: string;
  depth: number;
  gitStatusMap: Map<string, GitFileStatus>;
  onViewDiff: (status: GitFileStatus) => void;
  onViewFile: (path: string) => void;
  /** SSH 远程项目:有值 = 子目录懒加载走 ssh_remote_list_directory,
   *  不注册 notify watcher,右键菜单仅保留复制相对/绝对路径(POSIX)。 */
  remoteConnectionId?: string;
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

function TreeNode({ entry, projectRoot, depth, gitStatusMap, onViewDiff, onViewFile, remoteConnectionId }: TreeNodeProps) {
  const t = useT();
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const [expanded, setExpanded] = useState(() =>
    activeProjectId ? isExpanded(activeProjectId, entry.path) : false
  );
  const [children, setChildren] = useState<FileEntry[]>([]);
  // 仅远程展开时置 true:SFTP 往返可达秒级,需要行内 spinner 反馈;
  // 本地列目录近乎即时,置 loading 反而闪一帧
  const [loadingChildren, setLoadingChildren] = useState(false);

  const loadChildren = useCallback(async () => {
    // 远程目录:SFTP readdir(每次展开重新拉取,不做 notify 监听);
    // 失败(断线/超时)静默保持旧列表,不清空已展示内容
    if (remoteConnectionId) {
      try {
        const entries = await invoke<FileEntry[]>('ssh_remote_list_directory', {
          connectionId: remoteConnectionId,
          path: entry.path,
          projectRoot,
        });
        setChildren(entries);
      } catch {
        // 保持旧列表
      }
      return;
    }
    const entries = await invoke<FileEntry[]>('list_directory', {
      projectRoot,
      path: entry.path,
    });
    setChildren(entries);
  }, [entry.path, projectRoot, remoteConnectionId]);

  // 恢复时(初始即展开)加载一次子节点
  useEffect(() => {
    if (expanded && entry.isDir) {
      if (remoteConnectionId) setLoadingChildren(true);
      loadChildren().finally(() => setLoadingChildren(false));
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps -- 仅在 mount 时按持久化展开态恢复一次

  // 目录监听生命周期:展开时注册 watcher,折叠 / 组件卸载 / 路径变化时自动注销。
  // 旧实现只在手动折叠当前节点时 unwatch,父级折叠或切换项目导致后代节点直接 unmount
  // 时其 watcher 永不释放,会持续累积 OS 文件监听句柄(inotify / ReadDirectoryChangesW)。
  useEffect(() => {
    // 远程项目不做 notify 监听(SFTP 无监听通道);展开重拉 + 树顶手动刷新代替
    if (!entry.isDir || !expanded || remoteConnectionId) return;
    invoke('watch_directory', { path: entry.path, projectPath: projectRoot }).catch(() => {});
    return () => {
      invoke('unwatch_directory', { path: entry.path }).catch(() => {});
    };
  }, [expanded, entry.isDir, entry.path, projectRoot, remoteConnectionId]);

  const handleToggle = useCallback(async () => {
    if (!entry.isDir) {
      onViewFile(entry.path);
      return;
    }
    if (loadingChildren) return; // 加载中忽略重复点击,避免叠发 SFTP 请求
    const next = !expanded;
    // 展开前先加载子节点避免空帧;watcher 的注册/注销由上面的监听生命周期 effect
    // 跟随 expanded 状态自动处理(含 unmount 时的释放),此处不再手动 watch/unwatch。
    if (next) {
      if (remoteConnectionId) setLoadingChildren(true);
      try {
        await loadChildren();
      } finally {
        setLoadingChildren(false);
      }
    }
    setExpanded(next);
    if (activeProjectId) {
      toggleExpandedDir(activeProjectId, entry.path, next);
    }
  }, [entry, expanded, loadingChildren, loadChildren, onViewFile, activeProjectId, remoteConnectionId]);

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (remoteConnectionId) return; // 远程项目无 watcher,fs-change 与本树无关
    if (expanded && payload.path.startsWith(entry.path)) {
      loadChildren();
    }
  }, [expanded, entry.path, loadChildren, remoteConnectionId]));

  return (
    <div>
      <div
        data-file-item
        className={`flex items-center gap-1 py-[3px] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-base transition-colors duration-100 ${
          entry.ignored ? 'text-[var(--text-muted)] opacity-50' : entry.isDir ? 'text-[var(--color-folder)]' : 'text-[var(--color-file)]'
        }`}
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        role="treeitem"
        aria-expanded={entry.isDir ? expanded : undefined}
        aria-label={entry.name}
        tabIndex={0}
        onClick={handleToggle}
        onKeyDown={(e) => {
          // 目录：Enter/Space/→ 展开，← 折叠；文件：Enter/Space 打开预览
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            void handleToggle();
          } else if (entry.isDir && e.key === 'ArrowRight' && !expanded) {
            e.preventDefault();
            void handleToggle();
          } else if (entry.isDir && e.key === 'ArrowLeft' && expanded) {
            e.preventDefault();
            void handleToggle();
          }
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const relativePath = getRelativePath(entry.path, projectRoot);
          // 远程项目 MVP 只读:仅保留复制相对/绝对路径(路径为 POSIX 分隔符,
          // getRelativePath 依据根路径无 `\` 自动选 `/`);「资源管理器显示 /
          // 默认应用打开 / 重命名 / 删除 / 新建」等本地操作一律隐藏。
          if (remoteConnectionId) {
            showContextMenu(e.clientX, e.clientY, [
              {
                label: t('fileTree.menu.copyRelativePath'),
                onClick: () => writeText(relativePath),
              },
              {
                label: t('fileTree.menu.copyAbsolutePath'),
                onClick: () => writeText(entry.path),
              },
            ]);
            return;
          }
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
          loadingChildren ? (
            <span className="w-3 flex items-center justify-center flex-shrink-0">
              <span className="inline-block w-2.5 h-2.5 border border-[var(--text-muted)] border-t-transparent rounded-full animate-spin" />
            </span>
          ) : (
            <span className="text-base w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
              style={{ transform: expanded ? 'rotate(0deg)' : 'rotate(-90deg)', display: 'inline-block' }}>
              ▾
            </span>
          )
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
            // 与根 map 一致:key 掺连接 id,保持「key = 服务器 + 路径」不变量自洽
            key={`${remoteConnectionId ?? 'local'}:${child.path}`}
            entry={child}
            projectRoot={projectRoot}
            depth={depth + 1}
            gitStatusMap={gitStatusMap}
            onViewDiff={onViewDiff}
            onViewFile={onViewFile}
            remoteConnectionId={remoteConnectionId}
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

  // SSH 远程项目:文件树走 ssh_remote_list_directory(SFTP readdir),
  // 不 watch、不拉 git 状态;断链(连接被删除)给明确错误提示。
  const remoteConnectionId = project?.sshConnectionId;
  const isRemote = !!remoteConnectionId;
  const remoteBroken =
    isRemote && !config.sshConnections.some((c) => c.id === remoteConnectionId);
  // 缓存 key:远程项目掺连接 id(不同服务器同 POSIX 路径不得互串);本地即 path
  const cacheKey = project ? projectCacheKey(project) : undefined;

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
    return (cacheKey ? getFileTreeCache(cacheKey) : undefined)?.rootEntries ?? [];
  });
  const [gitStatusMap, setGitStatusMap] = useState<Map<string, GitFileStatus>>(() => {
    return (cacheKey ? getFileTreeCache(cacheKey) : undefined)?.gitStatusMap ?? new Map();
  });
  const [loading, setLoading] = useState(() => !cacheKey || !getFileTreeCache(cacheKey));
  const [loadError, setLoadError] = useState<string | null>(null);
  const [diffTarget, setDiffTarget] = useState<GitFileStatus | null>(null);
  const [viewFilePath, setViewFilePath] = useState<string | null>(null);
  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const rootEntriesRef = useRef(rootEntries);
  rootEntriesRef.current = rootEntries;
  const gitStatusMapRef = useRef(gitStatusMap);
  gitStatusMapRef.current = gitStatusMap;

  const loadGitStatus = useCallback(() => {
    if (!project || isRemote) return; // 远程项目跳过 git 状态(远程 Git 二期)
    const key = projectCacheKey(project);
    invoke<GitFileStatus[]>('get_git_status', { projectPath: project.path })
      .then((statuses) => {
        const map = new Map<string, GitFileStatus>();
        for (const s of statuses) map.set(s.path, s);
        setGitStatusMap(map);
        gitStatusMapRef.current = map;
        setFileTreeCache(key, {
          rootEntries: rootEntriesRef.current,
          gitStatusMap: map,
        });
      })
      .catch(() => setGitStatusMap(new Map()));
  }, [project?.path, isRemote]);

  const debouncedRefresh = useCallback(() => {
    if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
    refreshTimerRef.current = setTimeout(loadGitStatus, 500);
  }, [loadGitStatus]);

  /** 加载根目录列表。`refreshIgnore=true` 仅远程有效:强制后端重读根 .gitignore(手动刷新)。 */
  const loadRootEntries = useCallback((refreshIgnore = false) => {
    if (!project) return;
    const projectPath = project.path;
    if (remoteBroken) {
      setLoading(false);
      setLoadError(t('fileTree.remote.broken'));
      return;
    }
    setLoadError(null);
    if (rootEntriesRef.current.length === 0) setLoading(true);
    const listPromise = remoteConnectionId
      ? invoke<FileEntry[]>('ssh_remote_list_directory', {
          connectionId: remoteConnectionId,
          path: projectPath,
          projectRoot: projectPath,
          refreshIgnore: refreshIgnore || undefined,
        })
      : invoke<FileEntry[]>('list_directory', {
          projectRoot: projectPath,
          path: projectPath,
        });
    listPromise.then((entries) => {
      setRootEntries(entries);
      rootEntriesRef.current = entries;
      setLoading(false);
      setLoadError(null);
      setFileTreeCache(projectCacheKey(project), {
        rootEntries: entries,
        gitStatusMap: gitStatusMapRef.current,
      });
    }).catch((err) => {
      setLoading(false);
      setLoadError(typeof err === 'string' ? err : String(err));
    });
  }, [project?.path, remoteConnectionId, remoteBroken, t]);

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
    const key = projectCacheKey(project);
    const cached = getFileTreeCache(key);
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

    // SSH 远程项目:SFTP readdir,不拉 git 状态、不注册 notify watcher;
    // 断链直接给明确错误(项目仍可见、可删除)。
    if (isRemote) {
      if (remoteBroken) {
        setLoading(false);
        setLoadError(t('fileTree.remote.broken'));
        return () => { cancelled = true; };
      }
      invoke<FileEntry[]>('ssh_remote_list_directory', {
        connectionId: remoteConnectionId,
        path: projectPath,
        projectRoot: projectPath,
      }).then((entries) => {
        if (cancelled) return;
        setRootEntries(entries);
        rootEntriesRef.current = entries;
        setLoading(false);
        setFileTreeCache(key, { rootEntries: entries, gitStatusMap: new Map() });
      }).catch((err) => {
        if (cancelled) return;
        setLoading(false);
        setLoadError(typeof err === 'string' ? err : String(err));
      });
      return () => { cancelled = true; };
    }

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
      setFileTreeCache(key, { rootEntries: entries, gitStatusMap: map });
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
    // t 仅用于断链错误文案,语言切换不重拉列表
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project?.path, remoteConnectionId, remoteBroken]);

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (!project || isRemote) return; // 远程项目无 watcher
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
  }, [project?.path, isRemote, loadRootEntries]));

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (project && !isRemote && payload.projectPath === project.path) {
      debouncedRefresh();
    }
  }, [project?.path, isRemote, debouncedRefresh]));

  const GIT_PATTERNS = [/create mode/, /Switched to/, /Already up to date/, /insertions?\(\+\)/, /deletions?\(-\)/];
  useTauriEvent<PtyOutputPayload>('pty-output', useCallback((payload: PtyOutputPayload) => {
    if (isRemote) return; // 远程项目不做 git 状态刷新
    if (isAiPty(payload.ptyId)) return;
    if (GIT_PATTERNS.some((p) => p.test(payload.data))) {
      debouncedRefresh();
    }
  }, [isRemote, debouncedRefresh]));

  const handleViewDiff = useCallback((status: GitFileStatus) => {
    setDiffTarget(status);
  }, []);

  const handleViewFile = useCallback((path: string) => {
    if (isRemote) return; // 文件查看走本地读文件链路,远程 MVP 只读浏览不支持
    setViewFilePath(path);
  }, [isRemote]);

  const handleRootContextMenu = useCallback((e: React.MouseEvent) => {
    if (!project) return;
    e.preventDefault();
    if (isRemote) return; // 远程 MVP 只读:根目录「新建文件/文件夹」隐藏
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
  }, [project, isRemote, loadRootEntries, t]);

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
          {t('panels.filesOf', { project: project.name })}
        </span>
        <div className="flex items-center flex-shrink-0 gap-1">
          {/* 内容搜索是本地 ripgrep 链路,远程项目隐藏 */}
          {!isRemote && (
            <button
              type="button"
              onClick={() => setSearchModalOpen(true)}
              title={t('fileTree.header.searchTitle', { mod: MOD_LABEL })}
              aria-label={t('fileTree.header.searchTitle', { mod: MOD_LABEL })}
              className="w-[26px] h-[26px] flex items-center justify-center text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
            >
              <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
                <circle cx="7" cy="7" r="4.2" />
                <path d="M10.2 10.2L14 14" />
              </svg>
            </button>
          )}
          <button
            type="button"
            onClick={() => {
              // 远程:refreshIgnore=true 强制重读根 .gitignore;本地:同旧行为
              loadRootEntries(isRemote);
              loadGitStatus();
            }}
            title={isRemote ? t('fileTree.remote.refreshTitle') : t('fileTree.header.refresh')}
            aria-label={isRemote ? t('fileTree.remote.refreshTitle') : t('fileTree.header.refresh')}
            className="w-[26px] h-[26px] flex items-center justify-center text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
              <path d="M13.5 8a5.5 5.5 0 1 1-1.7-3.97" />
              <path d="M13.6 2.6v3.2h-3.2" />
            </svg>
          </button>
          {/* 外部编辑器打开的是本机路径,远程项目隐藏 */}
          {!isRemote && config.editors.length > 0 && (
            <div className="flex items-center ml-0.5 pl-1 border-l border-[var(--border-subtle)]">
              <button
                type="button"
                onClick={() => handleOpenInEditor()}
                title={t('fileTree.header.openWithEditor', { editor: config.editors.find((e) => e.name === config.defaultEditor)?.name ?? config.editors[0]?.name ?? t('fileTree.header.editorFallback') })}
                className="h-[26px] flex items-center text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-xs px-1.5 rounded-l-[var(--radius-sm)] hover:bg-[var(--border-subtle)]"
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
                  className="h-[26px] flex items-center text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-xs pl-1 pr-1.5 rounded-r-[var(--radius-sm)] hover:bg-[var(--border-subtle)] border-l border-[var(--border-subtle)]"
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
      <div className="flex-1 min-h-0 overflow-y-auto px-1" role="tree" aria-label={t('panels.files')} onContextMenu={handleRootContextMenu}>
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
              onClick={() => loadRootEntries()}
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
                // key 掺连接 id:两台服务器可有相同 POSIX 路径(如都是 /root/app)。
                // FileTree 不随切项目 remount,若 key 只用 path,从缓存命中的同路径远程
                // 项目切回时 React 会按 path 复用 TreeNode 实例,其 children/expanded
                // state 仍来自另一台服务器 → 展开子树静默显示错误服务器的文件。掺入
                // remoteConnectionId 后,不同服务器的根节点 key 不同强制整树重挂并按当前
                // 连接重新拉取;切回同一远程项目 key 不变仍复用、保留展开态。
                key={`${remoteConnectionId ?? 'local'}:${entry.path}`}
                entry={entry}
                projectRoot={project.path}
                depth={0}
                gitStatusMap={gitStatusMap}
                onViewDiff={handleViewDiff}
                onViewFile={handleViewFile}
                remoteConnectionId={remoteConnectionId}
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
