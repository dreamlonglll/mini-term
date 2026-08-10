const assert = require('node:assert/strict');

const { isAiCompletion, isAttentionCause, isAttentionRise } = require('../.tmp-tests/utils/aiCompletion.js');

// Stop 是唯一表示"任务做完了"的 hook 事件
{
  assert.equal(isAiCompletion('ai-working', 'ai-idle', 'Stop'), true);
}

// 权限请求同样落到 ai-idle,但那是"等你批",不是完成 —— 回归:审批框一弹就播报完成
{
  assert.equal(isAiCompletion('ai-working', 'ai-idle', 'PermissionRequest'), false);
}

// 其余落到 ai-idle 的 hook 事件同样不是完成
{
  for (const cause of ['Notification', 'Elicitation', 'SessionStart']) {
    assert.equal(isAiCompletion('ai-working', 'ai-idle', cause), false, cause);
  }
}

// 无 cause = 后端 monitor 轮询算出的下降沿,只可能来自无 hook 的降级路径
// (WSL / SSH / hook 关闭)。那条路径没有事件名,下降沿是唯一的完成信号,必须放行
{
  assert.equal(isAiCompletion('ai-working', 'ai-idle', undefined), true);
}

// 不是 ai-working → ai-idle 的下降沿,一律不算完成(哪怕 cause 是 Stop)
{
  assert.equal(isAiCompletion('ai-idle', 'ai-idle', 'Stop'), false);
  assert.equal(isAiCompletion('idle', 'ai-idle', 'Stop'), false);
  assert.equal(isAiCompletion('ai-working', 'idle', 'Stop'), false);
  assert.equal(isAiCompletion('ai-working', 'error', 'Stop'), false);
  assert.equal(isAiCompletion('ai-working', 'ai-working', 'Stop'), false);
}

// SessionEnd 直推的是 idle 而非 ai-idle,不构成完成(pane 退出不该播完成音)
{
  assert.equal(isAiCompletion('ai-working', 'idle', 'SessionEnd'), false);
}

// StopFailure = 这轮因 API 错误没跑完;Interrupt = 用户自己按 Esc 打断。
// 两者都把状态收回 ai-idle(否则徽章确定性地卡在 ai-working),但都不是完成
{
  assert.equal(isAiCompletion('ai-working', 'ai-idle', 'StopFailure'), false);
  assert.equal(isAiCompletion('ai-working', 'ai-idle', 'Interrupt'), false);
}

// 停摆兜底(后端 process_monitor.rs):ai-working 静默 10s 后把徽章摘下来。
// Stall 收到 ai-idle、StallExit 直落 idle,两者都不是"任务做完了",
// 播报即误报 —— 这正是 v0.9.3 删掉旧兜底的原因,新兜底靠成因把它挡在这里
{
  assert.equal(isAiCompletion('ai-working', 'ai-idle', 'Stall'), false);
  assert.equal(isAiCompletion('ai-working', 'idle', 'StallExit'), false);
  assert.equal(isAttentionCause('Stall'), false);
  assert.equal(isAttentionCause('StallExit'), false);
}

// 新补的工作中事件即便碰巧构成下降沿也不是完成
{
  for (const cause of ['PostToolUseFailure', 'PostToolBatch', 'PermissionDenied', 'ElicitationResult']) {
    assert.equal(isAiCompletion('ai-working', 'ai-idle', cause), false, cause);
  }
}

// 托盘黄灯的成因集,必须与后端 hook_server.rs 的 is_attention_cause 同集
{
  for (const cause of ['PermissionRequest', 'Elicitation', 'StopFailure']) {
    assert.equal(isAttentionCause(cause), true, cause);
  }
  // 完成、打断、以及黄灯的熄灭路径都不点黄灯
  for (const cause of ['Stop', 'Interrupt', 'Notification', 'SessionStart', 'SessionEnd',
    'PermissionDenied', 'ElicitationResult', 'UserPromptSubmit', undefined]) {
    assert.equal(isAttentionCause(cause), false, String(cause));
  }
}

// 待确认提醒只在上升沿响:黄灯没亮 + attention 类成因 = 提醒
{
  for (const cause of ['PermissionRequest', 'Elicitation', 'StopFailure']) {
    assert.equal(isAttentionRise(false, cause), true, cause);
    assert.equal(isAttentionRise(undefined, cause), true, cause);
  }
}

// 黄灯已亮着,后续 attention 事件不再响 —— 后端把 attention 类成因排除在去重之外
// (同一轮第二次授权请求不能被吞),所以同一次待确认完全可能连推多条
{
  assert.equal(isAttentionRise(true, 'PermissionRequest'), false);
  assert.equal(isAttentionRise(true, 'Elicitation'), false);
  // 用户对该 pane 键入会把 attention 清掉,下一次请求于是重新构成上升沿
  assert.equal(isAttentionRise(false, 'PermissionRequest'), true);
}

// 非 attention 成因一律不提醒,哪怕黄灯此前没亮 —— 完成/打断/停摆兜底各有各的路径
{
  for (const cause of ['Stop', 'Interrupt', 'Notification', 'SessionStart', 'SessionEnd',
    'Stall', 'StallExit', 'PermissionDenied', 'ElicitationResult', undefined]) {
    assert.equal(isAttentionRise(false, cause), false, String(cause));
  }
}

console.log('aiCompletion tests passed');
