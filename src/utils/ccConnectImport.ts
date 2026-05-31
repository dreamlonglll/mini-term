import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { t } from '../i18n';
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

/**
 * 导入会附带的占位平台说明(后端 make_project_table 硬编码注入一个占位 telegram 平台)。
 * cc-connect 强制每个项目至少一个 [[projects.platforms]],否则冷启动 os.Exit(1);导入时拿不到真实
 * IM 凭据,故注入占位平台保证可冷启动,用户后续在 Dashboard 替换为真实平台。confirm 文案据此告知。
 */
const placeholderNote = () => t('ccConnectImport.placeholderNote');

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

/** 刷新一次 cc-connect status(写入 store)。import/移除 后调用加快 UI 响应,无需等 5s 轮询。 */
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
 * 导入单个 mini-term 项目到 cc-connect。
 *
 * 流程:confirm(含占位平台提示 + 重启风险) → list_projects 二次去重 → cc_connect_import_project
 * (toml_edit 写 config + restart) → 写 projectLinks → 立即 probe 刷新。
 * tomlWritten=true 即写 projectLinks(不卡在 restartOk),避免"项目存在但未关联"半同步态。
 * 返回 boolean:用户取消或失败返回 false。
 */
export async function importProjectToCcConnect(project: ProjectConfig): Promise<boolean> {
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;

  const ok = await showConfirm(
    t('ccConnectImport.importTitle'),
    t('ccConnectImport.importConfirm', {
      name: project.name,
      note: placeholderNote(),
      path: project.path,
    }),
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
    if (result.tomlWritten) {
      await writeProjectLinks((links) => ({ ...links, [project.id]: result.name }));
      await refreshStatus(cc.configPath);
      if (!result.restartOk) {
        await showAlert(
          t('ccConnectImport.importRestartFailedTitle'),
          t('ccConnectImport.importRestartFailedMsg', {
            name: result.name,
            error: result.restartError ?? t('ccConnectImport.unknownError'),
          }),
        );
      }
      return true;
    }
    await showAlert(
      t('ccConnectImport.importFailedTitle'),
      t('ccConnectImport.importFailedNoWrite'),
    );
    return false;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    await showAlert(t('ccConnectImport.importFailedTitle'), msg);
    return false;
  }
}

/**
 * 批量导入多个 mini-term 项目到 cc-connect(一次写盘 + 仅重启一次 cc-connect)。
 *
 * 相比逐个调 importProjectToCcConnect,避免 N 次 restart 多次断开 IM active sessions。
 * 流程:一次 confirm 列出全部待导入项 → list_projects 拉现有名 → "现有 + 批次内部"统一去重
 * (冲突加 8 字符 hash 后缀) → cc_connect_import_projects 批量写 toml + 单次 restart →
 * 一次性写入所有 projectLinks → 刷新 status。返回 boolean:用户取消或失败返回 false。
 */
export async function importProjectsToCcConnect(projects: ProjectConfig[]): Promise<boolean> {
  if (projects.length === 0) return false;
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;

  const MAX_LIST = 15;
  const names = projects.map((p) => `· ${p.name}`);
  const listStr =
    names.length > MAX_LIST
      ? `${names.slice(0, MAX_LIST).join('\n')}\n${t('ccConnectImport.batchListMore', { count: projects.length })}`
      : names.join('\n');
  const ok = await showConfirm(
    t('ccConnectImport.batchImportTitle'),
    t('ccConnectImport.batchImportConfirm', {
      count: projects.length,
      list: listStr,
      note: placeholderNote(),
    }),
  );
  if (!ok) return false;

  try {
    // 拉现有列表,本批次内部 + 与现有项目统一去重
    const list = await invoke<CcProject[]>('cc_connect_list_projects', {
      configPath: cc.configPath || undefined,
    });
    const used = new Set(list.map((p) => p.name));
    const reqs: ImportProjectRequest[] = [];
    const idToName: Record<string, string> = {};
    for (const p of projects) {
      let name = p.name;
      if (used.has(name)) name = `${p.name}_${shortHashSuffix(p.id)}`;
      // 极端兜底:加 hash 后仍冲突(几乎不可能),继续追加直到唯一
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
      // 一次写入所有 projectLinks(避免多次 save_config)
      await writeProjectLinks((links) => {
        const next = { ...links };
        for (const p of projects) next[p.id] = idToName[p.id];
        return next;
      });
      await refreshStatus(cc.configPath);
      if (!result.restartOk) {
        await showAlert(
          t('ccConnectImport.batchRestartFailedTitle'),
          t('ccConnectImport.batchRestartFailedMsg', {
            count: result.imported.length,
            error: result.restartError ?? t('ccConnectImport.unknownError'),
          }),
        );
      }
      return true;
    }
    // tomlWritten=false:选中项目在 cc-connect 中均已存在(前端去重后理论不可达)
    await showAlert(
      t('ccConnectImport.batchNoNeedTitle'),
      t('ccConnectImport.batchNoNeedMsg'),
    );
    return false;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    await showAlert(t('ccConnectImport.batchImportFailedTitle'), msg);
    return false;
  }
}

/**
 * 从 cc-connect 移除已导入的项目(删除 [[projects]] + restart),用于纠错/撤销导入。
 *
 * 流程:confirm → cc_connect_unlink_project(DELETE /api/v1/projects/{name} + restart)→
 * 从 projectLinks 删 key → 刷新 status。即使 DELETE 失败(例如用户已在 Dashboard 手动删了),
 * 仍二次确认后清理本地 projectLinks 让 UI 摆脱"已导入"残留。返回 boolean。
 */
export async function unlinkProjectFromCcConnect(project: ProjectConfig): Promise<boolean> {
  const cfg = useAppStore.getState().config;
  const cc = cfg.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;
  const linkedName = cc.projectLinks?.[project.id];
  if (!linkedName) {
    await showAlert(
      t('ccConnectImport.notImportedTitle'),
      t('ccConnectImport.notImportedMsg', { name: project.name }),
    );
    return false;
  }

  const ok = await showConfirm(
    t('ccConnectImport.removeTitle'),
    t('ccConnectImport.removeConfirm', { name: linkedName }),
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
      restartWarning = result.restartError ?? t('ccConnectImport.unknownError');
    }
  } catch (e: unknown) {
    // DELETE 失败不阻断本地 link 清理(例如 cc-connect 那边已被手动删了)
    const msg = e instanceof Error ? e.message : String(e);
    const proceed = await showConfirm(
      t('ccConnectImport.deleteFailedTitle'),
      t('ccConnectImport.deleteFailedConfirm', { error: msg, name: project.name }),
    );
    if (!proceed) return false;
  }

  await writeProjectLinks((links) => {
    const next = { ...links };
    delete next[project.id];
    return next;
  });
  await refreshStatus(cc.configPath);
  if (restartWarning) {
    await showAlert(
      t('ccConnectImport.removeRestartFailedTitle'),
      t('ccConnectImport.removeRestartFailedMsg', {
        name: linkedName,
        error: restartWarning,
      }),
    );
  }
  return true;
}
