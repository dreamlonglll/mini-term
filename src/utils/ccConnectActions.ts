import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showConfirm, showAlert } from './prompt';
import type {
  AppConfig,
  BatchImportResult,
  CcConnectConfig,
  CcConnectStatus,
  CcProject,
  ImportProjectRequest,
  ImportProjectResult,
  ProjectConfig,
  UnlinkProjectResult,
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
    const result = await invoke<ImportProjectResult>('cc_connect_import_project', {
      req,
      configPath: cc.configPath || undefined,
    });
    // 后端契约:tomlWritten 为 true 时(目前只可能为 true,失败直接 Err 出),
    // 不论 restartOk 都写本地 projectLinks,避免"toml 写了但 link 没写"半同步态。
    if (result.tomlWritten) {
      await writeProjectLinks((links) => ({ ...links, [project.id]: result.name }));
      await refreshStatus(cc.configPath);
      await refreshCcProjects();
      if (!result.restartOk) {
        await showAlert(
          '导入成功但 cc-connect 重启失败',
          `项目「${result.name}」已写入 cc-connect 配置;但重启 cc-connect 失败:\n${result.restartError ?? '未知错误'}\n\n下次启动 cc-connect 时新项目会生效。`,
        );
      }
      return true;
    }
    // 理论不可达(toml 未写时后端会 Err 出),但保留分支防御
    await showAlert('导入失败', 'cc-connect 未能写入项目配置');
    return false;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    await showAlert('导入失败', msg);
    return false;
  }
}

/**
 * 批量导入多个 mini-term 项目到 cc-connect（一次写盘 + 仅重启一次 cc-connect）。
 *
 * 相比逐个调 importProjectToCcConnect，避免 N 次 restart 多次断开 IM active sessions。
 * 流程：
 * 1. 一次 confirm 列出全部待导入项目
 * 2. list_projects 拉现有项目名，在“现有 + 本批次内部”统一去重（冲突加 8 字符 hash 后缀）
 * 3. cc_connect_import_projects 批量写 toml + 单次 restart
 * 4. 一次性写入所有 projectLinks 并 save_config
 * 5. 刷新 status + cc 项目列表
 *
 * 返回 boolean：用户取消或失败返回 false。
 */
export async function importProjectsToCcConnect(
  projects: ProjectConfig[],
  refreshCcProjects: () => Promise<void>,
): Promise<boolean> {
  if (projects.length === 0) return false;
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;

  const MAX_LIST = 15;
  const names = projects.map((p) => `· ${p.name}`);
  const listStr =
    names.length > MAX_LIST
      ? `${names.slice(0, MAX_LIST).join('\n')}\n…等共 ${projects.length} 个`
      : names.join('\n');
  const ok = await showConfirm(
    '批量导入到 cc-connect',
    `将向 cc-connect 添加以下 ${projects.length} 个项目并重启一次 cc-connect，可能短暂中断 IM 连接，继续吗？\n\n${listStr}`,
  );
  if (!ok) return false;

  try {
    // 拉现有列表，本批次内部 + 与现有项目统一去重
    const list = await invoke<CcProject[]>('cc_connect_list_projects', {
      configPath: cc.configPath || undefined,
    });
    const used = new Set(list.map((p) => p.name));
    const reqs: ImportProjectRequest[] = [];
    const idToName: Record<string, string> = {};
    for (const p of projects) {
      let name = p.name;
      if (used.has(name)) name = `${p.name}_${shortHashSuffix(p.id)}`;
      // 极端兜底：加 hash 后仍冲突（几乎不可能），继续追加直到唯一
      while (used.has(name)) name = `${name}_x`;
      used.add(name);
      idToName[p.id] = name;
      reqs.push({ name, workDir: p.path, agentType: 'claudecode' });
    }

    const result = await invoke<BatchImportResult>('cc_connect_import_projects', {
      reqs,
      configPath: cc.configPath || undefined,
    });

    if (result.tomlWritten) {
      // 一次写入所有 projectLinks（避免多次 save_config）
      await writeProjectLinks((links) => {
        const next = { ...links };
        for (const p of projects) next[p.id] = idToName[p.id];
        return next;
      });
      await refreshStatus(cc.configPath);
      await refreshCcProjects();
      if (!result.restartOk) {
        await showAlert(
          '批量导入成功但 cc-connect 重启失败',
          `${result.imported.length} 个项目已写入 cc-connect 配置；但重启 cc-connect 失败：\n${result.restartError ?? '未知错误'}\n\n下次启动 cc-connect 时新项目会生效。`,
        );
      }
      return true;
    }
    // tomlWritten=false：选中项目在 cc-connect 中均已存在（前端去重后理论不可达）
    await showAlert('无需导入', '选中的项目在 cc-connect 中均已存在');
    return false;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    await showAlert('批量导入失败', msg);
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

  let restartWarning: string | null = null;
  try {
    const result = await invoke<UnlinkProjectResult>('cc_connect_unlink_project', {
      name: linkedName,
      configPath: cc.configPath || undefined,
    });
    // 后端契约:deletedOk=true 即返 Ok,restartOk 失败时仅 toast 提示但仍清本地 link。
    if (!result.restartOk) {
      restartWarning = result.restartError ?? '未知错误';
    }
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
  if (restartWarning) {
    await showAlert(
      '解除关联成功但 cc-connect 重启失败',
      `项目「${linkedName}」已从 cc-connect 删除;但重启 cc-connect 失败:\n${restartWarning}\n\n下次启动 cc-connect 时会生效。`,
    );
  }
  return true;
}
