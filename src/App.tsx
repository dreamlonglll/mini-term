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
import { ActivityBar } from './components/ActivityBar';
import { RightDrawer } from './components/RightDrawer';
import { SettingsModal, type SettingsPage } from './components/SettingsModal';
import { SshModal } from './components/SshModal';
import { MobileRelayModal } from './components/MobileRelayModal';
import { SearchModal } from './components/SearchModal';
import { ToastContainer } from './components/ToastContainer';
import { useTauriEvent } from './hooks/useTauriEvent';
import { useAiSubmitMarker } from './hooks/useAiSubmitMarker';
import { useMarkerHotkeys } from './hooks/useMarkerHotkeys';
import { useExternalFileDrop } from './hooks/useExternalFileDrop';
import { checkForUpdate, type ReleaseInfo } from './utils/updateChecker';
import { applyTheme } from './utils/themeManager';
import { applyUiFontFamily } from './utils/fontManager';
import { markAiPty, updateAllTerminalThemes } from './utils/terminalCache';
import { includeActiveProject } from './utils/projectKeepAlive';
import { useT } from './i18n';
import type { AppConfig, PtyStatusChangePayload, PtyExitPayload, PaneStatus, MobileRelayStatusPayload } from './types';

export function App() {
  const t = useT();
  const [configLoaded, setConfigLoaded] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [configPage, setConfigPage] = useState<SettingsPage | undefined>(undefined);
  const [sshOpen, setSshOpen] = useState(false);
  const [mobileOpen, setMobileOpen] = useState(false);
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
    });
  }, []);

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

  // Ctrl+Shift+F 打开/关闭搜索弹窗(内容搜索是本地 ripgrep 链路,SSH 远程项目不支持)
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key === 'F') {
        e.preventDefault();
        const { searchModalOpen: isOpen, setSearchModalOpen: setOpen, config: cfg, activeProjectId: pid } = useAppStore.getState();
        const activeProject = cfg.projects.find((p) => p.id === pid);
        if (!isOpen && activeProject?.sshConnectionId) return; // 远程项目:不打开
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

  // 启动时获取版本号：写进原生窗口标题（原自定义标题栏已移除），并检查更新
  useEffect(() => {
    getVersion().then((ver) => {
      getCurrentWindow().setTitle(`Mini-Term v${ver}`).catch(() => {});
      checkForUpdate(ver).then((release) => {
        if (release) setUpdateInfo(release);
      }).catch(() => {});
    });
  }, []);

  useTauriEvent<PtyStatusChangePayload>('pty-status-change', useCallback((payload) => {
    markAiPty(payload.ptyId, payload.status === 'ai-working' || payload.status === 'ai-idle');
    updatePaneStatusByPty(payload.ptyId, payload.status as PaneStatus);
  }, [updatePaneStatusByPty]));

  // 中转连接状态:后端长连状态机推送,写入 store 供设置页「移动端」区域实时展示
  useTauriEvent<MobileRelayStatusPayload>('mobile-relay-status', useCallback((payload) => {
    useAppStore.getState().setMobileRelayStatus(payload);
  }, []));

  useTauriEvent<PtyExitPayload>('pty-exit', useCallback((payload) => {
    // 登记已退出的 PTY:远程项目 pane 据此叠加「连接已断开,点击重连」覆盖层
    // (不区分用户主动 exit 与异常断线);本地 pane 不消费该集合,登记无副作用。
    useAppStore.getState().markPtyExited(payload.ptyId);
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

  // 右侧抽屉宽度：拖拽结束时持久化一次
  const persistRightDrawerWidth = useCallback((width: number) => {
    const cfg = useAppStore.getState().config;
    const newConfig = { ...cfg, rightDrawerWidth: width };
    setConfig(newConfig);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  return (
    <div className="flex flex-col h-full">
      <div className="flex-1 overflow-hidden flex">
        {/* Icon 栏 — 常驻最左侧 */}
        {configLoaded && (
          <ActivityBar
            onOpenSettings={() => { setConfigPage(undefined); setConfigOpen(true); }}
            onOpenSsh={() => setSshOpen(true)}
            onOpenMobile={() => setMobileOpen(true)}
            updateVersion={updateInfo?.version ?? null}
            onOpenUpdate={() => { if (updateInfo) openUrl(updateInfo.url); }}
          />
        )}

        {/* 主内容区域 — Allotment 可拖拽 + 右侧悬浮抽屉 */}
        {configLoaded ? (
          <div className="relative flex-1 overflow-hidden">
            <Allotment
              defaultSizes={config.layoutSizes?.length === 2 ? config.layoutSizes : [520, 1000]}
              onChange={saveLayoutSizes}
            >
              {/* 中间栏：Projects(上) + Files(下) */}
              <Allotment.Pane minSize={180} maxSize={600} visible={config.middleColumnVisible}>
                <Allotment
                  vertical
                  defaultSizes={config.middleColumnSizes?.length === 2 ? config.middleColumnSizes : [320, 380]}
                  onChange={saveMiddleColumnSizes}
                >
                  <Allotment.Pane minSize={100}>
                    <ProjectList />
                  </Allotment.Pane>
                  <Allotment.Pane minSize={120}>
                    <FileTree />
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
            </Allotment>

            {/* 右侧悬浮抽屉：Sessions / Git（互斥单抽屉,浮在终端之上） */}
            <RightDrawer
              initialWidth={config.rightDrawerWidth ?? 340}
              onResizeEnd={persistRightDrawerWidth}
            />
          </div>
        ) : null}
      </div>
      <SettingsModal open={configOpen} onClose={() => setConfigOpen(false)} initialPage={configPage} />
      <SshModal open={sshOpen} onClose={() => setSshOpen(false)} />
      <MobileRelayModal
        open={mobileOpen}
        onClose={() => setMobileOpen(false)}
        onOpenSettings={() => { setConfigPage('mobile'); setConfigOpen(true); }}
      />
      <SearchModal open={searchModalOpen} onClose={() => setSearchModalOpen(false)} />
      <ToastContainer />
    </div>
  );
}
