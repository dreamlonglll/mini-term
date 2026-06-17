import { useState, useEffect, useCallback, useRef } from 'react';
import { Allotment } from 'allotment';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import { ask } from '@tauri-apps/plugin-dialog';
import { useAppStore, restoreLayout, flushLayoutToConfig, initExpandedDirs, flushExpandedDirsToConfig, flushProjectToConfig, persistConfig } from './store';
import { TerminalArea } from './components/TerminalArea';
import { ProjectList } from './components/ProjectList';
import { FileTree } from './components/FileTree';
import { GitHistory } from './components/GitHistory';
import { ActivityBar } from './components/ActivityBar';
import { SettingsModal, type SettingsPage } from './components/SettingsModal';
import { SshModal } from './components/SshModal';
import { SearchModal } from './components/SearchModal';
import { ToastContainer } from './components/ToastContainer';
import { CcConnectModal } from './components/CcConnectModal';
import { CcConnectDashboard } from './components/CcConnectDashboard';
import { useTauriEvent } from './hooks/useTauriEvent';
import { useAiSubmitMarker } from './hooks/useAiSubmitMarker';
import { useMarkerHotkeys } from './hooks/useMarkerHotkeys';
import { useExternalFileDrop } from './hooks/useExternalFileDrop';
import { useCcConnectProbe } from './hooks/useCcConnectProbe';
import { checkForUpdate, type ReleaseInfo } from './utils/updateChecker';
import { applyTheme } from './utils/themeManager';
import { applyUiFontFamily } from './utils/fontManager';
import { markAiPty, updateAllTerminalThemes } from './utils/terminalCache';
import { includeActiveProject } from './utils/projectKeepAlive';
import { useT } from './i18n';
import type { AppConfig, PtyStatusChangePayload, PtyExitPayload, PaneStatus, CcConnectStatus, CcConnectConfig } from './types';

/** cc-connect 未保存配置时,「连接」弹窗 / Dashboard 打开期间仍以默认路径探活的占位配置。 */
const EMPTY_CC_CONFIG: CcConnectConfig = {
  exePath: '',
  configPath: '',
  autoStart: false,
  extraArgs: [],
  projectLinks: {},
};

export function App() {
  const t = useT();
  const [configLoaded, setConfigLoaded] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [configPage, setConfigPage] = useState<SettingsPage | undefined>(undefined);
  const [sshOpen, setSshOpen] = useState(false);
  const [ccConnectOpen, setCcConnectOpen] = useState(false);
  const ccDashboardOpen = useAppStore((s) => s.ccDashboardOpen);
  const ccDashboardDeepLink = useAppStore((s) => s.ccDashboardDeepLink);
  const closeCcDashboard = useAppStore((s) => s.closeCcDashboard);
  const [currentVersion, setCurrentVersion] = useState('');
  const [updateInfo, setUpdateInfo] = useState<ReleaseInfo | null>(null);
  const [mountedProjectIds, setMountedProjectIds] = useState<string[]>([]);
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const updatePaneStatusByPty = useAppStore((s) => s.updatePaneStatusByPty);
  const searchModalOpen = useAppStore((s) => s.searchModalOpen);
  const setSearchModalOpen = useAppStore((s) => s.setSearchModalOpen);

  useEffect(() => {
    invoke<AppConfig>('load_config').then((cfg) => {
      setConfig(cfg);
      // 应用 UI 字体大小
      if (cfg.uiFontSize) {
        document.documentElement.style.fontSize = `${cfg.uiFontSize}px`;
      }
      applyUiFontFamily(cfg.uiFontFamily);
      const { projectStates } = useAppStore.getState();
      const newStates = new Map(projectStates);
      for (const p of cfg.projects) {
        if (!newStates.has(p.id)) {
          newStates.set(p.id, { id: p.id, tabs: [], activeTabId: '' });
        }
      }
      const lastActive = cfg.lastActiveProjectId;
      const initialActive =
        lastActive && cfg.projects.some((p) => p.id === lastActive)
          ? lastActive
          : cfg.projects[0]?.id ?? null;
      useAppStore.setState({
        projectStates: newStates,
        activeProjectId: initialActive,
      });

      // 恢复各项目的展开目录状态
      for (const p of cfg.projects) {
        initExpandedDirs(p.id, p.expandedDirs ?? []);
      }

      applyTheme(cfg.theme ?? 'auto');

      for (const p of cfg.projects) {
        if (p.savedLayout && p.savedLayout.tabs.length > 0) {
          restoreLayout(p.id, p.savedLayout, cfg);
        }
      }

      setConfigLoaded(true);

      // 布局元数据恢复完成后显示窗口；终端进程由可见 pane 按需创建。
      const showWindow = () => {
        // 双 rAF 确保 React 首帧布局完成后再显示。
        requestAnimationFrame(() => requestAnimationFrame(() => {
          getCurrentWindow().show();
        }));
      };
      showWindow();

      // cc-connect autoStart:首次 probe 发现未运行时尝试 spawn(勾选了 autoStart 即可,
      // 未填写可执行文件时回退 PATH 中的 cc-connect)
      const ccCfg = cfg.ccConnect;
      if (ccCfg?.autoStart) {
        const autoExe = ccCfg.exePath?.trim() || 'cc-connect';
        invoke<CcConnectStatus>('cc_connect_probe', {
          configPath: ccCfg.configPath || undefined,
        }).then((status) => {
          useAppStore.getState().setCcConnectStatus(status);
          if (!status.running) {
            return invoke<number>('cc_connect_start', {
              exePath: autoExe,
              configPath: ccCfg.configPath || undefined,
              extraArgs: ccCfg.extraArgs ?? [],
            }).then(() => {
              // spawn 后等 ~600ms 让 cc-connect 起监听端口再重新 probe
              setTimeout(() => {
                invoke<CcConnectStatus>('cc_connect_probe', {
                  configPath: ccCfg.configPath || undefined,
                })
                  .then((s) => useAppStore.getState().setCcConnectStatus(s))
                  .catch(() => {});
              }, 600);
            });
          }
        }).catch(() => {
          // autoStart 失败静默(用户可在设置面板手动启动 + 看错误诊断)
        });
      }
    });
  }, []);

  // cc-connect 状态 5s 轮询(失焦时暂停节省 CPU)、常驻探活:配置过用保存的 configPath,
  // 未配置则以默认 ~/.cc-connect/config.toml 探活 —— 这样零配置下也能识别 cc-connect 运行态,
  // 让项目列表的"导入到 cc-connect"右键菜单在 running 时直接可用。
  // 无 cc-connect 的用户:探活只是一次快速失败的文件读(不产生 HTTP),UI 也不展示任何状态。
  const ccProbeConfig = config.ccConnect ?? EMPTY_CC_CONFIG;
  useCcConnectProbe(configLoaded ? ccProbeConfig : undefined);

  // 阻止浏览器默认的文件拖放行为（防止导航到拖入的文件）
  useEffect(() => {
    const prevent = (e: DragEvent) => {
      if (e.dataTransfer?.types.includes('Files')) e.preventDefault();
    };
    document.addEventListener('dragover', prevent);
    document.addEventListener('drop', prevent);
    return () => {
      document.removeEventListener('dragover', prevent);
      document.removeEventListener('drop', prevent);
    };
  }, []);

  // 防御输入法候选框把布局「顶开」(issue #34):WebView2 在 IME composition 时会把获得
  // 焦点的 xterm helper-textarea 滚进可视区,给某个 overflow:hidden 的布局祖先(Allotment
  // pane / 主内容区 / #root/body)设了非 0 的 scrollLeft,整页内容被横向推走、右侧露出桌面。
  // 这类布局容器本就不该横向滚动,监听到偏移即复位;合法横向滚动容器是 overflow-x:auto/scroll
  // (代码块、tab 栏、modal),scrollLeft 短路或 overflowX 非 hidden 而被放过,不受影响。
  useEffect(() => {
    const onScroll = (e: Event) => {
      const node = e.target instanceof HTMLElement ? e.target : document.scrollingElement;
      if (!(node instanceof HTMLElement) || node.scrollLeft === 0) return;
      if (getComputedStyle(node).overflowX === 'hidden') node.scrollLeft = 0;
    };
    window.addEventListener('scroll', onScroll, true);
    return () => window.removeEventListener('scroll', onScroll, true);
  }, []);

  // Ctrl+Shift+F 打开/关闭搜索弹窗
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key === 'F') {
        e.preventDefault();
        const { searchModalOpen: isOpen, setSearchModalOpen: setOpen } = useAppStore.getState();
        setOpen(!isOpen);
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, []);

  // 主题变化时应用新主题
  useEffect(() => {
    applyTheme(config.theme ?? 'auto');
  }, [config.theme]);

  // 皮肤变化时应用
  useEffect(() => {
    const skin = config.skin ?? 'none';
    document.documentElement.dataset.skin = skin === 'none' ? '' : skin;
    updateAllTerminalThemes(config.terminalFollowTheme);
  }, [config.skin]);

  // 启动时获取版本号并检查更新
  useEffect(() => {
    getVersion().then((ver) => {
      setCurrentVersion(ver);
      checkForUpdate(ver).then((release) => {
        if (release) setUpdateInfo(release);
      }).catch(() => {});
    });
  }, []);

  useTauriEvent<PtyStatusChangePayload>('pty-status-change', useCallback((payload) => {
    markAiPty(payload.ptyId, payload.status === 'ai-working' || payload.status === 'ai-idle');
    updatePaneStatusByPty(payload.ptyId, payload.status as PaneStatus);
  }, [updatePaneStatusByPty]));

  useTauriEvent<PtyExitPayload>('pty-exit', useCallback((payload) => {
    if (payload.exitCode !== 0) {
      updatePaneStatusByPty(payload.ptyId, 'error');
    }
  }, [updatePaneStatusByPty]));

  // WSL 启动器重写提示:后端检测到 cwd 是 WSL UNC 路径并强制改用 wsl.exe 启动时,
  // 弹一次性 toast(5s 自动消失)。projectId 仅作占位 (不参与跳转,kind='wsl-info' 已屏蔽点击跳转)。
  useTauriEvent<{ ptyId: number; distro: string; unixPath: string }>(
    'wsl-shell-override',
    useCallback((payload) => {
      useAppStore.getState().pushNotification({
        projectId: '__wsl_info__',
        projectName: `WSL: ${payload.distro}`,
        kind: 'wsl-info',
        message: t('app.wslOverride', { path: payload.unixPath }),
      });
    }, []),
  );

  useAiSubmitMarker();
  useMarkerHotkeys();
  useExternalFileDrop();

  // 关闭窗口时二次确认并保存布局
  useEffect(() => {
    const appWindow = getCurrentWindow();
    const unlisten = appWindow.onCloseRequested(async (event) => {
      event.preventDefault();
      const confirmed = await ask(t('app.closeConfirm.message'), { title: t('app.closeConfirm.title'), kind: 'warning' });
      if (!confirmed) return;
      const { projectStates, activeProjectId: currentActive, config: currentConfig } = useAppStore.getState();
      for (const projectId of projectStates.keys()) {
        flushLayoutToConfig(projectId);
        flushExpandedDirsToConfig(projectId);
      }
      if (currentActive && currentConfig.lastActiveProjectId !== currentActive) {
        useAppStore.getState().setConfig({ ...useAppStore.getState().config, lastActiveProjectId: currentActive });
      }
      // flush 只更新 store，最后统一写一次磁盘
      await persistConfig().catch(() => {});
      appWindow.destroy();
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 切换项目时保存前一个项目的布局（合并为一次 setConfig）
  const prevProjectRef = useRef<string | null>(null);
  useEffect(() => {
    if (prevProjectRef.current && prevProjectRef.current !== activeProjectId) {
      flushProjectToConfig(prevProjectRef.current);
      persistConfig();
    }
    prevProjectRef.current = activeProjectId;
  }, [activeProjectId]);

  useEffect(() => {
    const existingIds = new Set(config.projects.map((p) => p.id));
    setMountedProjectIds((ids) =>
      includeActiveProject(ids.filter((id) => existingIds.has(id)), activeProjectId)
    );
  }, [activeProjectId, config.projects]);

  // 派生：左栏/中栏是否可见
  const leftColumnVisible = config.projectsVisible || config.sessionsVisible;
  const middleColumnVisible = config.filesVisible || config.gitVisible;
  const terminalProjectIds = includeActiveProject(mountedProjectIds, activeProjectId);

  // 防抖保存布局尺寸
  const saveTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const saveLayoutSizes = useCallback((sizes: number[]) => {
    clearTimeout(saveTimer.current);
    saveTimer.current = setTimeout(() => {
      const cfg = useAppStore.getState().config;
      const newConfig = { ...cfg, layoutSizes: sizes };
      setConfig(newConfig);
      invoke('save_config', { config: newConfig });
    }, 500);
  }, [setConfig]);

  const saveMidTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const saveMiddleColumnSizes = useCallback((sizes: number[]) => {
    clearTimeout(saveMidTimer.current);
    saveMidTimer.current = setTimeout(() => {
      const cfg = useAppStore.getState().config;
      const newConfig = { ...cfg, middleColumnSizes: sizes };
      setConfig(newConfig);
      invoke('save_config', { config: newConfig });
    }, 500);
  }, [setConfig]);

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-4 px-4 py-2 bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)] text-xs select-none"
        onMouseDown={(e) => {
          // 用 Tauri API 拖拽替代 -webkit-app-region: drag，
          // 避免 WebView2 内部拖拽模态循环导致外部截图工具触发输入锁定
          if (e.button === 0 && !(e.target as HTMLElement).closest('[data-no-drag]')) {
            e.preventDefault();
            getCurrentWindow().startDragging();
          }
        }}>
        <span className="font-semibold tracking-wide text-[var(--accent)] text-sm" style={{ fontFamily: "'DM Sans', sans-serif", letterSpacing: '0.05em' }}>
          MINI-TERM
        </span>
        {currentVersion && (
          <span className="text-[10px] text-[var(--text-muted)] font-mono">v{currentVersion}</span>
        )}
        {updateInfo && (
          <span
            className="text-[10px] px-1.5 py-0.5 rounded-full bg-[var(--accent)]/15 text-[var(--accent)] cursor-pointer hover:bg-[var(--accent)]/25 transition-colors"
            data-no-drag
            onClick={() => openUrl(updateInfo.url)}
            title={t('app.update.title', { version: updateInfo.version })}
          >
            {t('app.update.badge', { version: updateInfo.version })}
          </span>
        )}
        <div className="w-px h-3.5 bg-[var(--border-default)]" />
        <div className="flex items-center gap-3 text-[var(--text-muted)]" data-no-drag>
          <span className="cursor-pointer hover:text-[var(--text-primary)] transition-colors duration-150" onClick={() => { setConfigPage(undefined); setConfigOpen(true); }}>{t('app.menu.settings')}</span>
          <span className="cursor-pointer hover:text-[var(--text-primary)] transition-colors duration-150" onClick={() => setSshOpen(true)}>SSH</span>
          <span className="cursor-pointer hover:text-[var(--text-primary)] transition-colors duration-150" onClick={() => setCcConnectOpen(true)}>{t('app.menu.connect')}</span>
        </div>
        <div className="flex-1" />
      </div>

      <div className="flex-1 overflow-hidden flex">
        {/* Activity Bar — 常驻最左侧 */}
        {configLoaded && <ActivityBar />}

        {/* 主内容区域 — Allotment 可拖拽 */}
        {configLoaded ? <Allotment
          defaultSizes={config.layoutSizes ?? [200, 280, 1000]}
          onChange={saveLayoutSizes}
        >
          {/* 左栏：Projects + Sessions */}
          <Allotment.Pane minSize={140} maxSize={350} visible={leftColumnVisible}>
            <ProjectList />
          </Allotment.Pane>

          {/* 中栏：FileTree + Git */}
          <Allotment.Pane minSize={100} visible={middleColumnVisible}>
            <Allotment
              vertical
              defaultSizes={config.middleColumnSizes ?? [300, 200]}
              onChange={saveMiddleColumnSizes}
            >
              <Allotment.Pane minSize={150} visible={config.filesVisible}>
                <FileTree />
              </Allotment.Pane>
              <Allotment.Pane minSize={36} visible={config.gitVisible}>
                <GitHistory />
              </Allotment.Pane>
            </Allotment>
          </Allotment.Pane>

          {/* 右栏：Terminal */}
          <Allotment.Pane>
            <div className="relative h-full">
              {terminalProjectIds.map((projectId) => {
                const project = config.projects.find((p) => p.id === projectId);
                if (!project) return null;
                return (
                  <div
                    key={project.id}
                    className="absolute inset-0"
                    style={{ display: project.id === activeProjectId ? 'block' : 'none' }}
                  >
                    <TerminalArea
                      projectId={project.id}
                      projectPath={project.path}
                    />
                  </div>
                );
              })}
              {config.projects.length === 0 && (
                <div className="h-full bg-[var(--bg-terminal)] flex items-center justify-center text-[var(--text-muted)] text-sm">
                  {t('app.emptyState')}
                </div>
              )}
            </div>
          </Allotment.Pane>
        </Allotment> : null}
      </div>
      <SettingsModal open={configOpen} onClose={() => setConfigOpen(false)} initialPage={configPage} />
      <SshModal open={sshOpen} onClose={() => setSshOpen(false)} />
      <CcConnectModal open={ccConnectOpen} onClose={() => setCcConnectOpen(false)} />
      <SearchModal open={searchModalOpen} onClose={() => setSearchModalOpen(false)} />
      <CcConnectDashboard
        open={ccDashboardOpen}
        onClose={closeCcDashboard}
        deepLink={ccDashboardDeepLink || undefined}
      />
      <ToastContainer />
    </div>
  );
}
