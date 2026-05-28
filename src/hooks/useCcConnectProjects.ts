import { useEffect, useState, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import type { CcProject } from '../types';

const POLL_INTERVAL_MS = 10000;

/**
 * cc-connect 项目列表轮询 + 失效关联检测。
 *
 * - 仅在 cc-connect running 时拉取 list_projects(避免无意义错误)
 * - 10s 轮询 + 提供手动 refresh 接口(import/unlink 后立即调一次)
 * - 返回 { ccProjects, missingLinks, refresh }:
 *   - ccProjects: cc-connect 中实际存在的项目数组
 *   - missingLinks: 用户在 mini-term 关联了但 cc-connect 中已不存在的 projectId 集合(用于渲染红色失效图标)
 *   - refresh: 手动触发拉取,通常在 import/unlink 后调用以加快 UI 响应
 */
export function useCcConnectProjects(): {
  ccProjects: CcProject[];
  missingLinks: Set<string>;
  refresh: () => Promise<void>;
} {
  const ccConfig = useAppStore((s) => s.config.ccConnect);
  const status = useAppStore((s) => s.ccConnectStatus);
  const running = status?.running ?? false;
  const configPath = ccConfig?.configPath;
  const projectLinks = ccConfig?.projectLinks;

  const [ccProjects, setCcProjects] = useState<CcProject[]>([]);
  // listLoaded:本轮 list_projects 调用成功过一次才置 true;为 false 时不计算 broken,避免:
  // (a) cc-connect 刚启动 probe=running 但 list 还没拉回 → 短暂全红
  // (b) cc-connect restart 中 list 5s 超时抛错 → 全部链接被误标失效
  const [listLoaded, setListLoaded] = useState(false);
  const disposedRef = useRef(false);

  const refresh = useCallback(async () => {
    if (!running) {
      setCcProjects([]);
      setListLoaded(false);
      return;
    }
    try {
      const projects = await invoke<CcProject[]>('cc_connect_list_projects', {
        configPath: configPath || undefined,
      });
      if (!disposedRef.current) {
        setCcProjects(projects);
        setListLoaded(true);
      }
    } catch {
      // cc-connect 可能在 restart 中,本轮失败不重置 ccProjects(保留上一轮成功的结果),
      // 但本轮 listLoaded 不切 true。若从未成功过,ccProjects 仍为 [] + listLoaded=false → 不算 broken。
      // 如此可避免 restart 短暂超时把所有已关联项目误标红色 ⚠
    }
  }, [running, configPath]);

  useEffect(() => {
    disposedRef.current = false;
    if (!running) {
      setCcProjects([]);
      setListLoaded(false);
      return;
    }
    // running 切换时先把 listLoaded 重置,避免用上一轮"已加载"状态判定 broken
    setListLoaded(false);
    void refresh();
    const timer = setInterval(() => { void refresh(); }, POLL_INTERVAL_MS);
    return () => {
      disposedRef.current = true;
      clearInterval(timer);
    };
  }, [running, refresh]);

  // 计算失效关联:projectLinks 里有 key,但 cc-connect 列表里没有对应 name
  // 必须 running && listLoaded 才能判断"失效",否则避免:
  // - cc-connect 未跑(running=false): 不能判失效(不能确定)
  // - cc-connect 正在 probe 未拉到 list (listLoaded=false): 同上
  const missingLinks = new Set<string>();
  if (projectLinks && running && listLoaded) {
    const existingNames = new Set(ccProjects.map((p) => p.name));
    for (const [projectId, ccName] of Object.entries(projectLinks)) {
      if (!existingNames.has(ccName)) {
        missingLinks.add(projectId);
      }
    }
  }

  return { ccProjects, missingLinks, refresh };
}
