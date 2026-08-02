import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import { markStartup } from './utils/startupTrace';
// 本地字体（woff2 随安装包分发）：启动路径零网络请求，离线可用（含 xterm 等宽字体）
import '@fontsource-variable/dm-sans';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';
import '@fontsource/jetbrains-mono/600.css';
import './styles.css';
import './fluent2.css';

// 主 chunk（含全部静态依赖模块）加载 + 解析 + 执行完毕的时刻
markStartup('main chunk exec done');

// 禁用 WebView 默认右键菜单
document.addEventListener('contextmenu', (e) => {
  e.preventDefault();
});

// 禁用 WebView 默认快捷键（F5 刷新、F7 光标浏览、F12 开发者工具、Ctrl+R 等）。
//
// 只拦真正有 WebView 默认行为的那几个键：早先是 F1–F12 一律 preventDefault，
// 把整排功能键都变成了不可用键位（应用自己想用 F2 重命名也用不了），
// 而 TUI 程序（vim/htop/midnight commander）恰恰重度依赖 F 键，全拦等于把它们
// 送不进终端。
const BLOCKED_FN_KEYS = new Set(['F1', 'F3', 'F5', 'F6', 'F7', 'F11', 'F12']);

/**
 * WebView 自带、在这个应用里一概没有意义的浏览器级快捷键。
 *
 * 必须**无条件**拦掉，不能只在应用自己处理某个键时才拦：否则弹窗打开、
 * 或应用因故没接管这个键时，按下 Ctrl+F 会弹出 WebView2 的网页查找条
 * （真的会在终端文字上打黄底高亮），Ctrl+P 会弹打印对话框。
 *
 * preventDefault 不影响终端收键：xterm 在 keydown 里自行计算并发送控制序列，
 * 不依赖浏览器的默认动作。所以 Ctrl+P（bash 的上一条历史）等仍能正常送进 PTY。
 */
const BLOCKED_BROWSER_KEYS = new Set([
  'KeyF', // 网页查找（应用自己的终端查找也是这个键）
  'KeyG', // 查找下一个 / 上一个
  'KeyP', // 打印
  'KeyS', // 保存网页
  'KeyO', // 打开文件
  'KeyU', // 查看源代码
  'KeyR', // 刷新
  'KeyJ', // 下载
]);

document.addEventListener('keydown', (e) => {
  if (BLOCKED_FN_KEYS.has(e.key)) {
    e.preventDefault();
    return;
  }
  if (!(e.ctrlKey || e.metaKey)) return;

  // Ctrl+Shift+I 开发者工具
  if (e.shiftKey && e.code === 'KeyI') {
    e.preventDefault();
    return;
  }
  if (BLOCKED_BROWSER_KEYS.has(e.code)) {
    e.preventDefault();
  }
});

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
