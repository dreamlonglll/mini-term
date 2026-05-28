import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showConfirm, showAlert } from './prompt';
import type {
  AppConfig,
  CcConnectConfig,
  CcConnectStatus,
  CcProject,
  ImportProjectRequest,
  ProjectConfig,
} from '../types';

const DEFAULT_CC_CONNECT_CONFIG: CcConnectConfig = {
  exePath: '',
  configPath: '',
  autoStart: false,
  extraArgs: [],
  projectLinks: {},
};

/** 基于 projectId 生成 8 字符 hash 后缀(同名冲突时避免与既存 cc-connect 项目撞名)。 */
function shortHashSuffix(projectId: string): string {
  let h = 0;
  for (let i = 0; i < projectId.length; i++) {
    h = (h * 31 + projectId.charCodeAt(i)) | 0;
  }
  // 转换为 unsigned 32-bit 再 base36,补齐到 8 字符
  const hex = (h >>> 0).toString(36);
  return hex.padStart(8, '0').slice(-8);
}

/** 检查 cc-connect 中是否已存在同名项目,返回唯一名称(冲突时加 hash 后缀)。 */
async function resolveUniqueName(
  project: ProjectConfig,
  configPath: string | undefined,
): Promise<string> {
  const list = await invoke<CcProject[]>('cc_connect_list_projects', {
    configPath: configPath || undefined,
  });
  const existingNames = new Set(list.map((p) => p.name));
  if (!existingNames.has(project.name)) return project.name;
  return `${project.name}_${shortHashSuffix(project.id)}`;
}

/** 刷新一次 cc-connect status(写入 store)。import/unlink 后调用加快 UI 响应,无需等 5s 轮询。 */
async function refreshStatus(configPath: string | undefined): Promise<void> {
  try {
    const status = await invoke<CcConnectStatus>('cc_connect_probe', {
      configPath: configPath || undefined,
    });
    useAppStore.getState().setCcConnectStatus(status);
  } catch {
    // 静默(下一次 5s 轮询会恢复)
  }
}

async function writeProjectLinks(
  patch: (current: Record<string, string>) => Record<string, string>,
): Promise<AppConfig> {
  const cfg = useAppStore.getState().config;
  const currentCc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;
  const newLinks = patch(currentCc.projectLinks ?? {});
  const newCc: CcConnectConfig = { ...currentCc, projectLinks: newLinks };
  const newConfig: AppConfig = { ...cfg, ccConnect: newCc };
  useAppStore.getState().setConfig(newConfig);
  await invoke('save_config', { config: newConfig });
  return newConfig;
}

/**
 * 导入 mini-term 项目到 cc-connect。
 *
 * 流程:
 * 1. confirm 显式提示"将向 cc-connect 添加项目并重启,可能短暂中断 IM 连接"
 * 2. list_projects 二次校验同名冲突,冲突时加 8 字符 hash 后缀
 * 3. cc_connect_import_project(toml_edit 写 config + restart)
 * 4. 写入 AppConfig.ccConnect.projectLinks 并 save_config
 * 5. 立即 probe 刷新 status,调用方负责 refreshCcProjects 触发列表重拉
 *
 * 返回 boolean 指示是否成功(用户取消或失败均返回 false)。
 */
export async function importProjectToCcConnect(
  project: ProjectConfig,
  refreshCcProjects: () => Promise<void>,
): Promise<boolean> {
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;

  const ok = await showConfirm(
    '导入到 cc-connect',
    `将向 cc-connect 添加项目「${project.name}」并重启 cc-connect,可能短暂中断 IM 连接,继续吗?\n\n` +
      `工作目录: ${project.path}\n` +
      `Agent 类型: claudecode (后续可在 dashboard 中修改)`,
  );
  if (!ok) return false;

  try {
    const uniqueName = await resolveUniqueName(project, cc.configPath);
    const req: ImportProjectRequest = {
      name: uniqueName,
      workDir: project.path,
      agentType: 'claudecode',
    };
    await invoke('cc_connect_import_project', {
      req,
      configPath: cc.configPath || undefined,
    });
    await writeProjectLinks((links) => ({ ...links, [project.id]: uniqueName }));
    await refreshStatus(cc.configPath);
    await refreshCcProjects();
    return true;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    await showAlert('导入失败', msg);
    return false;
  }
}

/**
 * 解除 mini-term 项目与 cc-connect 的关联。
 *
 * 流程:
 * 1. confirm "将从 cc-connect 删除该项目并重启"
 * 2. cc_connect_unlink_project(DELETE /api/v1/projects/{name} + restart)
 * 3. 从 AppConfig.ccConnect.projectLinks 删 key 并 save_config
 * 4. 立即 probe 刷新
 *
 * 即使 DELETE 失败(例如项目已被手动从 cc-connect 移除),仍会清理本地 projectLinks
 * 让 UI 摆脱"失效红色"状态。
 */
export async function unlinkProjectFromCcConnect(
  project: ProjectConfig,
  refreshCcProjects: () => Promise<void>,
): Promise<boolean> {
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;
  const linkedName = cc.projectLinks?.[project.id];
  if (!linkedName) {
    await showAlert('未关联', `项目「${project.name}」尚未关联到 cc-connect`);
    return false;
  }

  const ok = await showConfirm(
    '解除 cc-connect 关联',
    `将从 cc-connect 删除项目「${linkedName}」并重启 cc-connect,可能短暂中断 IM 连接,继续吗?`,
  );
  if (!ok) return false;

  try {
    await invoke('cc_connect_unlink_project', {
      name: linkedName,
      configPath: cc.configPath || undefined,
    });
  } catch (e: unknown) {
    // DELETE 失败不阻断本地 link 清理(例如 cc-connect 那边已被手动删了)
    const msg = e instanceof Error ? e.message : String(e);
    const proceed = await showConfirm(
      'cc-connect 删除失败',
      `${msg}\n\n是否仍要从 mini-term 端清理「${project.name}」的关联记录?`,
    );
    if (!proceed) return false;
  }

  await writeProjectLinks((links) => {
    const next = { ...links };
    delete next[project.id];
    return next;
  });
  await refreshStatus(cc.configPath);
  await refreshCcProjects();
  return true;
}
