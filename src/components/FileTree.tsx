import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { revealItemInDir, openPath } from '@tauri-apps/plugin-opener';
import { useAppStore } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { showContextMenu } from '../utils/contextMenu';
import { showPrompt } from '../utils/prompt';
import type { FileEntry, FsChangePayload } from '../types';

interface TreeNodeProps {
  entry: FileEntry;
  projectRoot: string;
  depth: number;
}

function TreeNode({ entry, projectRoot, depth }: TreeNodeProps) {
  const [expanded, setExpanded] = useState(false);
  const [children, setChildren] = useState<FileEntry[]>([]);

  const loadChildren = useCallback(async () => {
    const entries = await invoke<FileEntry[]>('list_directory', {
      projectRoot,
      path: entry.path,
    });
    setChildren(entries);
  }, [entry.path, projectRoot]);

  const handleToggle = useCallback(async () => {
    if (!entry.isDir) return;
    if (!expanded) {
      await loadChildren();
      invoke('watch_directory', { path: entry.path, projectPath: projectRoot });
    } else {
      invoke('unwatch_directory', { path: entry.path });
    }
    setExpanded(!expanded);
  }, [entry, expanded, loadChildren, projectRoot]);

  useTauriEvent<FsChangePayload>('fs-change', useCallback((payload: FsChangePayload) => {
    if (expanded && payload.path.startsWith(entry.path)) {
      loadChildren();
    }
  }, [expanded, entry.path, loadChildren]));

  return (
    <div>
      <div
        className={`flex items-center gap-1 py-[3px] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-base transition-colors duration-100 ${
          entry.isDir ? 'text-[var(--color-folder)]' : 'text-[var(--color-file)]'
        }`}
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        onClick={handleToggle}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const items: Parameters<typeof showContextMenu>[2] = [
            {
              label: '在文件夹中打开',
              onClick: () => revealItemInDir(entry.path),
            },
          ];
          if (!entry.isDir) {
            items.unshift({
              label: '使用默认工具打开',
              onClick: () => openPath(entry.path),
            });
          }
          if (entry.isDir) {
            items.push({ separator: true });
            items.push({
              label: '新建文件',
              onClick: async () => {
                const name = await showPrompt('新建文件', '请输入文件名');
                if (!name?.trim()) return;
                const sep = entry.path.includes('/') ? '/' : '\\';
                await invoke('create_file', { path: `${entry.path}${sep}${name.trim()}` });
                if (!expanded) handleToggle();
                else loadChildren();
              },
            });
            items.push({
              label: '新建文件夹',
              onClick: async () => {
                const name = await showPrompt('新建文件夹', '请输入文件夹名');
                if (!name?.trim()) return;
                const sep = entry.path.includes('/') ? '/' : '\\';
                await invoke('create_directory', { path: `${entry.path}${sep}${name.trim()}` });
                if (!expanded) handleToggle();
                else loadChildren();
              },
            });
          }
          showContextMenu(e.clientX, e.clientY, items);
        }}
        draggable
        onDragStart={(e) => {
          e.dataTransfer.setData('text/plain', entry.path);
          e.dataTransfer.effectAllowed = 'copy';
        }}
      >
        {entry.isDir && (
          <span className="text-[13px] w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
            style={{ transform: expanded ? 'rotate(0deg)' : 'rotate(-90deg)', display: 'inline-block' }}>
            ▾
          </span>
        )}
        {!entry.isDir && <span className="w-3 text-center text-[var(--text-muted)] text-xs">·</span>}
        <span className="truncate">{entry.name}</span>
      </div>

      {expanded &&
        children.map((child) => (
          <TreeNode key={child.path} entry={child} projectRoot={projectRoot} depth={depth + 1} />
        ))}
    </div>
  );
}

export function FileTree() {
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const config = useAppStore((s) => s.config);
  const project = config.projects.find((p) => p.id === activeProjectId);

  const [rootEntries, setRootEntries] = useState<FileEntry[]>([]);

  useEffect(() => {
    if (!project) {
      setRootEntries([]);
      return;
    }
    invoke<FileEntry[]>('list_directory', {
      projectRoot: project.path,
      path: project.path,
    }).then(setRootEntries);
  }, [project?.path]);

  if (!project) {
    return (
      <div className="h-full bg-[var(--bg-surface)] flex items-center justify-center text-[var(--text-muted)] text-base">
        选择一个项目
      </div>
    );
  }

  return (
    <div className="h-full bg-[var(--bg-surface)] flex flex-col overflow-y-auto border-l border-[var(--border-subtle)]">
      <div className="px-3 pt-3 pb-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium">
        Files — {project.name}
      </div>
      <div className="flex-1 px-1">
        {rootEntries.map((entry) => (
          <TreeNode key={entry.path} entry={entry} projectRoot={project.path} depth={0} />
        ))}
      </div>
    </div>
  );
}
