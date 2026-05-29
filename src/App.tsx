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
import { CcConnectStatusDot } from './components/CcConnectStatusDot';
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
import type { AppConfig, PtyStatusChangePayload, PtyExitPayload, PaneStatus, CcConnectStatus } from './types';

export function App() {
  const [configLoaded, setConfigLoaded] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [configPage, setConfigPage] = useState<SettingsPage | undefined>(undefined);
  const [sshOpen, setSshOpen] = useState(false);
  const ccConnectStatus = useAppStore((s) => s.ccConnectStatus);
  const ccRunning = ccConnectStatus?.running ?? false;
  const ccDashboardOpen = useAppStore((s) => s.ccDashboardOpen);
  const ccDashboardDeepLink = useAppStore((s) => s.ccDashboardDeepLink);
  const openCcDashboard = useAppStore((s) => s.openCcDashboard);
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

      // cc-connect autoStart:首次 probe 发现未运行时尝试 spawn(配置过且勾选了 autoStart)
      const ccCfg = cfg.ccConnect;
      if (ccCfg?.autoStart && ccCfg.exePath?.trim()) {
        invoke<CcConnectStatus>('cc_connect_probe', {
          configPath: ccCfg.configPath || undefined,
        }).then((status) => {
          useAppStore.getState().setCcConnectStatus(status);
          if (!status.running) {
            return invoke<number>('cc_connect_start', {
              exePath: ccCfg.exePath,
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

  // cc-connect 状态 5s 轮询(失焦时暂停节省 CPU);仅在用户配置过时启动
  useCcConnectProbe(configLoaded ? config.ccConnect : undefined);

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
        message: `已检测到 WSL 项目,使用 wsl.exe 启动终端 (${payload.unixPath})`,
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
      const confirmed = await ask('确定要关闭 Mini-Term 吗？', { title: '关闭确认', kind: 'warning' });
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
            title={`新版本 ${updateInfo.version} 可用，点击前往下载`}
          >
            新版本 {updateInfo.version}
          </span>
        )}
        <div className="w-px h-3.5 bg-[var(--border-default)]" />
        <div className="flex items-center gap-3 text-[var(--text-muted)]" data-no-drag>
          <span className="cursor-pointer hover:text-[var(--text-primary)] transition-colors duration-150" onClick={() => { setConfigPage(undefined); setConfigOpen(true); }}>设置</span>
          <span className="cursor-pointer hover:text-[var(--text-primary)] transition-colors duration-150" onClick={() => setSshOpen(true)}>SSH</span>
        </div>
        <div className="w-px h-3.5 bg-[var(--border-default)]" />
        <CcConnectStatusDot
          onOpenSettings={() => { setConfigPage('cc-connect'); setConfigOpen(true); }}
          onOpenDashboard={() => openCcDashboard()}
        />
        {config.ccConnect && (
          <span
            data-no-drag
            className={`text-[10px] transition-colors duration-150 ${
              ccRunning
                ? 'text-[var(--text-muted)] hover:text-[var(--accent)] cursor-pointer'
                : 'text-[var(--text-muted)]/50 cursor-not-allowed'
            }`}
            onClick={() => {
              if (ccRunning) openCcDashboard();
            }}
            title={ccRunning ? '打开 cc-connect Dashboard' : '需要先启动 cc-connect'}
          >
            Dashboard
          </span>
        )}
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
                  请先在左栏添加项目
                </div>
              )}
            </div>
          </Allotment.Pane>
        </Allotment> : null}
      </div>
      <SettingsModal open={configOpen} onClose={() => setConfigOpen(false)} initialPage={configPage} />
      <SshModal open={sshOpen} onClose={() => setSshOpen(false)} />
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
