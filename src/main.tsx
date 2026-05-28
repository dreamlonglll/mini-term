import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import './styles.css';
import './fluent2.css';

// 禁用 WebView 默认右键菜单
document.addEventListener('contextmenu', (e) => {
  e.preventDefault();
});

// 禁用 WebView 默认快捷键（F5 刷新、F7、F12 开发者工具、Ctrl+R 等）
document.addEventListener('keydown', (e) => {
  // F1-F12（注意：必须排除单独的 'F'，否则 Shift+F 输入大写 F 会被误拦截）
  if (/^F([1-9]|1[0-2])$/.test(e.key)) {
    e.preventDefault();
    return;
  }
  // Ctrl+R / Ctrl+Shift+R 刷新
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'r') {
    e.preventDefault();
    return;
  }
  // Ctrl+Shift+I 开发者工具
  if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === 'i') {
    e.preventDefault();
    return;
  }
  // Ctrl+U 查看源代码
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'u') {
    e.preventDefault();
    return;
  }
});

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
