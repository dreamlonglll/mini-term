import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';
import { classifyProject, parsePackageDeps, PROJECT_MARKER_FILES } from '../utils/projectKind';
import type { ProjectKind } from '../utils/projectKind';
import type { FileContentResult, FileEntry, FsChangePayload, ProjectConfig } from '../types';

/**
 * 项目类型探测的缓存与调度。
 *
 * - 本地项目:挂载时批量探测(根目录一层文件名 + package.json deps 细分),结果缓存;
 * - 远程项目:不探测(项目行领位固定显示 SSH 图标);
 * - 失效:根目录标记文件(pom.xml/package.json/…)出现 fs-change 时重探(仅活跃项目
 *   的根目录被 watch,这正是唯一能在应用内改动这些文件的场景)。
 */

// 模块级缓存:key = 项目路径(原样)。value null = 已探测但识别不出,不再重复探测。
const kindCache = new Map<string, ProjectKind | null>();
const pending = new Set<string>();
const listeners = new Set<() => void>();

function notify() {
  listeners.forEach((fn) => fn());
}

function normPath(p: string): string {
  return p.replace(/[\\/]+/g, '/').replace(/\/+$/, '');
}

async function detectLocal(projectPath: string): Promise<ProjectKind | null> {
  const entries = await invoke<FileEntry[]>('list_directory', {
    projectRoot: projectPath,
    path: projectPath,
  });
  const files = new Set(entries.filter((e) => !e.isDir).map((e) => e.name));
  let deps: Record<string, string> | undefined;
  if (files.has('package.json')) {
    const sep = projectPath.includes('/') ? '/' : '\\';
    try {
      const res = await invoke<FileContentResult>('read_file_content', {
        projectRoot: projectPath,
        path: `${projectPath}${sep}package.json`,
      });
      if (!res.isBinary && !res.tooLarge) deps = parsePackageDeps(res.content);
    } catch {
      // 读不了 package.json:按无 deps 处理,仍能给出 nodejs 级别的判定
    }
  }
  return classifyProject(files, deps);
}

/** 返回 projectId → ProjectKind 的映射(识别不出/未就绪的项目不在表里)。 */
export function useProjectKinds(projects: ProjectConfig[]): Map<string, ProjectKind> {
  const [version, setVersion] = useState(0);

  useEffect(() => {
    const fn = () => setVersion((v) => v + 1);
    listeners.add(fn);
    return () => {
      listeners.delete(fn);
    };
  }, []);

  useEffect(() => {
    for (const p of projects) {
      if (p.sshConnectionId) continue; // 远程项目等 FileTree seed
      if (kindCache.has(p.path) || pending.has(p.path)) continue;
      pending.add(p.path);
      detectLocal(p.path)
        .then((kind) => {
          kindCache.set(p.path, kind);
          notify();
        })
        .catch(() => {
          kindCache.set(p.path, null);
        })
        .finally(() => {
          pending.delete(p.path);
        });
    }
  }, [projects, version]);

  useTauriEvent<FsChangePayload>(
    'fs-change',
    useCallback(
      (payload: FsChangePayload) => {
        const changed = normPath(payload.path);
        const idx = changed.lastIndexOf('/');
        if (idx < 0) return;
        if (!PROJECT_MARKER_FILES.has(changed.slice(idx + 1))) return;
        const parent = changed.slice(0, idx);
        const proj = projects.find((p) => !p.sshConnectionId && normPath(p.path) === parent);
        if (!proj || !kindCache.has(proj.path)) return;
        kindCache.delete(proj.path);
        setVersion((v) => v + 1); // 触发上面的探测 effect 重跑
      },
      [projects],
    ),
  );

  return useMemo(() => {
    const map = new Map<string, ProjectKind>();
    for (const p of projects) {
      const kind = kindCache.get(p.path);
      if (kind) map.set(p.id, kind);
    }
    return map;
    // version 驱动缓存变化后的重算
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projects, version]);
}
