import { useAppStore } from '../store';
import { useT } from '../i18n';

export function ToastContainer() {
  const t = useT();
  const notifications = useAppStore((s) => s.notifications);
  const dismissNotification = useAppStore((s) => s.dismissNotification);
  const pauseNotification = useAppStore((s) => s.pauseNotification);
  const setActiveProject = useAppStore((s) => s.setActiveProject);

  // 最多同时渲染 5 个，超出排队
  const visible = notifications.slice(0, 5);

  if (visible.length === 0) return null;

  return (
    <div className="toast-stack" role="status" aria-live="polite">
      {visible.map((n) => {
        const isWslInfo = n.kind === 'wsl-info';
        // 移动端发起的新会话:带项目跳转(点一下就能接管),图标用信息态区分
        const isMobileSession = n.kind === 'mobile-session';
        // 远程粘贴上传失败:错误态图标,点击仅关闭(项目就在眼前,不需要跳转)
        const isPasteError = n.kind === 'paste-error';
        const isInfo = isWslInfo || isMobileSession;
        return (
          <div
            key={n.id}
            className="toast-card"
            // 悬停暂停自动消失:5s 硬性倒计时会在鼠标正要点它时把它抽走
            onMouseEnter={() => pauseNotification(n.id, true)}
            onMouseLeave={() => pauseNotification(n.id, false)}
            onClick={() => {
              // WSL 信息提示 / 粘贴失败不带项目跳转语义,点击仅 dismiss
              if (!isWslInfo && !isPasteError) {
                setActiveProject(n.projectId);
              }
              dismissNotification(n.id);
            }}
          >
            <div
              className={
                isPasteError
                  ? 'toast-icon toast-icon--error'
                  : isInfo
                    ? 'toast-icon toast-icon--info'
                    : 'toast-icon'
              }
            >
              {isPasteError ? '!' : isInfo ? 'i' : '✓'}
            </div>
            <div className="toast-body">
              <div className="toast-name">{n.projectName}</div>
              <div className="toast-desc">
                {isInfo || isPasteError ? (n.message ?? '') : t('toast.aiDone')}
              </div>
            </div>
            <button
              type="button"
              className="toast-close"
              aria-label={t('toast.dismiss')}
              title={t('toast.dismiss')}
              onClick={(e) => {
                e.stopPropagation();
                dismissNotification(n.id);
              }}
            >×</button>
          </div>
        );
      })}
    </div>
  );
}
