import { useCallback, useEffect, useMemo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';
import { useAppStore } from '../store';
import { classifyProject, parsePackageDeps, PROJECT_MARKER_FILES } from '../utils/projectKind';
import type { FileContentResult, FileEntry, FsChangePayload, ProjectConfig, ProjectKind } from '../types';

/**
 * 项目类型探测的缓存与调度。
 *
 * - 本地项目:挂载时批量探测(根目录一层文件名 + package.json deps 细分),结果缓存;
 * - 远程项目:不探测(项目行领位固定显示 SSH 图标);
 * - 失效:根目录标记文件(pom.xml/package.json/…)出现 fs-change 时重探(仅活跃项目
 *   的根目录被 watch,这正是唯一能在应用内改动这些文件的场景)。
 *
 * 缓存本体与版本号在 store(dirKinds/dirKindsVersion)——store 是唯一全局状态;
 * 本模块只留探测调度(pending 为在途去重,不是可订阅状态)。
 */

const pending = new Set<string>();

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

/** 读取缓存的目录类型;undefined = 尚未探测,null = 已探测但识别不出。 */
export function getDirKind(path: string): ProjectKind | null | undefined {
  return useAppStore.getState().dirKinds.get(path);
}

/** 批量探测目录类型(去重、带缓存;仅限本地路径,远程由调用方跳过)。
 *  项目根与文件树里的子工程目录共用同一份缓存。 */
export function ensureDirKinds(paths: string[]): void {
  const { dirKinds, setDirKind } = useAppStore.getState();
  for (const path of paths) {
    if (dirKinds.has(path) || pending.has(path)) continue;
    pending.add(path);
    detectLocal(path)
      .then((kind) => {
        setDirKind(path, kind);
      })
      .catch(() => {
        setDirKind(path, null);
      })
      .finally(() => {
        pending.delete(path);
      });
  }
}

/** 订阅目录类型缓存变化(文件树用:探测完成后重渲染出技术栈图标)。 */
export function useDirKindsVersion(): number {
  return useAppStore((s) => s.dirKindsVersion);
}

/** 返回 projectId → ProjectKind 的映射(识别不出/未就绪的项目不在表里)。 */
export function useProjectKinds(projects: ProjectConfig[]): Map<string, ProjectKind> {
  const version = useAppStore((s) => s.dirKindsVersion);

  useEffect(() => {
    // 远程项目不探测(项目行领位固定显示 SSH 图标);失效(removeDirKind)后
    // version 变化会让本 effect 重跑补探
    ensureDirKinds(projects.filter((p) => !p.sshConnectionId).map((p) => p.path));
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
        if (!proj) return;
        useAppStore.getState().removeDirKind(proj.path);
      },
      [projects],
    ),
  );

  return useMemo(() => {
    const map = new Map<string, ProjectKind>();
    const dirKinds = useAppStore.getState().dirKinds;
    for (const p of projects) {
      const kind = dirKinds.get(p.path);
      if (kind) map.set(p.id, kind);
    }
    return map;
    // version 驱动缓存变化后的重算
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projects, version]);
}
