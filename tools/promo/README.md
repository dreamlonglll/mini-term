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
       music.py          纯程序合成的氛围配乐(无采样、无版权素材)
build.sh                 全流程,产物写到 docs/promo/mini-term-promo.mp4
```

## 运行

只在 Linux 上跑(实测 Ubuntu 24.04,无显卡容器即可):

```bash
sudo apt-get install -y xvfb mesa-vulkan-drivers xdotool xclip x11-utils imagemagick ffmpeg \
  fonts-noto-cjk fonts-jetbrains-mono fonts-inter
pip install numpy
npm i -g playwright && npx playwright install chromium   # 已有 Chromium 时可跳过下载

cargo build -p mt-app          # 先有 target/debug/mini-term
tools/promo/build.sh           # 约 25 分钟:两套皮肤各跑一遍 e2e + 逐帧渲染 74 秒视频
tools/promo/build.sh --skip-e2e   # 只改了 scenes.js 时,复用已有截图重做视频
```

- 渲染走 Xvfb(3840×2160)+ `GPUI_X11_SCALE_FACTOR=2` + lavapipe 软件 Vulkan:4K 截图让视频里的推镜特写依然清晰;
  代价是软件渲染只有 1~2 帧/秒,所以视频用「截图 + 镜头运动」而不是直接录屏。
- 演示环境整个隔离:`env -i` 起应用、`HOME` 指向 `DEMO_HOME`(默认 `/home/dev`,画面里的路径就是它)、
  `MT_APP_DATA_DIR` 指向 `out/data-<皮肤>`,不会碰到本机的 mini-term 配置和 `~/.claude`。
- Playwright 装在项目外时,用 `PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs` 指给 `render.mjs`。

## 调整

- **时间轴 / 字幕 / 镜头**:改 `video/scenes.js`。镜头关键帧是 `[t, 中心x, 中心y, 缩放]`(逻辑像素 1920×1080),
  `node video/frames.mjs video/compose.html video/prev 12.8 19.6` 先抽几帧看效果。
- **主视觉挑哪张连拍**:`scenario.sh` 在 toast 前后连拍 8 张(`02-hero-N`),`HERO_BH` / `HERO_MM` 指定用第几张,
  挑「左格 diff + 编译转圈、右上绿勾、右下待授权、右下角 toast」同框的那张。
- **坐标**:`scenario.sh` 里的点击坐标对应 `seed.py` 种出来的布局,改了种子布局要同步。
- 画面里的项目、提交、会话、金额全是虚构的演示数据;`bin/` 下的 AI CLI 只是打印预设文本的脚本,不联网、不调用任何模型。
