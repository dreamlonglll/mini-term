## 更新内容

### 修复

- **右键菜单右侧不再白多一条滚动条轨道** — 上一版给「统计面板的项目下拉」补滚动条时口径放得太宽，把文件树、终端、项目列表那些右键菜单也一并套了进去：它们统共十来项、离视口封顶差得远，压根滚不动，右侧却跟着多出一条轨道和一圈让位边。现在滚动条只留给下拉式菜单——即调用方主动封了高、真会溢出的那一类。

### 体验

- 判定「内容是否真溢出」的阈值从 0 放宽到 1px：容器高与内容高是分两路算出来的，行高带小数时两边取整会差出零点几像素，按 0 判会给一个根本滚不动的菜单挂上一条永远在的轨道。

## 下载

- **Windows x64（主要支持平台）** — `mini-term-gpui-v1.0.5-beta-windows-x64-setup.exe`（NSIS 安装包，用户级安装免管理员；装过旧版的默认原目录升级，且先卸载旧版再装而不是文件覆盖写）
- **macOS arm64** — `mini-term-gpui-v1.0.5-beta-macos-arm64.dmg`
  - 首次打开若提示 "is damaged and can't be opened"，是 Release 产物没有 Apple Developer ID 签名被 Gatekeeper 拦下，拖进 `/Applications` 后执行一次 `xattr -cr /Applications/Mini-Term.app` 即可
- **Linux x64** — `mini-term-gpui-v1.0.5-beta-linux-amd64.deb` 或 `mini-term-gpui-v1.0.5-beta-linux-x64.tar.gz`

> macOS / Linux 代码层面已支持，但可用性欠佳、未经充分打磨，欢迎提 Issue。

---

https://github.com/dreamlonglll/mini-term/compare/v1.0.4-beta...v1.0.5-beta
