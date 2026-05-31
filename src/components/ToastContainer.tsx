import { useAppStore } from '../store';
import { useT } from '../i18n';

export function ToastContainer() {
  const t = useT();
  const notifications = useAppStore((s) => s.notifications);
  const dismissNotification = useAppStore((s) => s.dismissNotification);
  const setActiveProject = useAppStore((s) => s.setActiveProject);

  // 最多同时渲染 5 个，超出排队
  const visible = notifications.slice(0, 5);

  if (visible.length === 0) return null;

  return (
    <div className="toast-stack">
      {visible.map((n) => {
        const isWslInfo = n.kind === 'wsl-info';
        return (
          <div
            key={n.id}
            className="toast-card"
            onClick={() => {
              // WSL 信息提示不带项目跳转语义,点击仅 dismiss
              if (!isWslInfo) {
                setActiveProject(n.projectId);
              }
              dismissNotification(n.id);
            }}
          >
            <div className={isWslInfo ? 'toast-icon toast-icon--info' : 'toast-icon'}>
              {isWslInfo ? 'i' : '✓'}
            </div>
            <div className="toast-body">
              <div className="toast-name">{n.projectName}</div>
              <div className="toast-desc">
                {isWslInfo ? (n.message ?? '') : t('toast.aiDone')}
              </div>
            </div>
            <div
              className="toast-close"
              onClick={(e) => {
                e.stopPropagation();
                dismissNotification(n.id);
              }}
            >×</div>
          </div>
        );
      })}
    </div>
  );
}
