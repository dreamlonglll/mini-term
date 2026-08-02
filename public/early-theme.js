(function () {
  // 启动埋点锚位:HTML 解析最早期(首个脚本)的时刻,由 startupTrace 统一上报
  window.__earlyThemeTs = Date.now();
  var t = localStorage.getItem('mini-term-theme');
  if (t === 'light' || t === 'dark') {
    document.documentElement.dataset.theme = t;
  }
  var bg = t === 'light' ? '#ffffff' : '#0e0d0b';
  document.documentElement.style.background = bg;
  document.body.style.background = bg;
})();
