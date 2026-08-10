import { useState, useEffect, useCallback, useRef, lazy, Suspense } from 'react';
import { Allotment } from 'allotment';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import { ask } from '@tauri-apps/plugin-dialog';
import { useAppStore, restoreLayout, flushLayoutToConfig, initExpandedDirs, flushExpandedDirsToConfig, flushProjectToConfig, persistConfig, setPaneAiSessionByPty, saveLayoutToConfig, syncTrayStatus, setWindowFocused, saveConfigToDisk, setConfigToken } from './store';
import { TerminalArea } from './components/TerminalArea';
import { ProjectList } from './components/ProjectList';
import { FileTree } from './components/FileTree';
import { ActivityBar } from './components/ActivityBar';
import { TitleBar } from './components/TitleBar';
import { RightDrawer } from './components/RightDrawer';
import type { SettingsPage } from './components/SettingsModal';
import { SshModal } from './components/SshModal';
import { SearchModal } from './components/SearchModal';
import { ToastContainer } from './components/ToastContainer';
import { FirstRunGuide } from './components/FirstRunGuide';
import { ProjectSwitcher } from './components/ProjectSwitcher';
import { TerminalSearchBar } from './components/TerminalSearchBar';
import { useTauriEvent } from './hooks/useTauriEvent';
import { useEverOpened } from './hooks/useOverlayMotion';
import { markStartup, flushStartupTrace } from './utils/startupTrace';
import { useAiSubmitMarker } from './hooks/useAiSubmitMarker';
import { useMarkerHotkeys } from './hooks/useMarkerHotkeys';
import { useGlobalHotkeys } from './hooks/useGlobalHotkeys';
import { useExternalFileDrop } from './hooks/useExternalFileDrop';
import { collectPanes } from './utils/layoutOps';
import { focusAttentionTarget } from './utils/attentionJump';
import { checkForUpdate, type ReleaseInfo } from './utils/updateChecker';
import { applyTheme } from './utils/themeManager';
import { applyUiFontFamily } from './utils/fontManager';
import { clearCustomTheme, loadAndApplyCustomTheme } from './utils/themePackManager';
import { markAiPty, updateAllTerminalThemes } from './utils/terminalCache';
import { includeActiveProject } from './utils/projectKeepAlive';
import { initMobileSessionSync } from './utils/mobileSessionSync';
import { handleMobileStartSession } from './utils/mobileStartSession';
import { useT } from './i18n';
import type { LoadedConfig, PtyAiSessionPayload, PtyStatusChangePayload, PtyExitPayload, PaneStatus, MobileRelayStatusPayload, MobileStartSessionPayload, MobileRenamePanePayload } from './types';

// 懒加载三个重弹窗（SettingsModal 自身体量 / MobileRelayModal 连带 qrcode / UsageStatsModal
// 连带 usage/ 成套组件与 react-markdown），首次打开才拉 chunk，不占启动关键路径
const SettingsModal = lazy(() => import('./components/SettingsModal').then((m) => ({ default: m.SettingsModal })));
const MobileRelayModal = lazy(() => import('./components/MobileRelayModal').then((m) => ({ default: m.MobileRelayModal })));
const UsageStatsModal = lazy(() => import('./components/usage/UsageStatsModal').then((m) => ({ default: m.UsageStatsModal })));

/**
 * 关窗前盘点还活着的 AI 会话（ai-working / ai-idle）：数量 + 给用户看的名字清单。
 * 只数 AI 会话——裸 shell 关掉不心疼，AI 会话被 kill 才是真损失。
 */
function collectLiveAiPanes(): { count: number; names: string[] } {
  const { projectStates, config } = useAppStore.getState();
  const names: string[] = [];
  for (const [projectId, ps] of projectStates) {
    if (!ps.layout) continue;
    const projectName = config.projects.find((p) => p.id === projectId)?.name ?? '';
    for (const pane of collectPanes(ps.layout)) {
      if (pane.ptyId === undefined) continue;
      if (pane.status !== 'ai-working' && pane.status !== 'ai-idle') continue;
      const label = pane.customTitle || pane.shellName;
      names.push(projectName ? `· ${projectName} / ${label}` : `· ${label}`);
    }
  }
  return { count: names.length, names };
}

// 启动埋点:App 首次进入 render 的时刻(StrictMode 双 render 只记第一次)
let firstRenderMarked = false;

export function App() {
  if (!firstRenderMarked) {
    firstRenderMarked = true;
    markStartup('App() first render');
  }
  const t = useT();
  const [configLoaded, setConfigLoaded] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [configPage, setConfigPage] = useState<SettingsPage | undefined>(undefined);
  const [sshOpen, setSshOpen] = useState(false);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [statsOpen, setStatsOpen] = useState(false);
  // 懒挂载门控：三个懒弹窗首次打开前不挂载（chunk 不拉）；之后常驻，退场动画照播
  const settingsEverOpened = useEverOpened(configOpen);
  const mobileEverOpened = useEverOpened(mobileOpen);
  const statsEverOpened = useEverOpened(statsOpen);
  const [switcherOpen, setSwitcherOpen] = useState(false);
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
    // 加载失败重试 3 次;仍失败则显示窗口并明确报错,绝不带着空白配置
    // 静默运行。写盘由令牌把关:load_config 成功才发放令牌,save_config
    // 必须携带当前令牌——空白页面/加载失败的页面天然没有写盘资格,
    // 防止空状态覆盖磁盘上的完整配置
    // 回收上一轮页面遗留的 PTY,必须早于任何 create_pty。
    //
    // WebView2 的 renderer 被 OOM 杀掉后会重载页面,而 PtyManager 活在主进程里
    // 毫发无损:重载后的前端按 savedLayout 恢复出的是**全新** pane(新 id、无
    // ptyId),再各自新建 PTY,旧的 shell/AI 进程与它们的线程就此再无人引用却
    // 继续运行 —— 崩一次就漏一整套,内存压力更大、下次崩得更快。
    // 首次启动时后端存量为空,这里是个 no-op。
    const reapOrphanPtys = async () => {
      try {
        const killed = await invoke<number[]>('kill_all_ptys');
        if (killed.length > 0) {
          console.warn(`[startup] 回收了 ${killed.length} 个上一轮页面遗留的 PTY:`, killed);
        }
      } catch (e) {
        // 回收失败不该挡住启动:代价只是这一轮继续带着孤儿跑
        console.error('[startup] 孤儿 PTY 回收失败:', e);
      }
    };

    const loadWithRetry = async (): Promise<LoadedConfig> => {
      markStartup('load_config invoke');
      let lastErr: unknown;
      for (let i = 0; i < 3; i++) {
        try {
          return await invoke<LoadedConfig>('load_config');
        } catch (e) {
          lastErr = e;
          await new Promise((r) => setTimeout(r, 500 * (i + 1)));
        }
      }
      throw lastErr;
    };
    reapOrphanPtys().then(loadWithRetry).then(({ config: cfg, token }) => {
      markStartup('load_config resolved');
      setConfigToken(token);
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
          newStates.set(p.id, { id: p.id, layout: null, status: 'idle' });
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

      // 外置主题异步加载：不进 show() 前关键路径；失败回落内置（上面已应用）
      if (cfg.customThemeId) {
        loadAndApplyCustomTheme(cfg.customThemeId)
          .then(() => updateAllTerminalThemes(cfg.terminalFollowTheme ?? true))
          .catch((e) => {
            console.error(`自定义主题 ${cfg.customThemeId} 加载失败，回落内置外观:`, e);
            // 失败点未必在读取阶段：applyCustomTheme 跑完之后的任何一步抛（终端
            // 配色应用是最可能的一处），CSS 变量、背景层、注入的 theme.css 与模块
            // 内的激活态就都已经挂上了。只清 config 会留下"配置说没启用、DOM 上
            // 却还挂着"的半死状态，getCustomTerminalTheme() 还会继续把坏配色发给
            // 每个新建终端。先把 DOM 与模块态整个清掉，再落回内置明暗基线。
            clearCustomTheme();
            applyTheme(cfg.theme ?? 'auto');
            // 运行时 config 里的 id 必须一并清掉：下面的 skin effect 按
            // customThemeId 收敛，id 还在就把 data-skin 强制置空，用户原本的
            // 内置皮肤在"回落"时一起没了，设置页还显示着这个包处于激活态。
            // 只改内存不落盘 —— 主题目录可能只是这次读不到（盘没挂载、文件正
            // 被替换），落盘会把用户的选择永久抹掉，下次启动就找不回来了。
            const cur = useAppStore.getState().config;
            if (cur.customThemeId) setConfig({ ...cur, customThemeId: undefined });
            updateAllTerminalThemes(cur.terminalFollowTheme ?? true);
          });
      }

      for (const p of cfg.projects) {
        if (p.savedLayout && p.savedLayout.tabs.length > 0) {
          restoreLayout(p.id, p.savedLayout, cfg);
        }
      }

      markStartup('config applied (layout restored)');
      setConfigLoaded(true);

      // 布局元数据恢复完成后显示窗口；终端进程由可见 pane 按需创建。
      const showWindow = () => {
        // 双 rAF 确保 React 首帧布局完成后再显示。
        requestAnimationFrame(() => requestAnimationFrame(() => {
          // 到这里 configLoaded 后的主界面首帧已渲染完成:
          // 本节点与上一节点的间隔 ≈ React 主界面首帧(含 xterm 首挂)耗时
          markStartup('show() call (main UI first frame done)');
          getCurrentWindow().show().then(() => flushStartupTrace());
        }));
      };
      showWindow();
    }).catch((e) => {
      // 配置加载彻底失败:显示窗口让用户看到明确报错,而不是一个"全新"的空应用
      getCurrentWindow().show();
      console.error('load_config 失败:', e);
      alert(t('app.configLoadFailed', { detail: String(e) }));
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

  // 主题变化时应用新主题；外置皮肤激活时 data-theme 由皮肤 appearance 定死
  useEffect(() => {
    if (config.customThemeId) return;
    applyTheme(config.theme ?? 'auto');
  }, [config.theme, config.customThemeId]);

  // 皮肤变化时应用；自定义主题激活时 data-skin 置空
  useEffect(() => {
    const skin = config.skin ?? 'none';
    document.documentElement.dataset.skin =
      config.customThemeId || skin === 'none' ? '' : skin;
    updateAllTerminalThemes(config.terminalFollowTheme);
  }, [config.skin, config.customThemeId]);

  // 主题包热重载（改主题文件即重应用）后联动刷新终端配色
  useEffect(() => {
    const onReload = () => {
      updateAllTerminalThemes(useAppStore.getState().config.terminalFollowTheme ?? true);
    };
    window.addEventListener('custom-theme-reloaded', onReload);
    return () => window.removeEventListener('custom-theme-reloaded', onReload);
  }, []);

  // 启动时获取版本号：喂给自定义标题栏显示，同时写进原生窗口标题 ——
  // 窗口虽已无边框，任务栏悬停预览与 Alt+Tab 仍读这个标题
  useEffect(() => {
    getVersion().then((ver) => {
      setCurrentVersion(ver);
      getCurrentWindow().setTitle(`Mini-Term v${ver}`).catch(() => {});
      checkForUpdate(ver).then((release) => {
        if (release) setUpdateInfo(release);
      }).catch(() => {});
    });
  }, []);

  useTauriEvent<PtyStatusChangePayload>('pty-status-change', useCallback((payload) => {
    markAiPty(payload.ptyId, payload.status === 'ai-working' || payload.status === 'ai-idle');
    updatePaneStatusByPty(payload.ptyId, payload.status as PaneStatus, payload.cause, payload.agent);
  }, [updatePaneStatusByPty]));

  // 菜单栏状态灯:焦点变化经 Tauri 原生事件上报(DOM focus 在点原生标题栏等
  // 场景不可靠,曾导致绿灯不灭)。聚焦 = 完成已读,绿灯熄灭;失焦状态供闪烁策略用
  useEffect(() => {
    const unlisten = getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      setWindowFocused(focused);
      if (focused) {
        useAppStore.getState().clearUnreadDone();
      }
      syncTrayStatus();
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 点状态栏图标 → 和标题栏状态灯同一套落点:切到项目并激活那个 pane。
  // 关掉「点击定位」时后端已经把窗口唤起了,这里就什么都不动,留在原地
  useTauriEvent<null>('tray-clicked', useCallback(() => {
    if (!(useAppStore.getState().config.trayClickFocus ?? true)) return;
    focusAttentionTarget();
  }, []));

  // 状态栏右键菜单点项目 → 切到该项目,并定位到它内部最该处理的那个 pane
  useTauriEvent<string>('tray-project-clicked', useCallback((projectId) => {
    const { config, setActiveProject } = useAppStore.getState();
    if (!config.projects.some((p) => p.id === projectId)) return;
    // 菜单是上一次推送的快照,点下去时那些 pane 可能已经安静了 —— 定位不到目标
    // 也要把项目切过去,不能让这一下没反应
    if (!focusAttentionTarget(projectId)) setActiveProject(projectId);
  }, []));

  // 托盘开关/上限配置变化时立即生效(签名含 enabled,重复调用被去重)
  const trayStatusEnabled = useAppStore((s) => s.config.trayStatusEnabled ?? true);
  const trayMaxProjects = useAppStore((s) => s.config.trayMaxProjects ?? 5);
  useEffect(() => {
    syncTrayStatus();
  }, [trayStatusEnabled, trayMaxProjects]);

  // hook 上报的 AI 会话身份 → 写进 pane 并防抖持久化,供重启后 resume 续接
  useTauriEvent<PtyAiSessionPayload>('pty-ai-session', useCallback((payload) => {
    const projectId = setPaneAiSessionByPty(payload.ptyId, {
      agent: payload.agent,
      sessionId: payload.sessionId,
      cwd: payload.cwd,
    });
    if (projectId) saveLayoutToConfig(projectId);
  }, []));

  // 中转连接状态:后端长连状态机推送,写入 store 供设置页「移动端」区域实时展示
  useTauriEvent<MobileRelayStatusPayload>('mobile-relay-status', useCallback((payload) => {
    useAppStore.getState().setMobileRelayStatus(payload);
  }, []));

  // 活跃 AI 会话结构同步:store 变化 → 后端 → 中转 → 移动端列表
  useEffect(() => {
    initMobileSessionSync();
  }, []);

  // 移动端远程发起新 AI 会话:在目标项目后台新开一个 tab 并拉起 AI CLI,
  // 不切当前项目/tab(远程操作不改动桌面上正在看的现场)
  useTauriEvent<MobileStartSessionPayload>('mobile-start-session', useCallback((payload) => {
    void handleMobileStartSession(payload);
  }, []));

  // 移动端改会话名:直接改布局里那个 pane 的 customTitle,桌面端 tab 栏同步显示。
  // 不回执——改完的新名字会随结构增量推回手机,那既是反馈也是真相。
  useTauriEvent<MobileRenamePanePayload>('mobile-rename-pane', useCallback((payload) => {
    useAppStore.getState().renamePaneById(payload.paneId, payload.title);
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
  useGlobalHotkeys({
    onOpenSettings: useCallback(() => { setConfigPage(undefined); setConfigOpen(true); }, []),
    onSwitchProject: useCallback(() => setSwitcherOpen(true), []),
  });

  // 关闭窗口：只在真的会毁掉什么时才拦一下。
  // 之前无条件弹确认，日常开关十几次全是噪音，用户学会的是「闭眼点确定」——
  // 那正好让确认框在唯一该起作用的时候（AI 正在跑）也失效。
  useEffect(() => {
    const appWindow = getCurrentWindow();
    const unlisten = appWindow.onCloseRequested(async (event) => {
      event.preventDefault();

      const live = collectLiveAiPanes();
      if (live.count > 0) {
        const confirmed = await ask(
          t('app.closeConfirm.messageWithSessions', {
            count: live.count,
            names: live.names.join('\n'),
          }),
          { title: t('app.closeConfirm.titleAi'), kind: 'warning' },
        );
        if (!confirmed) return;
      }

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
      saveConfigToDisk(newConfig);
    }, 500);
  }, [setConfig]);

  const saveMidTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const saveMiddleColumnSizes = useCallback((sizes: number[]) => {
    clearTimeout(saveMidTimer.current);
    saveMidTimer.current = setTimeout(() => {
      const cfg = useAppStore.getState().config;
      const newConfig = { ...cfg, middleColumnSizes: sizes };
      setConfig(newConfig);
      saveConfigToDisk(newConfig);
    }, 500);
  }, [setConfig]);

  // 右侧抽屉宽度：拖拽结束时持久化一次
  const persistRightDrawerWidth = useCallback((width: number) => {
    const cfg = useAppStore.getState().config;
    const newConfig = { ...cfg, rightDrawerWidth: width };
    setConfig(newConfig);
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  return (
    <div className="flex flex-col h-full">
      {/* 无边框窗口的自定义标题栏。不受 configLoaded 门控：配置加载失败时
          用户也得有地方能把窗口关掉 */}
      <TitleBar version={currentVersion} />
      <div className="flex-1 overflow-hidden flex">
        {/* Icon 栏 — 常驻最左侧 */}
        {configLoaded && (
          <ActivityBar
            onOpenSettings={() => { setConfigPage(undefined); setConfigOpen(true); }}
            onOpenSsh={() => setSshOpen(true)}
            onOpenMobile={() => setMobileOpen(true)}
            onOpenStats={() => setStatsOpen(true)}
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
                  {config.projects.length === 0 && <FirstRunGuide />}
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
      {/* 三个懒弹窗各自独立 Suspense：共享边界会让一个弹窗首次加载时把另一个已打开的闪没 */}
      {settingsEverOpened && (
        <Suspense fallback={null}>
          <SettingsModal open={configOpen} onClose={() => setConfigOpen(false)} initialPage={configPage} />
        </Suspense>
      )}
      <SshModal open={sshOpen} onClose={() => setSshOpen(false)} />
      {mobileEverOpened && (
        <Suspense fallback={null}>
          <MobileRelayModal open={mobileOpen} onClose={() => setMobileOpen(false)} />
        </Suspense>
      )}
      {statsEverOpened && (
        <Suspense fallback={null}>
          <UsageStatsModal open={statsOpen} onClose={() => setStatsOpen(false)} />
        </Suspense>
      )}
      <SearchModal open={searchModalOpen} onClose={() => setSearchModalOpen(false)} />
      <ProjectSwitcher open={switcherOpen} onClose={() => setSwitcherOpen(false)} />
      <TerminalSearchBar />
      <ToastContainer />
    </div>
  );
}
