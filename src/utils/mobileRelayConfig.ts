/**
 * mobileRelay 配置块的部分更新助手。
 *
 * 「移动端」面板有两处各改一半:地址 + 密钥在上面,启动器在下面。两处都得保证
 * 别把没碰的字段写丢,所以补全逻辑收在这一个地方。
 *
 * `load_config` 的迁移已保证该块存在(整块缺失时后端补上预置启动器),所以
 * `current` 为空只是理论上的兜底。此时 `launchers` 取空列表而**不是**重新塞回
 * 预置两条:凭空补预置会跟后端"用户删光是有意结果"的迁移规则打架。
 */
import type { MobileRelayConfig } from '../types';

export function withMobileRelayDefaults(
  current: MobileRelayConfig | undefined,
  patch: Partial<MobileRelayConfig>,
): MobileRelayConfig {
  return { relayUrl: '', desktopKey: '', launchers: [], ...current, ...patch };
}
