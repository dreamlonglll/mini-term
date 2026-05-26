import { useAppStore } from '../store';

/**
 * 取出某项目用于 create_pty 注入的环境变量,过滤掉 enabled=false 与空 key 的行。
 * 返回 Tauri command 期望的 `Vec<(String, String)>` 形态(JSON 中即二维数组)。
 */
export function getProjectEnvs(projectId: string): Array<[string, string]> {
  const cfg = useAppStore.getState().config;
  const project = cfg.projects.find((p) => p.id === projectId);
  const list = project?.envVars;
  if (!list || list.length === 0) return [];
  return list
    .filter((e) => e.enabled && e.key.trim())
    .map((e) => [e.key, e.value] as [string, string]);
}
