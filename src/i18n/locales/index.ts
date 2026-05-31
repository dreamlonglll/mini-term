/**
 * 各命名空间翻译字典的聚合入口（i18n 迁移收尾阶段统一生成）。
 *
 * 约定：每个源文件对应一个命名空间文件 `src/i18n/locales/<ns>.ts`，
 * 形如 `export const <ns> = { zh: {...}, en: {...} } as const;`
 * 此处聚合为 `dicts.zh[<ns>]` / `dicts.en[<ns>]`，组件通过 `t('<ns>.<key>')` 访问。
 */
import { app } from './app';
import { settings } from './settings';
import { ccConnectModal } from './ccConnectModal';
import { ccDashboard } from './ccDashboard';
import { projectList } from './projectList';
import { fileTree } from './fileTree';
import { sshModal } from './sshModal';
import { sshAssoc } from './sshAssoc';
import { envVars } from './envVars';
import { terminal } from './terminal';
import { terminalArea } from './terminalArea';
import { gitHistory } from './gitHistory';
import { gitHistoryContent } from './gitHistoryContent';
import { gitChanges } from './gitChanges';
import { search } from './search';
import { sessionList } from './sessionList';
import { sessionViewer } from './sessionViewer';
import { fileViewer } from './fileViewer';
import { commitDiff } from './commitDiff';
import { diffModal } from './diffModal';
import { paneGroup } from './paneGroup';
import { markerList } from './markerList';
import { toast } from './toast';
import { time } from './time';
import { prompt } from './prompt';
import { externalLink } from './externalLink';
import { updateChecker } from './updateChecker';
import { ccConnectImport } from './ccConnectImport';
import { ccProbe } from './ccProbe';

type Dict = Record<string, unknown>;

export const dicts: { zh: Dict; en: Dict } = {
  zh: {
    app: app.zh,
    settings: settings.zh,
    ccConnectModal: ccConnectModal.zh,
    ccDashboard: ccDashboard.zh,
    projectList: projectList.zh,
    fileTree: fileTree.zh,
    sshModal: sshModal.zh,
    sshAssoc: sshAssoc.zh,
    envVars: envVars.zh,
    terminal: terminal.zh,
    terminalArea: terminalArea.zh,
    gitHistory: gitHistory.zh,
    gitHistoryContent: gitHistoryContent.zh,
    gitChanges: gitChanges.zh,
    search: search.zh,
    sessionList: sessionList.zh,
    sessionViewer: sessionViewer.zh,
    fileViewer: fileViewer.zh,
    commitDiff: commitDiff.zh,
    diffModal: diffModal.zh,
    paneGroup: paneGroup.zh,
    markerList: markerList.zh,
    toast: toast.zh,
    time: time.zh,
    prompt: prompt.zh,
    externalLink: externalLink.zh,
    updateChecker: updateChecker.zh,
    ccConnectImport: ccConnectImport.zh,
    ccProbe: ccProbe.zh,
  },
  en: {
    app: app.en,
    settings: settings.en,
    ccConnectModal: ccConnectModal.en,
    ccDashboard: ccDashboard.en,
    projectList: projectList.en,
    fileTree: fileTree.en,
    sshModal: sshModal.en,
    sshAssoc: sshAssoc.en,
    envVars: envVars.en,
    terminal: terminal.en,
    terminalArea: terminalArea.en,
    gitHistory: gitHistory.en,
    gitHistoryContent: gitHistoryContent.en,
    gitChanges: gitChanges.en,
    search: search.en,
    sessionList: sessionList.en,
    sessionViewer: sessionViewer.en,
    fileViewer: fileViewer.en,
    commitDiff: commitDiff.en,
    diffModal: diffModal.en,
    paneGroup: paneGroup.en,
    markerList: markerList.en,
    toast: toast.en,
    time: time.en,
    prompt: prompt.en,
    externalLink: externalLink.en,
    updateChecker: updateChecker.en,
    ccConnectImport: ccConnectImport.en,
    ccProbe: ccProbe.en,
  },
};
