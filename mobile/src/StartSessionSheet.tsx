import { useState } from 'react';
import { startAiSession, useRelayStore } from './relay';
import { useT } from './i18n';
import type { MobileProject } from './protocol';

/**
 * 发起新 AI 会话的弹层:选项目 → 选启动器。
 *
 * 只有一条启动器时跳过第二步(少点一步)。SSH 远程项目与 WSL 根项目置灰并标注
 * "对话镜像不可用"——在那里开会话只能盲发指令、看不到回复。
 * 手机侧永远不自拟命令,传的只是启动器 id。
 */
export function StartSessionSheet({ onClose }: { onClose: () => void }) {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const launchers = useRelayStore((s) => s.launchers);
  const [picked, setPicked] = useState<MobileProject | null>(null);

  const launch = (project: MobileProject, launcherId: string) => {
    if (startAiSession(project.projectId, project.name, launcherId)) onClose();
  };

  const pickProject = (project: MobileProject) => {
    if (launchers.length === 1) {
      launch(project, launchers[0].id);
      return;
    }
    setPicked(project);
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="sheet-header">
          {picked ? (
            <button className="sheet-back" onClick={() => setPicked(null)}>
              ‹ {t('start.back')}
            </button>
          ) : (
            <span className="sheet-title">{t('start.pickProject')}</span>
          )}
          <button className="sheet-close" onClick={onClose}>
            {t('start.cancel')}
          </button>
        </div>

        {picked ? (
          <div className="sheet-body">
            <div className="sheet-subtitle">
              {picked.name} · {t('start.pickLauncher')}
            </div>
            {launchers.map((launcher) => (
              <button
                key={launcher.id}
                className="sheet-row"
                onClick={() => launch(picked, launcher.id)}
              >
                <span className="sheet-row-name">{launcher.name}</span>
                <span className="pane-chevron">›</span>
              </button>
            ))}
          </div>
        ) : (
          <div className="sheet-body">
            {projects.length === 0 ? (
              <div className="sheet-empty">{t('start.noProjects')}</div>
            ) : (
              projects.map((project) => (
                <button
                  key={project.projectId}
                  className="sheet-row"
                  disabled={!project.canStartSession}
                  onClick={() => pickProject(project)}
                >
                  <span className="sheet-row-name">{project.name}</span>
                  {project.canStartSession ? (
                    <span className="pane-chevron">›</span>
                  ) : (
                    <span className="sheet-row-note">{t('start.notSupported')}</span>
                  )}
                </button>
              ))
            )}
          </div>
        )}
      </div>
    </div>
  );
}
