# 皮肤包示例（theme.json 参考模板）

这是一份可直接导入、也可照着改的外置皮肤包（Dream Skin 兼容格式）。

- **拿到它**：设置 → 外观 → 主题与语言 → 外置皮肤 → 「生成示例」，会在皮肤目录下生成
  `example/`（内容与本目录相同）；或直接把本目录整个复制走，用「添加皮肤」导入。
- **改它**：皮肤目录里改完保存即生效（目录监听 300ms 防抖后整包热重载），不用重启。
  皮肤目录位置见「打开皮肤目录」按钮；Windows 上是
  `%APPDATA%\com.mini-term.app\themes\`。
- **注意**：`theme.json` 是严格 JSON，**不能写注释**，也不能有多余的尾逗号 —— 解析失败的包
  会被列表静默跳过（控制台有 warn）。

## 包结构

| 文件 | 必需 | 说明 |
|------|------|------|
| `theme.json` | 是 | 皮肤定义，见下 |
| `theme.css` | 否 | 附加样式，注入前过卫生检查；可用锚点与旋钮变量见文件内注释 |
| 背景图（如 `background.jpg`） | 否 | 文件名写进 `theme.json` 的 `image` 字段 |
| `manifest.json` | 否 | 有则导入时逐文件核对 `bytes` + `sha256`，防包在传输中损坏 |

导入时只拷**顶层文件**，子目录不会进来。皮肤 id = 目录名（与 `theme.json` 的 `id` 不一致时以目录名为准）。

## theme.json 字段

### 顶层

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `schemaVersion` | number | 否 | 格式版本，当前写 `1` |
| `id` | string | 是 | 皮肤 id，建议与目录名一致 |
| `name` | string | 是 | 显示名（设置页卡片标题） |
| `appearance` | `"dark"` \| `"light"` | 是 | 皮肤明暗。**由作者定死**：激活期间内置主题按钮置为未选中态，未覆盖的 token 回落对应明暗基线 |
| `image` | string | 否 | 背景图文件名（仅文件名，不能带路径分隔符或 `..`）。留空或不写 = 纯配色皮肤 |
| `colors` | object | 是 | 十个语义色，见下 |
| `art` | object | 否 | 背景图构图，见下 |
| `effects` | object | 否 | 氛围层旋钮，见下 |
| `terminal` | object | 否 | xterm 配色覆盖，见下 |
| `tokens` | object | 否 | 直接覆盖 CSS 变量的逃生舱，见下 |

### colors（必需）

七个必填、三个选填，值是任意合法 CSS 色值（`#rrggbb` / `rgba()` / 命名色都行；
但只有 `#rgb`/`#rrggbb`/`#rrggbbaa`/`rgb()`/`rgba()` 能参与透明度派生，其他格式会原样使用）。

| 键 | 必需 | 落到哪 |
|----|------|--------|
| `background` | 是 | `--bg-base` / `--bg-terminal`，终端与窗口底色 |
| `panel` | 是 | `--bg-surface`，侧栏、文件树等面板 |
| `panelAlt` | 是 | `--bg-elevated` / `--bg-overlay`，抬升面板与弹窗菜单 |
| `accent` | 是 | `--accent`（并派生 `--accent-muted` / `--accent-subtle`）、终端光标与选区 |
| `text` | 是 | `--text-primary`（并派生 `--text-secondary`）、终端前景色 |
| `muted` | 是 | `--text-muted` |
| `line` | 是 | `--border-default`（并派生 `--border-subtle` / `--border-strong`） |
| `accentAlt` | 否 | `--color-warning` |
| `secondary` | 否 | `--color-info` |
| `highlight` | 否 | `--color-success` |

十个色同时还会各自映射一份 `--mt-theme-color-*` 旋钮变量，供 `theme.css` 引用。

### art（可选，仅带背景图时有意义）

| 键 | 类型 | 默认 | 说明 |
|----|------|------|------|
| `focusX` | number 0–1 | `0.5` | 图片焦点在视口的横向位置 |
| `focusY` | number 0–1 | `0.5` | 纵向位置 |

`safeArea` / `taskMode` 是 Dream Skin 的字段，mini-term 目前不消费，写了也不报错。

### effects（可选）

| 键 | 类型 | 默认 | 说明 |
|----|------|------|------|
| `surfaceOpacity` | number 0–1 | `0.72` | 面板表面不透明度，**仅带背景图时生效** |
| `backgroundDim` | number 0–1 | `0.35` | 背景图上的压暗层浓度（用皮肤底色调，浅色皮肤自动变成浅纱罩） |
| `terminalOpacity` | number 0–1 | `0.6` | 终端区着色层不透明度，**仅带背景图时生效** |
| `surfaceRadius` | string | `"10px"` | 写进 `--mt-theme-surface-radius`，给 `theme.css` 用 |
| `surfaceBlur` | string | `"12px"` | 写进 `--mt-theme-surface-blur`，同上 |

后两个是字符串旋钮，`theme.css` 里能当值引用，所以和 `tokens` 走同一道外链闸（不许指向包外）。

### terminal（可选）

xterm 的 24 个配色字段，可只写一部分。没写的走推导：ANSI 16 色取当前明暗态的**内置基线**
（乱推会毁掉 TUI 可读性），`background` / `foreground` / `cursor` / `selection*` 从 `colors` 派生。

> ⚠️ **带背景图时 `terminal.background` 会被忽略**。它在展开顺序上排在透明化之后，
> 照着内置主题抄全 24 字段的皮肤会把氛围图整块盖死 —— 所以这一项被主动丢掉，
> 终端着色统一由 `--bg-terminal` 容器层承担。

字段名：`background` `foreground` `cursor` `cursorAccent` `selectionBackground`
`selectionForeground` `black` `red` `green` `yellow` `blue` `magenta` `cyan` `white`
`brightBlack` `brightRed` `brightGreen` `brightYellow` `brightBlue` `brightMagenta`
`brightCyan` `brightWhite`。坏色值会让整包校验失败（而不是把终端刷到一半）。

### tokens（可选）

优先级最高的逃生舱：直接覆盖任意 CSS 变量，上面所有映射都能被它盖掉。

两条硬约束：

- 键名必须是 `--` 开头的 CSS 变量名（`^--[A-Za-z0-9_-]+$`）。不带 `--` 时设的是**真实 CSS 属性**，
  一行 `{"background-image": "url(https://…)"}` 就绕开了全部检查。
- 值不许指向包外（`url()` 与裸字符串双查，转义写法一并还原后再查）。包内相对路径与 `data:` 可用。

## 加背景图

1. 把图片放进包目录（如 `background.jpg`）；
2. `theme.json` 加 `"image": "background.jpg"`；
3. 需要时用 `art.focusX/focusY` 调构图、`effects.backgroundDim` 调压暗。

带背景图的皮肤激活时，终端会退回 DOM 渲染（WebGL canvas 不透明，会把背景图盖死），
切回不透明皮肤自动恢复 WebGL。

## 安全边界

皮肤是从别处下载来的共享产物，`theme.css` 与 `tokens` 因此都过同一道闸：256KB 上限、
禁 `@import`、禁一切指向包外的引用。检查跑在**剥掉注释、还原 CSS 转义后**的取样上，
`url(\68 ttps://…)`、`image-set("https://…" 1x)` 这类写法都拦得住。
