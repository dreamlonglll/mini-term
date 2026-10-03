# 宣传片制作管线

README 顶部的宣传片(`docs/promo/`)由这里的脚本**实拍真实应用**生成:e2e 驱动 GPUI 版
mini-term 跑完一整套操作并截图,再把截图交给一张 HTML 时间轴逐帧合成、配乐、编码。
界面改版后重跑一遍即可更新视频,不用手工录屏剪辑。

README 顶部用的是 GitHub 原生视频播放器(可暂停 / 拖动进度):它只认网页上传得到的
`https://github.com/user-attachments/assets/…` 链接,没有接口可用。所以重做视频后还要手动一步:
把 `docs/promo/mini-term-promo.mp4` 拖进 GitHub 任意评论框(新建 issue 页面即可,不必提交),
等出现链接后替换 `README.md` 与 `README.en.md` 里的那一行 —— 链接必须**单独成行、前后空行**,
写进 HTML 标签里就不会渲染成播放器。免费账户的视频上限 10 MB,`build.sh` 的编码参数按这个压的。
播放器没有封面参数,开播前显示的就是视频第一帧,所以片头第 0 帧就是完整标题卡,改片头时别丢了这一点。

```
assets/ blue-hour/ morning-mist/  素材库:视频用到的 18 张 4K 实拍截图(WebP q92,入库),合成页只读这里
e2e/   make-projects.sh  造演示项目(本仓克隆 + 5 个不同技术栈的小样例,带分支合并历史与未提交改动)
       seed.py           直接写 config.db / layout.db:项目分组、三分屏布局、皮肤、命令库、SSH 连接,
                         外加一个月的 Claude / Codex 会话记录(给「使用统计」「AI 历史」出图)
       bin/claude|codex  AI CLI 替身:只打印预设剧本,并像真实 hook 一样 POST 到 mini-term 的 hook server
       scenario.sh       xdotool 场景脚本:三个会话分别停在「在跑 / 做完 / 等你批准」,后台项目完成弹 toast,
                         再依次截悬停预览、Git、AI 历史、用量统计、Markdown + Mermaid、搜索、命令库、项目切换、
                         SSH、设置页里点皮肤卡实时换肤
       e2e.sh            一键:建项目 → 种数据 → 起 Xvfb + mini-term → 跑场景 → 关应用
video/ compose.html + engine.js + scenes.js   时间轴(所有动画都是 t 的纯函数,可任意跳帧)
       render.mjs        Playwright 逐帧截图 → ffmpeg 母版
       frames.mjs        抽帧预览(调镜头 / 字幕时不必整片重渲)
       prepare-assets.sh 从一轮 e2e 截图里挑图、压成 WebP 写进素材库
       music.py          纯程序合成的氛围配乐(无采样、无版权素材)
build.sh                 全流程,产物写到 docs/promo/mini-term-promo.mp4
```

## 运行

只在 Linux 上跑(实测 Ubuntu 24.04,无显卡容器即可):

```bash
# 公共依赖(合成与编码)
sudo apt-get install -y ffmpeg fonts-noto-cjk fonts-jetbrains-mono fonts-inter
pip install numpy
npm i -g playwright && npx playwright install chromium   # 已有 Chromium 时可跳过下载

# 只改字幕 / 节奏 / 配乐:直接用素材库,新克隆的仓库即可,不必编译应用
tools/promo/build.sh --skip-e2e    # 约 15~20 分钟(逐帧渲染 74 秒视频 + 编码)

# 界面改版、要重新实拍:再装 e2e 依赖并先编出应用
sudo apt-get install -y xvfb mesa-vulkan-drivers xdotool xclip x11-utils imagemagick
cargo build -p mt-app              # 先有 target/debug/mini-term
tools/promo/build.sh               # 约 25 分钟:两套皮肤各跑一遍 e2e → 更新素材库 → 渲染
```

- 渲染走 Xvfb(3840×2160)+ `GPUI_X11_SCALE_FACTOR=2` + lavapipe 软件 Vulkan:4K 截图让视频里的推镜特写依然清晰;
  代价是软件渲染只有 1~2 帧/秒,所以视频用「截图 + 镜头运动」而不是直接录屏。
- 演示环境整个隔离:`env -i` 起应用、`HOME` 指向 `DEMO_HOME`(默认 `/home/dev`,画面里的路径就是它)、
  `MT_APP_DATA_DIR` 指向 `out/data-<皮肤>`,不会碰到本机的 mini-term 配置和 `~/.claude`。
- Playwright 装在项目外时,用 `PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs` 指给 `render.mjs`。

## 素材库

`assets/` 存的是**当前视频实际用到的那批截图**(拍于 v1.13.13-pre,2026-10-03),所以不重跑 e2e
也能原样重做视频。重新实拍时 `prepare-assets.sh` 会整批覆盖它们 —— 与 `docs/promo/` 的视频一起提交。
图标与片头片尾背景直接引用 `docs/icon.png`、`theme/blue-hour/background.jpg`;配乐由 `music.py`
固定随机种子生成,每次一致,不必存。

| 文件 | 画面 | 出自 e2e 截图 |
|---|---|---|
| `blue-hour/hero.webp` | 三分屏主视觉:左格 diff + 编译转圈、右上 Codex 做完、右下等待授权、右下角 toast | `02-hero-N`(连拍,默认第 3 张) |
| `blue-hour/git.webp` | Git 抽屉:暂存 / 未暂存 / 未跟踪 + 提交历史图 | `06-git` |
| `blue-hour/sessions.webp` | AI 历史会话列表 | `07-sessions` |
| `blue-hour/hover.webp` | 项目行悬停预览 | `04-hover` |
| `blue-hour/usage.webp` | 使用统计(趋势图悬停提示) | `08-usage-hover` |
| `blue-hour/markdown.webp` | Markdown 预览 + Mermaid | `09-markdown` |
| `blue-hour/mermaid.webp` | Mermaid 整窗放大 | `10-mermaid` |
| `blue-hour/search.webp` | 全局搜索(内容模式) | `11-search` |
| `blue-hour/commands.webp` | 命令库 | `12-commands` |
| `blue-hour/switcher.webp` | 项目快速切换 | `13-switcher` |
| `blue-hour/ssh.webp` | SSH 连接管理 | `14-ssh` |
| `blue-hour/settings-theme.webp` | 设置 → 主题与语言(蓝调选中) | `15-settings-theme` |
| `blue-hour/settings-switched.webp` | 点 Morning Mist 卡片后实时换肤 | `16-settings-switched` |
| `blue-hour/switched-main.webp` | 换肤后的主界面 | `17-switched-main` |
| `morning-mist/hero.webp` | 晨雾皮肤下的三分屏主视觉 | `02-hero-N` |
| `morning-mist/markdown.webp` | 晨雾皮肤下的 Markdown 预览 | `09-markdown` |
| `morning-mist/usage.webp` | 晨雾皮肤下的使用统计 | `08-usage` |
| `morning-mist/git.webp` | 晨雾皮肤下的 Git 抽屉 | `06-git` |

## 调整

- **时间轴 / 字幕 / 镜头**:改 `video/scenes.js`。镜头关键帧是 `[t, 中心x, 中心y, 缩放]`(逻辑像素 1920×1080),
  `node video/frames.mjs video/compose.html video/prev 12.8 19.6` 先抽几帧看效果。
- **主视觉挑哪张连拍**:`scenario.sh` 在 toast 前后连拍 8 张(`02-hero-N`),`HERO_BH` / `HERO_MM` 指定用第几张,
  挑「左格 diff + 编译转圈、右上绿勾、右下待授权、右下角 toast」同框的那张。
- **坐标**:`scenario.sh` 里的点击坐标对应 `seed.py` 种出来的布局,改了种子布局要同步。
- 画面里的项目、提交、会话、金额全是虚构的演示数据;`bin/` 下的 AI CLI 只是打印预设文本的脚本,不联网、不调用任何模型。
