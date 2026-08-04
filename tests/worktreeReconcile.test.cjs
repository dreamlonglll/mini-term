const assert = require('node:assert/strict');

const {
  collectWorktreeProbePaths,
  findStaleWorktreeProjects,
} = require('../.tmp-tests/utils/worktreeReconcile.js');

const main = { id: 'main', name: 'repo', path: 'D:\\repos\\app' };
const wt = { id: 'wt', name: 'app-feat', path: 'D:\\repos\\app-feat', parentProjectId: 'main' };
const wt2 = { id: 'wt2', name: 'app-fix', path: 'D:\\repos\\app-fix', parentProjectId: 'main' };
const topLevel = { id: 'top', name: 'other', path: 'D:\\repos\\other' };

// 探测集合 = 子项目路径 + 父项目路径,同一父项目只出现一次;顶层项目不参与
{
  const paths = collectWorktreeProbePaths([main, wt, wt2, topLevel]);
  assert.deepEqual(
    [...paths].sort(),
    ['D:\\repos\\app', 'D:\\repos\\app-feat', 'D:\\repos\\app-fix'],
  );
}

// 子项目目录消失、父目录仍在 → 判定失效
{
  const stale = findStaleWorktreeProjects([main, wt, wt2], ['D:\\repos\\app', 'D:\\repos\\app-fix']);
  assert.deepEqual(stale.map((p) => p.id), ['wt']);
}

// 子项目目录还在 → 不清理
{
  const stale = findStaleWorktreeProjects([main, wt], ['D:\\repos\\app', 'D:\\repos\\app-feat']);
  assert.deepEqual(stale, []);
}

// 父目录也消失(盘符拔出等整树消失) → 不清理,防误删
{
  const stale = findStaleWorktreeProjects([main, wt], []);
  assert.deepEqual(stale, []);
}

// 顶层项目目录消失 → 从不自动清理
{
  const stale = findStaleWorktreeProjects([main, topLevel], ['D:\\repos\\app']);
  assert.deepEqual(stale, []);
}

// SSH 远程项目(自身或父项目)不参与探测与清理
{
  const sshParent = { id: 'sp', name: 'r', path: '/srv/app', sshConnectionId: 'c1' };
  const sshChild = { id: 'sc', name: 'r-wt', path: '/srv/app-wt', parentProjectId: 'sp', sshConnectionId: 'c1' };
  const localChildOfSsh = { id: 'lc', name: 'x', path: 'D:\\x', parentProjectId: 'sp' };
  assert.deepEqual(collectWorktreeProbePaths([sshParent, sshChild, localChildOfSsh]), []);
  assert.deepEqual(findStaleWorktreeProjects([sshParent, sshChild, localChildOfSsh], []), []);
}

// UNC(WSL)路径不参与:自身 UNC 或父项目 UNC 都跳过
{
  const uncParent = { id: 'up', name: 'wsl', path: '\\\\wsl$\\Ubuntu\\home\\a' };
  const uncChild = { id: 'uc', name: 'wsl-wt', path: '\\\\wsl$\\Ubuntu\\home\\a-wt', parentProjectId: 'up' };
  const localChildOfUnc = { id: 'lu', name: 'y', path: 'D:\\y', parentProjectId: 'up' };
  assert.deepEqual(collectWorktreeProbePaths([uncParent, uncChild, localChildOfUnc]), []);
  assert.deepEqual(findStaleWorktreeProjects([uncParent, uncChild, localChildOfUnc], []), []);
}

// parentProjectId 悬空(父项目已被删) → 不参与,交给 removeProject 的晋升逻辑
{
  const orphan = { id: 'o', name: 'o', path: 'D:\\o', parentProjectId: 'gone' };
  assert.deepEqual(collectWorktreeProbePaths([orphan]), []);
  assert.deepEqual(findStaleWorktreeProjects([orphan], []), []);
}
