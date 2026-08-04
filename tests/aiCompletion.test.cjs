const assert = require('node:assert/strict');

const { isAiCompletion } = require('../.tmp-tests/utils/aiCompletion.js');

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

console.log('aiCompletion tests passed');
