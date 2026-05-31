import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { t } from '../i18n';
import type { CcConnectConfig, CcConnectStatus } from '../types';

const POLL_INTERVAL_MS = 5000;

/**
 * cc-connect 状态轮询。
 *
 * - 每 5s 调一次 cc_connect_probe,结果写入 store.ccConnectStatus
 * - 窗口失焦(document.hidden = true)时暂停轮询省 CPU
 * - 仅当用户在设置里配置过 ccConnect 时才启用(传 undefined 时 noop)
 * - cc_connect_probe 即使后端 HTTP 失败也会返回带 diagnostic 的 status 对象,
 *   只有 invoke 整体失败才会进 catch 分支
 */
export function useCcConnectProbe(ccConfig: CcConnectConfig | undefined): void {
  const setCcConnectStatus = useAppStore((s) => s.setCcConnectStatus);
  const configPath = ccConfig?.configPath;
  const enabled = ccConfig !== undefined;

  useEffect(() => {
    if (!enabled) {
      setCcConnectStatus(null);
      return;
    }

    let disposed = false;
    let timer: ReturnType<typeof setInterval> | null = null;

    const probeOnce = () => {
      invoke<CcConnectStatus>('cc_connect_probe', {
        configPath: configPath || undefined,
      })
        .then((status) => {
          if (!disposed) setCcConnectStatus(status);
        })
        .catch(() => {
          // invoke 层面失败(极少见,通常是后端崩了),给一个红 ⚠ 兜底
          if (!disposed) {
            setCcConnectStatus({
              running: false,
              port: 9820,
              diagnostic: t('ccProbe.invokeFailed'),
            });
          }
        });
    };

    const startTimer = () => {
      if (timer !== null) return;
      probeOnce();
      timer = setInterval(probeOnce, POLL_INTERVAL_MS);
    };

    const stopTimer = () => {
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
    };

    const handleVisibilityChange = () => {
      if (document.hidden) {
        stopTimer();
      } else {
        startTimer();
      }
    };

    if (!document.hidden) {
      startTimer();
    }
    document.addEventListener('visibilitychange', handleVisibilityChange);

    return () => {
      disposed = true;
      stopTimer();
      document.removeEventListener('visibilitychange', handleVisibilityChange);
    };
  }, [enabled, configPath, setCcConnectStatus]);
}
