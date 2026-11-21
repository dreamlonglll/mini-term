import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { startRelay } from './relay';
import './styles.css';

startRelay();

// 外壳缓存(public/sw.js):打开页面不等网络。只在生产构建注册 —— 开发服务器的
// 模块请求不该被缓存;非安全上下文(纯 http 的 IP 地址)浏览器本就不给注册,静默跳过
if (import.meta.env.PROD && 'serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  });
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
