# 设置面板剩余 9 分页 —— 实现规格

> 对应 `docs/gpui-parity-audit.md:49` 缺口 **#19**：
> 「设置面板剩余 9 分页：clipboard / appearance / font / ai-notification / ai-hook / system / editor / shortcuts / about
> （通知三开关现只能手改 config.json）+ 连字/scrollback/UI 字号字族/终端字号热更新（现只作用于新终端）」。
>
> 权威基线：`src/components/SettingsModal.tsx`（2170 行，单文件含全部 10 个分页）。
> 本文所有行号均指向**当前工作树**，实现时请回读源文件核对。
> 文中「原版」= Tauri/React 版，「GPUI」= `crates/` 下的新实现。

---

## 0. 任务边界

### 做什么

1. 把 `crates/mt-app/src/modal.rs` 里那个单页「终端配置」对话框（`open_terminal_settings`，modal.rs:109-120）
   升级成**两级侧栏 + 10 个分页**的设置面板，逐页对齐原版。
2. 补齐 4 项热更新（终端字号 / 终端字族 / scrollback / UI 字号字族），语义见 §3.4 与 §5。
3. 不新增 i18n 词条（§4 说明为什么）。

### 不做什么（属于别的缺口，本批只做 UI 或干脆置灰）

| 项 | 归属 | 本批处理 |
|---|---|---|
| 系统托盘本体 | audit #21 | `system` 页三个托盘开关**照做**（字段已在），但托盘本身没有，开关暂时无效果；见 §7 坑 3 |
| 长文本粘贴转文件 | audit #30 | `clipboard` 页三个开关照做，同样暂无消费方 |
| 远程粘贴目录（SSH） | audit #28 | `clipboard` 页输入框照做，暂无消费方 |
| 智能 Ctrl+C/V | 终端剪贴板批 | `clipboard` 页开关照做；`shortcuts` 页的联动行照做 |
| 内置皮肤 blueprint / fluent2 | 无对应缺口条目，**底层未实现** | 见 §7 坑 1，`appearance` 页皮肤段建议先只画不生效或整段隐藏 |
| 终端连字 | 底层未实现 | 见 §7 坑 1 |

---

## 1. 面板外壳

### 1.1 分页 id 与菜单结构

`SettingsPage` 联合类型：`src/components/SettingsModal.tsx:40-50`

```
'terminal' | 'clipboard' | 'appearance' | 'font'
| 'ai-notification' | 'ai-hook' | 'system' | 'editor' | 'shortcuts' | 'about'
```

⚠️ 源码注释（SettingsModal.tsx:36-39）明说：**旧 id 一律保留、拆页只挪内容不改 key**，因为
`initialPage` 深链会失效。GPUI 侧照抄这组字符串 id（或 Rust enum，但序列化名保持一致）。

菜单分组表：`SettingsModal.tsx:2030-2066`

| 分组标题 key | 分页 key | 标签 i18n key |
|---|---|---|
| `settings.menu.groupTerminal` | `terminal` | `settings.menu.shell` |
| ↑ | `clipboard` | `settings.menu.clipboard` |
| `settings.menu.groupAppearance` | `appearance` | `settings.menu.appearance` |
| ↑ | `font` | `settings.menu.font` |
| `settings.menu.groupAi` | `ai-notification` | `settings.menu.aiNotification` |
| ↑ | `ai-hook` | `settings.menu.aiHook` |
| `settings.menu.groupSystem` | `system` | `settings.menu.general` |
| ↑ | `editor` | `settings.menu.editor` |
| （空标题＝一条分隔线，SettingsModal.tsx:2059-2065） | `shortcuts` | `settings.menu.shortcuts` |
| ↑ | `about` | `settings.menu.about` |

分组标题为空串时渲染成 `mx-3 my-2 border-t border-[var(--border-subtle)]` 一条横线
（SettingsModal.tsx:2132-2134）。

### 1.2 外壳布局

`SettingsModal.tsx:2084-2170`

- 弹窗面板类名 `w-[680px] max-h-[80vh]`（:2099），标题 `settings.title`
- 左右两栏：`flex flex-1 overflow-hidden`（:2102）
  - 左：`w-[172px] flex-shrink-0 border-r border-[var(--border-subtle)] py-3 px-2 overflow-y-auto`（:2108）
  - 右：`flex-1 overflow-y-auto px-5 py-4`（:2164）
- 菜单项按钮（:2143-2157）：
  - 布局 `w-full flex items-center gap-2 px-3 py-2 rounded-[var(--radius-sm)] text-base text-left`
  - 选中 `bg-[var(--accent-subtle)] text-[var(--accent)]`；未选中 `text-[var(--text-secondary)]`，
    hover `text-[var(--text-primary)] hover:bg-[var(--border-subtle)]`
  - **左侧激活竖条：`w-0.5 h-4 rounded-full`，未选中时 `bg-transparent`（占位不占色）** ——
    注释（:2150）说明这是为了切页时标签文字不横向抖一下，GPUI 必须照做
- 键盘：↑/↓ 在扁平化分页序列里环形移动，跳过分组标题（:2109-2121；`MENU_ITEMS` 见 :2069）；
  移动后 `requestAnimationFrame` 把焦点送到新分页按钮上
- 无障碍：`role="tablist" aria-orientation="vertical"`，分页按钮 `role="tab" aria-selected`，
  `tabIndex` 只有当前页是 0（:2141-2142）；右栏 `role="tabpanel"`

### 1.3 弹窗行为（`src/components/Modal.tsx`）

- Esc 一律关（Modal.tsx:123-128），点遮罩默认关，且用 **mousedown 而非 click**
  （在面板内按下、拖到遮罩上松手不该关窗，Modal.tsx:166-169）
- 遮罩 `bg-black/50 backdrop-blur-sm`（Modal.tsx:171）
- 面板 `bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)]`（Modal.tsx:179）
- 顶栏 `px-5 py-4 border-b border-[var(--border-subtle)]`，标题 `text-lg font-semibold`（Modal.tsx:187-188）
- 关闭按钮 `w-7 h-7`，✕ 是 13×13 的两笔 SVG（Modal.tsx:209-220）
- 打开即把焦点送进面板（优先第一个 input），关闭还原原焦点（Modal.tsx:99-113）
- 垂直对齐默认 `top`：`items-start pt-[10vh]`（Modal.tsx:162）

GPUI 侧对应：`gpui_component::dialog::Dialog` + `crate::prompt::open_guarded(kind::SETTINGS, ..)`
（prompt.rs:47, prompt.rs:71）。防叠开已由 `open_guarded` 保证（连按 Ctrl+, 不会摞两层）。

### 1.4 入口

原版：`src/App.tsx:385`（快捷键 Ctrl+,）与 `src/App.tsx:483`（ActivityBar 齿轮），
两处都传 `setConfigPage(undefined)`，即**目前 `initialPage` 从未被真正用过**，
只是给未来的深链留的口子（`src/App.tsx:79, 550`）。

GPUI 现状：`crates/mt-app/src/main.rs:822`（`ctrl-,` → `OpenTerminalSettings`）与
`crates/mt-app/src/main.rs:658-660`（ActivityBar 齿轮）。两处都调 `modal::open_terminal_settings`。
本批把它改名/改成 `open_settings(store, initial_page: Option<SettingsPage>, ..)` 即可。

---

## 2. 通用原语（必须先建，10 页全靠它）

原版把三种形态收成组件（SettingsModal.tsx:52-56 的注释解释了动机：同一个 toggle 的 15 行 JSX 曾复制十来份）。
GPUI 侧建议在 `crates/mt-app/src/ui.rs` 里补同名的一批。

| 原语 | 源码 | 关键样式 / 语义 |
|---|---|---|
| `useConfigPatch` | :59-70 | 写一份 config 补丁 → `setConfig` → `saveConfigToDisk`。GPUI 对应 `AppStore` 上加一个通用 patch 入口，落盘走已有的 `save_config_soon`（store.rs:1460，500ms 防抖） |
| `Section` | :73-80 | `section` + `space-y-2`；标题 `text-base text-[var(--text-muted)] uppercase tracking-[0.1em]`。页根节点 `space-y-6` |
| `Hint` | :83-85 | 分节末尾补充说明，`text-sm text-[var(--text-muted)]` |
| `SettingRow` | :88-113 | 左标题+说明 / 右控件。`flex items-center justify-between gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]`；`disabled` 时 `opacity-50 pointer-events-none`。标题 `text-base text-[var(--text-primary)]`，说明 `text-sm text-[var(--text-muted)]` |
| `Toggle` | :115-142 | `w-9 h-5 rounded-full`（36×20px）；开 `bg-[var(--accent)]`，关 `bg-[var(--border-strong)]`；滑块 `absolute top-0.5 w-4 h-4 rounded-full bg-white`，开时 `translate-x-[18px]`、关时 `translate-x-0.5`。`role="switch" aria-checked` |
| `ToggleRow` | :144-165 | `SettingRow` + `Toggle`。多一个 `busy` 参数：**开关仍可见可点，只是这一刻不响应**（用于 hook 开关提交中） |
| `NumberRow` | :172-226 | `<input type=number>` `w-24 ... font-mono text-right`。**输入期间只改草稿，失焦/回车才归一并提交**（:167-171 注释：边打字边 clamp 会让「1000」在敲到「1」时就被吃掉）。默认归一规则（:202-206）：`finite && v >= (min ?? 0)` 则 `min(v, max ?? MAX_SAFE)`，否则 `null` → **回落已保存值**；`clamp` 传入时覆盖默认规则 |
| `ChoiceGroup` | :229-256 | 单选段（主题/皮肤）。每项 `flex-1 py-2 rounded-[var(--radius-sm)] text-base`；选中 `bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)]`，未选中 `bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)]` |
| `FontSizeSlider` | :744-778 | 标题行左标签右 `{value}px`（`font-mono text-[var(--accent)]`）；下方 `min` 数字 + `<input type=range step=1 accent-[var(--accent)] h-1.5>` + `max` 数字。**拖动即时提交**（`onChange` 直连），无草稿态 |
| `FontFamilyInput` | :945-982 | 上标签下输入框，`w-full ... px-2 py-1.5 font-mono`，`spellCheck=false`；草稿态，失焦/回车才提交（:960-962） |
| `ShellRow` | :260-368 | 已迁移，见 §3.1 |
| `EditorRow` | :372-476 | 与 `ShellRow` 同构，少一个 args 字段、多一个「...」浏览按钮 |

**GPUI 缺口**：`ui.rs` 目前只有 `ghost_button`（:297）/`primary_button`（:316）/`danger_button`（:338）/
`section_title`（:357）/`status_dot`（:401）。**没有 toggle / 数字输入行 / 滑块 / 单选段 / 复选框**，
全部要新建。`section_title` 与原版 `Section` 样式不同（它是「竖条 + 文字」，抄的是用量面板），
本批要么给它加一个变体、要么另起 `settings_section_title`。

---

## 3. 逐分页规格

### 3.0 通读约定

- 「字段」列给出 `camelCase`（TS `src/types.ts` / `config.json` 键名）与 `snake_case`
  （`crates/mt-config/src/config.rs` 的 Rust 字段），两者由 `#[serde(rename_all = "camelCase")]` 对齐。
- 「缺省」列给出 Rust 侧 `Default`/`serde(default)` 的值与 UI 层 `?? x` 的值；**两者不一致时以 UI 层为准**
  （UI 层的 `??` 才是用户看到的）。
- 所有设置项**一律即时生效 + 即时落盘**，没有「保存」按钮。原版落盘走 `saveConfigToDisk`；
  GPUI 走 `AppStore::save_config_soon`（500ms 防抖，store.rs:1459-1473）。

---

### 3.1 `terminal` —— Shell（**已迁移，仅作对照**）

原版 `TerminalSettings`：SettingsModal.tsx:480-636。

**Section 1「可用终端（●= 默认）」**（`settings.terminal.availableTerminals`）

- N 行 `ShellRow`（:260-368）：单选圆点 `w-3 h-3 rounded-full border-2`（默认项 accent 实心）+
  名称（`text-base font-medium`）+ 命令行（`text-sm font-mono truncate`，含 args）+
  hover 才出现的「编辑 / 删除」（`hidden group-hover:flex`，:352）
- 编辑态：名称(flex-1) + 命令(flex-2) 一行、args + 保存 + 取消一行（:294-333）
- 「+ 添加终端」虚线按钮（:606-613）；添加态边框 `border-[var(--accent)] border-dashed`（:568）
- 删除默认项时默认项回落到剩余第一项（:529-537）；改名时若改的是默认项，默认项跟着改名（:539-546）
- 底部 Hint `settings.terminal.defaultHint`
- 字段：`availableShells` / `available_shells`（`Vec<ShellConfig>`），`defaultShell` / `default_shell`
- args 解析：`args.trim().split(/\s+/)`，空则 `undefined`（:289）

**Section 2「终端行为」**（`settings.terminal.behavior`）

| 控件 | 文案 key | 字段 | 缺省 | 范围 | 生效 |
|---|---|---|---|---|---|
| `NumberRow` 回滚行数 | `settings.terminal.scrollback` / `...scrollbackDesc` | `terminalScrollback` / `terminal_scrollback` | 10000（`terminalScrollback.ts` `DEFAULT_SCROLLBACK`；config.rs:416-418 同值） | min 0 / max 200000（`MAX_SCROLLBACK`）/ step 1000 | **热更新全部已开终端**：`updateAllTerminalScrollback(v)` 先跑再落盘（SettingsModal.tsx:626-631） |

`resolveScrollback` 的钳制规则（`src/utils/terminalScrollback.ts`）：
非数字 / NaN / 负数 → 回落 10000；否则 `min(round(v), 200000)`。

**GPUI 现状**：`modal.rs:122-315` 已实现 shell 列表（含默认项圆点 / 编辑 / 删除 / 添加），
但**没有** Section 2 的回滚行数；另外它把「语言段控件」（modal.rs:323-373）与
「终端字号 −/+ 按钮」（modal.rs:277-313）也塞在这一页——本批要把这两段挪去 `appearance` / `font` 页。

---

### 3.2 `clipboard` —— 复制粘贴

原版 `ClipboardSettings`：SettingsModal.tsx:640-740。

**Section 1「复制粘贴」**（`settings.clipboard.copyPaste`）

| # | 控件 | 文案 key | 字段（camel / snake） | 缺省 | 范围/校验 | 生效语义 |
|---|---|---|---|---|---|---|
| 1 | `ToggleRow` | `settings.clipboard.smartCopyPasteTitle` / `...Desc` | `smartCopyPaste` / `smart_copy_paste` | `false`（UI `?? false`，:645；config.rs:490 同） | bool | 即时。**与 `shortcuts` 页联动**（§3.9） |
| 2 | `NumberRow` float | `settings.clipboard.autoCopyDwellTitle` / `...Desc` | `selectionAutoCopySecs` / `selection_auto_copy_secs`（`Option<f64>`） | UI `?? 1`（:647）；Rust `None` | min 0.2 / step 0.5 / float。**自定义 clamp**（:682-684）：`!finite || n < 0` → null(回落)；`n === 0` → 0（**0 = 关闭该功能**）；否则 `clamp(n, 0.2, 60)` | 即时落盘；需下发给已存在的终端 |

clamp 那条注释（:681）写得很清楚：**0 是「关掉」的唯一出口**，因为静默覆盖剪贴板的行为必须可退出。
非零值一律钳在 0.2~60s。GPUI 侧对应 `TerminalView::set_selection_dwell`（mt-ui/src/terminal/view.rs:319），
store 侧取数口是 `AppStore::selection_dwell()`（store.rs:884-886，`unwrap_or(1.0)`）；
store.rs:879-883 的注释已经把「设置页接上后要连带给存量终端下发」这件事写好了。

**Section 2「长文本粘贴」**（`settings.clipboard.longPaste`）

| # | 控件 | 文案 key | 字段 | 缺省 | 范围 | 备注 |
|---|---|---|---|---|---|---|
| 1 | `ToggleRow` | `settings.clipboard.longPasteTitle` / `...Desc` | `longPasteToFile` / `long_paste_to_file` | UI `?? true`（:646）；config.rs:482 `true` | bool | 关掉时下面两行**置灰**（`disabled={!longPasteEnabled}`，:702, :711） |
| 2 | `NumberRow` | `settings.clipboard.lineThreshold` / `...Desc` | `longPasteLineThreshold` / `long_paste_line_threshold` | UI `?? 10`（:648）；config.rs:437-439 `10` | min 0 / max 100000 | 0 = 不按行数判断 |
| 3 | `NumberRow` | `settings.clipboard.charThreshold` / `...Desc` | `longPasteCharThreshold` / `long_paste_char_threshold` | UI `?? 2000`（:649）；config.rs:440-442 `2000` | min 0 / max 10000000 | 0 = 不按字符判断 |
| 4 | `Hint` | `settings.clipboard.longPasteFooter` | — | — | — | 「任一阈值命中即触发转存」 |

判定逻辑（供实现方参考，不在设置页）：`terminalCache.ts:671-678` —— 任一阈值 > 0 且命中即转存。

**Section 3「远程粘贴」**（`settings.clipboard.remotePaste`）

- **不是 `SettingRow`**，是一个自绘的卡片（:718-735）：标题行 + 说明行（`mb-2`）+ 整宽输入框。
  输入框 `w-full ... font-mono`，`spellCheck=false`，placeholder = 默认值本身
- 字段 `remotePasteDir` / `remote_paste_dir`，缺省 `".mini-term/pasted"`
  （`src/utils/pastePath.ts` 的 `DEFAULT_REMOTE_PASTE_DIR`；Rust 侧 `config.rs:443-447 default_remote_paste_dir()`）
- 提交时机：失焦 / 回车（:732-733）
- **归一规则（:657-663）**：`trim()` 后为空 → 回落默认值（不落空串让后端每次兜底）；
  `..` 的拒绝在后端 `resolve_paste_dir`，**前端不重复判**（注释明说：两处判定会漂移）
- 底部 `Hint` = `settings.clipboard.remotePasteFooter`

---

### 3.3 `appearance` —— 主题与语言

原版 `AppearanceSettings`：SettingsModal.tsx:782-859（+ 内嵌 `CustomThemePacksSection`:1798-2013）。

**Section 1「语言」**（`settings.appearance.language`）

- `SettingRow` 标题 `settings.appearance.languageLabel`，右侧 `<LanguageToggle />`
- `LanguageToggle`（`src/components/LanguageToggle.tsx:16-33`）：
  `inline-flex rounded-md overflow-hidden border border-[var(--border-default)]`，
  两个 `px-3 py-1 text-xs` 按钮；选中 `bg-[var(--accent)] text-white`，
  未选中 `bg-transparent text-[var(--text-muted)] hover:text-[var(--text-primary)]`
- **选项文字是母语 endonym「中文」/「English」，永不翻译**（LanguageToggle.tsx:4-5）
- 字段 `locale`（GPUI 独有，config.rs:111-112；TS 侧存 localStorage）

GPUI 已实现：`modal.rs:317-373 render_language_section`，与上述一字不差
（连「endonym 不翻译」都照做了）。本批只是把它从 shell 页挪到 appearance 页第一节。

**Section 2「主题」**（`settings.appearance.theme`）

| 控件 | 选项 | 字段 | 缺省 |
|---|---|---|---|
| `ChoiceGroup` | `dark`/`light`/`auto` → `settings.appearance.themeDark` / `themeLight` / `themeAuto` | `theme` / `theme`（String） | `"auto"`（config.rs:419-421） |
| `ToggleRow` | `settings.appearance.terminalFollowTheme` / `...Desc` | `terminalFollowTheme` / `terminal_follow_theme` | `true`（config.rs:425-427） |

**⚠️ 三个字段的联动（这是本页最容易做错的地方）**

1. `ChoiceGroup` 的 `value` 是 **`config.customThemeId ? '' : config.theme`**（:827）——
   激活外置皮肤时三个按钮**全不高亮**（空串匹配不上任何选项）。皮肤段同理（:845）。
2. `handleThemeChange`（:787-795）：切主题 = **退出外置皮肤**。顺序是
   `clearCustomTheme()` → 写 `{ theme, customThemeId: undefined }` → `applyTheme(theme)`
   → `updateAllTerminalThemes(terminalFollowTheme ?? true)` → 落盘。
   注释（:788）：「外置皮肤的明暗由 appearance 定死，切主题 = 退出皮肤回内置」。
3. `handleSkinChange`（:797-808）：同样先 `clearCustomTheme()`、清 `customThemeId`，
   然后 `applyTheme(theme ?? 'auto')` + `updateAllTerminalThemes(terminalFollowTheme)`。
4. `handleTerminalFollowThemeChange`（:810-815）：只改这一个字段 + `updateAllTerminalThemes(follow)`。
   注意传的是**刚改的新值**而不是 store 现值（`terminalCache.ts:601` 的注释解释：
   调用方可能拿着一个还没落进 store 的值）。
5. `terminalFollowTheme` 关闭时终端**固定用内置暗色**，与当前明暗无关
   （`terminalCache.ts:76`；GPUI 侧 `theme.rs:159-165` 已照做且有测试 theme.rs:191-204）。

**Section 3「皮肤」**（`settings.appearance.skin`）

`ChoiceGroup`：`none` / `blueprint` / `fluent2`，标签分别是
`settings.appearance.skinNone` / `settings.appearance.skinBlueprint` / **字面量 `'Fluent 2'`**（:849，不走 i18n）。
底下 `Hint` = `settings.appearance.skinDesc`。字段 `skin` / `skin`，缺省 `"none"`（config.rs:422-424）。

> **GPUI 底层未实现**：`crates/mt-app/src/theme.rs:31-35` 明确写着
> 「`skin`（内置皮肤 blueprint / fluent2）没有实现：GPUI 侧还没有对应的内置皮肤色表，当前一律按 `none` 处理」。
> 本批建议：整段先不渲染，或渲染但只有 `none` 可选并给出说明。**不要做成看着能选、点了没反应**。

**Section 4「外置皮肤」（`CustomThemePacksSection`，:1798-2013）**

标题行（:1936-1973）：左边 `settings.themes.customSection`，右边 5 个 `px-2 py-1 text-sm` 小按钮，
`flex-wrap` 允许换行（注释 :1935 说明：680px 弹窗里英文文案会贴边）：

| 按钮 | key | 动作 | GPUI 对应 |
|---|---|---|---|
| 添加皮肤 | `settings.themes.addPack` | 目录选择框 → `import_theme_pack` → `invalidateThemeAssets` → 刷新 | `mt_config::ThemePacks::import_dir`（theme_packs.rs:159） |
| 导入 zip | `settings.themes.importZip` | zip 文件选择框 → `import_theme_pack_zip` | `ThemePacks::import_zip`（theme_packs.rs:176） |
| 生成示例 | `settings.themes.createExample`（tooltip `...createExampleHint`） | `create_example_theme_pack` → 刷新 → 成功提示 `settings.themes.exampleCreated`（插值 `{id}`） | `ThemePacks::create_example`（theme_packs.rs:131） |
| 打开皮肤目录 | `settings.themes.openDir` | `get_themes_dir` → `revealItemInDir` | `ThemePacks::root()`（theme_packs.rs:70）+ `fs_ops::reveal_in_file_manager`（fs_ops.rs:83） |
| 刷新 | `settings.themes.refresh` | 重新 `listThemePacks()` | `crate::theme::list_packs()`（theme.rs:87，目前带 `#[allow(dead_code)]` 注明「设置面板「外观」页的落点」） |

列表：`grid grid-cols-2 gap-2` 的 `ThemeCard`（:1706-1796）。卡片内容是一张**缩小版的界面预览**：

- 外框 `p-3 rounded-[var(--radius-md)] border`；激活 `border-[var(--accent)] bg-[var(--accent-subtle)]`
- 预览区 `w-full h-24 rounded-[var(--radius-sm)]`，底色 = `colors.background`
- 有图时铺 `backgroundSize: cover`，`backgroundPosition: {focusX*100}% {focusY*100}%`（默认 0.5/0.5，:1748）
- 压暗层：`color-mix(in srgb, {background} 35%, transparent)`（与真实氛围层同款 35%，:1755）
- 迷你侧栏：`left-1.5 top-1.5 bottom-1.5 w-12`，底 `color-mix(..., {panel} 72%, transparent)`，
  内含 3 条圆角小横杠（accent / text@60% / text@40%）
- 迷你终端区：`left-[3.9rem] right-1.5`，底 `color-mix(..., {background} 60%, transparent)`，
  内容 `❯ Aa 字`（提示符 accent、文字 text，`text-[10px] font-mono`）+ 一条 text@50% 横杠
- 卡片底部：名称（激活时 accent）+ 副标题 = `themeId`；hover 才出现的 ✕ 删除
  （`hidden group-hover/card:block`，:1785-1792，`stopPropagation` 防止触发选中）

关键行为：

- **选中**（`selectCustom`:1831-1844）：先 `loadAndApplyCustomTheme` 成功才写 `customThemeId` 并落盘；
  失败弹 `settings.themes.applyFailed`（插值 `{detail}`）且**不改配置**
- **删除**（`deletePack`:1882-1908）：
  1. `window.confirm(settings.themes.deleteConfirm)`（插值 `{name}`）
  2. **先退出该主题（`clearCustomTheme()` 内含 unwatch）再删目录** —— 注释 :1886-1888 记了这个坑：
     反过来的话 notify 的目录句柄还开着，被删目录在 Windows 上处于 delete-pending，
     紧接着重导入同名主题会撞 `ERROR_ACCESS_DENIED`
  3. 删失败要**把主题装回去**（:1896-1898），避免界面上皮肤没了、目录还在
- **同名覆盖导入必须 `invalidateThemeAssets(themeId)`**（:1856-1857），否则缩略图还是上一版
- 空列表时显示 `settings.themes.empty` 卡片（:1974-1977）
- error（红框）/ notice（绿框）二选一展示，`notice && !error`（:2001-2010）

---

### 3.4 `font` —— 字体

原版 `FontSettings`：SettingsModal.tsx:863-941。

**Section 1「字体大小」**（`settings.font.fontSize`）

| 控件 | 标签 key | 字段 | 缺省 | 范围 | 生效 |
|---|---|---|---|---|---|
| `FontSizeSlider` | `settings.font.uiFontSize` | `uiFontSize` / `ui_font_size` | 13（UI `?? 13` :894；config.rs:410-412 同） | 10–20，step 1 | **即时全局**：`document.documentElement.style.fontSize = '{n}px'`（:872），即改 rem 基准 |
| `FontSizeSlider` | `settings.font.terminalFontSize` | `terminalFontSize` / `terminal_font_size` | 14（UI `?? 14` :902；config.rs:413-415 同） | 10–24，step 1 | **热更新全部已开终端**（原版由 `TerminalInstance` 订阅 config 改 `term.options.fontSize`） |
| `Hint` | `settings.font.fontSizeFooter` | — | — | — | — |

> `uiFontSize` 是 Tailwind `text-base/sm/xs` 的 rem 基准：`styles.css` 里 `body { font-size: 13px }`（:131），
> 但 `text-base = 1rem` 跟的是 `html` 的 inline `fontSize`。所以默认下
> `text-base ≈ 13px`、`text-sm ≈ 11.4px`、`text-xs ≈ 9.75px`。
> 启动时的应用点在 `src/App.tsx:140-141`。

**Section 2「字体」**（`settings.font.font`）

| 控件 | 标签 key | 字段 | placeholder | 生效 |
|---|---|---|---|---|
| `FontFamilyInput` | `settings.font.uiFont` | `uiFontFamily` / `ui_font_family`（`Option<String>`） | `'DM Sans', system-ui, sans-serif`（:913 字面量） | `applyUiFontFamily`：把值同时写进 `--app-font-family` 与 `--app-font-mono` 两个 CSS 变量；空串则 `removeProperty` 回落默认（`src/utils/fontManager.ts:8-18`） |
| `FontFamilyInput` | `settings.font.terminalFont` | `terminalFontFamily` / `terminal_font_family`（`Option<String>`） | `DEFAULT_TERMINAL_FONT_FAMILY`（terminalCache.ts:50-51） | 热更新已开终端 |
| `Hint` | `settings.font.fontFamilyFooterPrefix` + `<span class=font-mono>'JetBrainsMono Nerd Font', monospace</span>` + `...Suffix`（:922-924） | — | — | 三段拼，中间那段是等宽字面量 |

空串提交 → 写 `undefined`（:881, :920），不是空字符串。

`DEFAULT_TERMINAL_FONT_FAMILY`（terminalCache.ts:50-51）：
`'JetBrainsMono Nerd Font', 'CaskaydiaCove Nerd Font', 'JetBrains Mono', 'Cascadia Code', Consolas` + CJK 回退。
CJK 回退（`CJK_FALLBACK_FONTS`，terminalCache.ts:48）：`'Microsoft YaHei', 'PingFang SC', 'Noto Sans CJK SC', monospace`，
**用户自选字体也会自动补这串**（`resolveTerminalFontFamily`，terminalCache.ts:53-58）。
GPUI 侧对应 `TerminalStyle.font_fallbacks`（mt-ui/src/terminal/theme.rs:113-114, 125-130）。

**Section 3「连体字」**（`settings.font.ligatures`）

`ToggleRow`：标题 `settings.font.ligaturesTitle`，说明由 5 段拼成（:930-934）：
`ligaturesDescPrefix` + `==` + `=>` + `!=` + `->`（都是 `font-mono` span）+ `ligaturesDescSuffix`。
字段 `terminalLigatures` / `terminal_ligatures`，缺省 `false`（UI `?? false` :887；config.rs:465 同）。

> **GPUI 底层完全没有连字**：`grep -ri "ligature\|calt" crates/` 只命中 `mt-config` 的字段定义本身。
> mt-ui 是自绘渲染器，没有走 HarfBuzz calt。见 §7 坑 1。

---

### 3.5 `ai-notification` —— 通知提醒

原版 `AiNotificationSettings`：SettingsModal.tsx:1257-1344。

**Section 1「通知方式」**（`settings.aiNotification.method`）

| # | 控件 | 文案 key | 字段 | 缺省 |
|---|---|---|---|---|
| 1 | `ToggleRow` | `settings.aiNotification.popup` / `...popupDesc` | `aiCompletionPopup` / `ai_completion_popup` | `true`（config.rs:428-430） |
| 2 | `ToggleRow` | `settings.aiNotification.taskbarFlash` / `...taskbarFlashDesc` | `aiCompletionTaskbarFlash` / `ai_completion_taskbar_flash` | `true`（config.rs:431-433） |
| 3 | `ToggleRow` | `settings.aiNotification.sound` / `...soundDesc` | `aiCompletionSound` / `ai_completion_sound` | `true`（config.rs:119-120） |
| 4 | `SettingRow`（自定义右侧控件组） | `settings.aiNotification.customSound` | `aiCompletionSoundPath` / `ai_completion_sound_path`（`Option<String>`） | `None` |
| 5 | `Hint` | `settings.aiNotification.footer` | — | — |

第 4 行细节（:1296-1328）：

- 说明位置显示当前路径，无值时显示 `settings.aiNotification.defaultSound`；
  样式 `font-mono block truncate`
- **整行 `disabled={!config.aiCompletionSound}`**（:1303）——提示音总开关关掉时置灰
- 右侧三个按钮（`px-2.5 py-1 text-sm`）：
  - 「试听」`settings.aiNotification.preview`（同时用作 `title`）→ `playNotificationSound(path)`
  - 「选择文件」`settings.aiNotification.chooseFile` → 文件选择框，
    title = `settings.aiNotification.soundDialogTitle`，
    filter name = `settings.aiNotification.audioFilter`，
    **扩展名 `['mp3','wav','ogg','flac','aac','m4a']`**（:1267）
  - 「清除」`settings.aiNotification.clear` —— **仅当已有自定义路径时才渲染**（:1319），
    hover 转 `--color-error`；点击写 `aiCompletionSoundPath: undefined`

**Section 2「触发时机」**（`settings.aiNotification.trigger`）

| 控件 | 文案 key | 字段 | 缺省 |
|---|---|---|---|
| `ToggleRow` | `settings.aiNotification.attention` / `...attentionDesc` | `aiAttentionNotify` / `ai_attention_notify` | `true`（config.rs:125-126） |
| `Hint` | `settings.aiNotification.attentionFooter` | — | — |

**GPUI 现状**：这五个字段**后端全都已经在消费**——
`store.rs:1032`（sound_path）、`store.rs:1062-1065`（sound/flash/popup/attention_notify）
→ `notify::NotifyPrefs`（notify.rs:67-75）→ `DoneTracker::apply`（notify.rs:117-164）。
纯粹只差 UI。默认提示音与自定义格式支持有偏差，见 §7 坑 2。

---

### 3.6 `ai-hook` —— Hook 事件

原版 `AiHookSettings`：SettingsModal.tsx:986-1253。这页**不是 `Section` 结构**，
根节点直接 `space-y-2`，顶部一行标题（:1106-1108，`settings.aiHook.title`，样式同 `Section` 标题）。

#### 布局顺序

1. **`ToggleRow` 启用 Hook 服务器**（:1110-1116）
   - 文案 `settings.aiHook.enableHook` / `...enableHookDesc`
   - 字段 `hookEnabled` / `hook_enabled`，缺省 `false`（config.rs:157-158）
   - `busy={toggling}`——提交期间开关仍在，只是不响应
   - `handleToggleHook`（:1034-1047）：先 `invoke('toggle_hook_server', { enabled })`，
     **成功了才写配置并落盘**，然后刷新状态；失败把错误写进 `resultMsg`
2. **`resultMsg` 错误/结果条**（:1119-1123）——`whitespace-pre-wrap`，
   **始终可见，不受下面那块的置灰影响**（注释 :1118）
3. **以下全部包在一个 `opacity-50 pointer-events-none`（开关关闭时）的容器里**（:1126）
4. **服务器状态卡**（:1127-1137）：`w-2 h-2 rounded-full` 状态点
   （running → `--color-success`，否则 `--border-strong`）+
   `settings.aiHook.serverLabel` + （`serverRunning`（插值 `{port}`）| `serverStopped`）+
   下一行说明 `settings.aiHook.serverDesc`
5. **注入目标列表**（:1140-1181）：卡片 `overflow-hidden`，顶部小标题
   `settings.aiHook.targetsLabel`，然后每家一行 `<label>`（`px-3 py-2 cursor-pointer hover:bg-[var(--border-subtle)]`）：
   `<input type=checkbox accent-[var(--accent)]>` + 名称（`r.label`）+ 配置文件路径（`text-sm truncate`，`title=r.file`）
   + 右侧状态徽章：
   | 条件 | 文案 key | 颜色 |
   |---|---|---|
   | `registered === 0` | `settings.aiHook.stateAbsent` | `--text-muted` |
   | `0 < registered < total` | `settings.aiHook.stateStale`（插值 `{n}` `{total}`） | `--color-warning` |
   | `registered === total` | `settings.aiHook.stateReady`（插值 `{n}` = total） | `--color-success` |
6. **注册 / 卸载两个按钮**（:1183-1198）：`flex gap-2`，各 `flex-1 py-2`；
   注册是主按钮（accent 实心），卸载是次按钮；
   `disabled = busy || agents.length === 0`；忙时文案换 `registering` / `unregistering`
7. **`agents.length === 0` 时**显示居中提示 `settings.aiHook.noTargetSelected`（:1199-1203）
8. **「查看配置片段（手动粘贴）」全宽文字按钮**（:1205-1210），展开后文案变 `collapseSnippet`
9. **配置片段面板**（:1212-1247）：三个 tab（Claude Code / Codex / Grok，**标签是字面量不走 i18n**，:1225），
   选中 tab `text-[var(--accent)] border-b-2 border-[var(--accent)]`；
   内容区 `text-xs font-mono whitespace-pre-wrap max-h-64 overflow-y-auto select-all`；
   Claude 是单文件，Codex/Grok 是文件数组，每个文件先一行灰色文件名（带 `(note)`）再是正文，
   非首个文件加 `mt-3 pt-3 border-t`
10. **`Hint`** `settings.aiHook.footer`

#### 默认勾选逻辑（**必须照抄**，:1017-1025）

```
第一次拿到注册现状时（selected === null）：
  installed = list.filter(r => r.registered > 0).map(r => r.agent)
  selected = installed.length > 0 ? Set(installed) : Set(全部)
```
注释说得很直白：默认勾「已经装了的那几家」，用户再点一次注册就是补齐新事件，
不会顺手往没在用的 CLI 里写配置；一家都没装过（首次使用）才全选，保住「一键注册」体验。

#### 后端 API 对照

| 原版 Tauri command | GPUI 现成实现 |
|---|---|
| `get_hook_status` | `mt_ai::hook_server::hook_status(&HookState) -> HookStatusInfo{port,running}`（hook_server.rs:778-783） |
| `toggle_hook_server` | `AiPerception::set_hook_server_enabled(&data_dir, enabled)`（perception.rs:137-144） |
| `get_ai_hook_registrations` | `mt_ai::hook_registry::get_ai_hook_registrations() -> Vec<HookRegistrationInfo>`（hook_registry.rs:937-954） |
| `register_ai_hooks` / `unregister_ai_hooks` | 同名函数（hook_registry.rs:910, :925），参数 `Option<Vec<HookAgent>>`，**缺省/空 = 三家全上**（hook_registry.rs:876-890） |
| `get_hook_config_snippet` | `mt_ai::hook_registry::get_hook_config_snippet() -> Result<serde_json::Value>`（hook_registry.rs:957） |

`HookRegistrationInfo` 字段（hook_registry.rs:893-905）：`agent` / `label` / `file` / `registered` / `total`，
与原版 `HookRegistration`（`src/types.ts`）同构。`HookAgent` 三家（hook_registry.rs:792-798）。

> **接线缺口**：`AiBridge`（crates/mt-app/src/ai.rs:46-119）只在启动时按 `hook_enabled` 起服务
> （main.rs:866-874 / ai.rs:70-74），**没有透出运行时开关与状态查询**。
> 需要在 `AiBridge` 上加两个直通方法（`set_hook_enabled` / `hook_status`），
> `data_dir` 用 `crate::app_data_dir()`。注意 `perception()` 已经是 `pub`（ai.rs:85-87）。

原版这几个 invoke 都是异步的；GPUI 侧 `register_ai_hooks` 会写用户主目录下的配置文件，
**必须丢 background executor**（与 N 批文件操作同一处理），回主线程刷新。

---

### 3.7 `system` —— 常规

原版 `SystemSettings`：SettingsModal.tsx:1348-1398。

**Section 1「状态栏」**（`settings.system.trayGroup`）

| # | 控件 | 文案 key | 字段 | 缺省 | 范围 |
|---|---|---|---|---|---|
| 1 | `ToggleRow` | `settings.system.trayStatusTitle` / `...Desc` | `trayStatusEnabled` / `tray_status_enabled`（`Option<bool>`） | UI `?? true`（:1353）；Rust `None` | bool |
| 2 | `ToggleRow` | `settings.system.trayClickFocusTitle` / `...Desc` | `trayClickFocus` / `tray_click_focus`（`Option<bool>`） | UI `?? true`（:1354） | bool |
| 3 | `NumberRow` | `settings.system.trayMaxTitle` / `...Desc` | `trayMaxProjects` / `tray_max_projects`（`Option<u32>`） | UI `?? 5`（:1355） | min 1 / max 20 |

**⚠️ 第 2、3 行不是置灰而是整个不渲染**（`{trayEnabled && (<>...</>)}`，:1368-1385）——
与 clipboard 页「置灰」的处理**不一样**，别抄串了。

**Section 2「启动」**（`settings.system.startupGroup`）

| 控件 | 文案 key | 字段 | 缺省 |
|---|---|---|---|
| `ToggleRow` | `settings.system.aiAutoResumeTitle` / `...Desc` | `aiAutoResume` / `ai_auto_resume`（`Option<bool>`） | UI `?? true`（:1357，注释 :1356：「缺省开启，保持旧行为，老配置升级上来不改变启动表现」） |

GPUI 侧 `ai_auto_resume` **已在消费**：`terminal_area.rs:364`、`store.rs:702`。
托盘三项**完全没有消费方**（托盘整块是 audit #21）。

---

### 3.8 `editor` —— 外部编辑器

原版 `EditorSettings`：SettingsModal.tsx:1402-1562。结构与 `terminal` 页的 shell 列表**同构**，
只有一个 `Section`（`settings.editor.externalEditor`，标题里带「（● = 默认）」）。

- 行组件 `EditorRow`（:372-476）：单选圆点 + 名称 + `command`（`font-mono truncate`）+ hover 出现的编辑/删除
- 编辑态（:405-443）：名称一行；命令行 + **「...」浏览按钮** + 保存 + 取消
- 「+ 添加编辑器」虚线按钮 `settings.editor.addEditor`（:1550-1555）；
  添加态（:1509-1548）：名称一行、命令 + 「...」+ 添加 + 取消 一行
- 底部 `Hint` = `settings.editor.editorDefaultHint`
- 字段：`editors` / `editors`（`Vec<EditorConfig{name,command}>`，config.rs:403-408），
  `defaultEditor` / `default_editor`（`Option<String>`）

**与 shell 列表的三处差异（别照抄漏了）**：

1. **重名校验**：新增（:1432-1435）与改名（:1462-1465）都会检查同名，撞了就
   `alert(settings.editor.editorExistsAlert)`（插值 `{name}`）并**放弃这次修改**。shell 列表没有这一条。
2. **浏览按钮**（`handleBrowseEditorPath`，:1479-1492）：
   title = `settings.editor.browseDialogTitle`；
   **Windows 下 filter `{ name: settings.editor.executableFilter, extensions: ['exe'] }`，
   非 Windows 不加 filter**（:1485-1487）
3. 空列表时 `defaultEditor` 写 `undefined` 而不是空串（:1423）

GPUI 侧 `editors` / `default_editor` **已在消费**：`file_tree.rs:194, 203` →
`mt_project::editor`（editor.rs:26-34，「没指定 → 用 `default_editor` 指向的那个，再退到列表第一个」）。

---

### 3.9 `shortcuts` —— 快捷键

原版 `ShortcutsSettings`：SettingsModal.tsx:1665-1702。

- **整页从 `src/utils/hotkeys.ts` 的表生成**（注释 :1659-1663：之前是手写静态说明，
  与实际监听逻辑各写各的，改了键位忘改说明就漂移）
- `hotkeyGroups()`（hotkeys.ts:143-154）按 `groupKey` 归组、保持声明顺序
- 每组：标题（同 `Section` 标题样式，`text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2`）
  + `space-y-1` 的行列表
- 每行（:1670-1678）：`flex items-center justify-between gap-4 px-3 py-2.5 rounded-[var(--radius-md)]
  bg-[var(--bg-base)] border border-[var(--border-subtle)]`，左边描述、右边 `<kbd class="kbd">`
- 底部 `Hint` = `settings.shortcuts.footer`

**唯一的动态项**（:1689-1695）：`smartCopyPaste` 开启时，在「复制粘贴」组
（`groupKey === 'settings.shortcuts.clipboard'`）末尾追加两行：

| id | 描述 key | 键位 |
|---|---|---|
| `smartCopy` | `settings.shortcuts.copyDesc` | `{MOD_LABEL}+C` |
| `smartPaste` | `settings.shortcuts.pasteToTerminal` | `{MOD_LABEL}+V` |

`MOD_LABEL` = mac `⌘` / 其余 `Ctrl`（`src/utils/platform.ts:5`）。
`comboLabel`（hotkeys.ts:126-134）：mac 用 `⌘⇧⌥` 无分隔符拼，其余 `Ctrl+Shift+Alt+X`；
键名映射 `ArrowUp→↑` 等（hotkeys.ts:116-123）。

**`.kbd` 样式**（`src/styles.css:494-506`）：
`inline-block; padding:1px 5px; border-radius:var(--radius-sm); background:var(--bg-elevated);
border:1px solid var(--border-default); border-bottom-width:2px; font-family:var(--app-font-mono);
font-size:0.85em; line-height:1.5; color:var(--text-secondary); white-space:nowrap;`
（`border-bottom-width:2px` 是那个「键帽」立体感的来源，别漏。）

#### 原版键位全表（`src/utils/hotkeys.ts:48-79`）

| 组 | id | 键位 | GPUI 现状 |
|---|---|---|---|
| `terminalOps` | newTerminal | Ctrl+Shift+T | ✅ main.rs:816 |
| | closePane | Ctrl+Shift+W | ✅ main.rs:817 |
| | renamePane | F2 | ✅ main.rs:821 |
| | splitRight | Ctrl+Shift+D | ✅ main.rs:818 |
| | splitDown | Ctrl+Shift+E | ✅ main.rs:819 |
| `navigation` | nextPane | Ctrl+Tab | ✅ main.rs:823 |
| | prevPane | Ctrl+Shift+Tab | ✅ main.rs:824 |
| | selectPaneN | Ctrl+`1…9`（**combo.key 存的就是占位串 `'1…9'`**） | ✅ main.rs:836-842 |
| | focusLeft/Right/Up/Down | Alt+←/→/↑/↓ | ✅ main.rs:829-832 |
| `global` | switchProject | Ctrl+Shift+P | ❌（audit #26） |
| | globalSearch | Ctrl+Shift+F | ❌（audit #24） |
| | terminalSearch | Ctrl+F | ❌（audit #23 宿主接线未做） |
| | openSettings | Ctrl+, | ✅ main.rs:822 |
| | toggleSidebar | Ctrl+Shift+B | ✅ main.rs:820 |
| `aiTaskMarks` | markerPrev / markerNext | Ctrl+Shift+↑ / ↓ | ❌（audit #25） |
| `clipboard`（terminal scope） | copySelection | Ctrl+Shift+C | 终端右键菜单已做（N 批），键位待查 |
| | pasteToTerminal | Ctrl+Shift+V | 同上 |

**GPUI 额外有原版没有的三条**（main.rs:825-828，已在注释里标注「原版没有的三条，保留」）：
`ctrl-shift-a` ToggleSessions / `ctrl-shift-u` ToggleUsage / `ctrl-shift-j` JumpAttention。

**实现要求**：GPUI 侧没有 `hotkeys.ts` 的等价物——`main.rs:815-843` 是一串裸 `KeyBinding::new`，
没有分组、没有描述 key。本批需要**新建一个 Rust 侧的键位表模块**（如 `crates/mt-app/src/hotkeys.rs`），
让 `main.rs` 的 `bind_keys` 与设置页共用一份，重演原版「唯一事实来源」的结构。
未实现的那 5 条建议**照原版列出但标注「暂未实现」**，或直接不列（**不要列出来还绑不上**）；
额外三条要补进表里（描述文案原版字典里没有，见 §4 的「新增词条流程」）。

---

### 3.10 `about` —— 关于

原版 `AboutSettings`：SettingsModal.tsx:1566-1655。根节点 `space-y-6`，顶部一行
`settings.about.versionInfo` 标题（:1600-1602）。

1. **当前版本卡**（:1605-1608）：`flex items-center gap-3 px-4 py-3 rounded-[var(--radius-md)]
   bg-[var(--bg-base)] border border-[var(--border-subtle)]`，
   左 `settings.about.currentVersion`，右 `font-mono text-[var(--accent)]` 的 `v{version}`。
   版本来源 `@tauri-apps/api/app` 的 `getVersion()`（:1574）；
   GPUI 用 `env!("CARGO_PKG_VERSION")`（main.rs:903 已经这么取了）
2. **检查更新按钮**（:1611-1617）：全宽 `py-2.5 border rounded-[var(--radius-md)]`，
   `disabled:opacity-50 disabled:cursor-not-allowed`；忙时文案换 `settings.about.checking`
3. **错误条**（:1620-1624）：`border-[var(--color-error)]/30 text-[var(--color-error)]`
4. **结果卡**（:1626-1650）：有更新时 `border-[var(--accent)]/50`，否则 `border-[var(--border-subtle)]`
   - 有更新：`settings.about.newVersionFound` + `font-mono accent` 的版本号；
     下一行 `settings.about.publishedAt`（插值 `{date}`，
     **`new Date(publishedAt).toLocaleDateString('zh-CN')` —— 原版这里的 locale 是写死的**，:1635）；
     再下面全宽主按钮 `settings.about.downloadFromGitHub` → `openUrl(latest.url)`
   - 无更新：`settings.about.upToDate`
5. **`Hint`** `settings.about.footer`

`checkUpdate` 逻辑（:1577-1594）：清空错误与结果 → `checkForUpdate(currentVersion)` →
**返回 `null`（无新版）时也把 `latest` 设成当前版本**（:1587，注释：仍显示当前为最新）→
异常写 `settings.about.checkFailed`。`hasUpdate = compareVersions(latest, current) > 0`（:1596）。

`src/utils/updateChecker.ts`：
- `GITHUB_REPO = 'dreamlonglll/mini-term'`（:3）
- `fetch('https://api.github.com/repos/{repo}/releases/latest')`（:22）
- 404 → `updateChecker.noRelease`，其他非 2xx → `updateChecker.requestFailed`（插值 `{status}`）
- 取 `tag_name` / `html_url` / `published_at`（:26-28）
- `compareVersions`（:11-19）：去掉前导 `v`，按 `.` 分段数值比较，缺段按 0

> **GPUI 无 HTTP 客户端**：整个 workspace 只有 `mt-ai` 的 `tiny_http = "0.12"`（服务端）。
> 见 §7 坑 2。

---

## 4. i18n

### 4.1 好消息：settings 命名空间已全部就位

`crates/mt-i18n/src/dict.rs:923-1108`（`SETTINGS_ZH`）与 `:1110-...`（`SETTINGS_EN`），
命名空间注册在 `dict.rs:1821-1823`。

**已做过全量键集 diff：TS 侧 `src/i18n/locales/settings.ts` 与 Rust dict 各 184 条，两侧完全一致，零差异。**
也就是说本批**不需要新增任何 settings 词条**，`t("settings", "clipboard.smartCopyPasteTitle")`
这样直接用即可。

调用约定（`crates/mt-app/src/i18n.rs:35`，`pub use mt_i18n::{Locale, t, tr}`）：

```rust
t("settings", "clipboard.copyPaste")                       // -> &'static str
tr!("settings", "aiHook.serverRunning", port = 5577)       // -> String（插值）
tr!("settings", "aiHook.stateStale", n = 3, total = 16)
```

插值语义（mt-i18n/src/lib.rs:370-401）：`{name}` 占位符，**args 里没有的占位符原样保留**，
不支持嵌套与 `{{` 转义，与 TS 侧 `interpolate()` 一致。

### 4.2 必做：补 `USED_KEYS`

`crates/mt-app/src/i18n.rs:122-264` 是一张手工维护的表，两条测试盯着它：
- `用到的每个_key_两种语言都在`（:275-285）
- `key_清单有序且不重复`（:289-294）—— **必须字典序、去重**

现在表里只有 12 条 `settings.*`（:213-227）。本批新增的每一条 `t("settings", ...)` 调用点
都要往这张表里按字典序插一行。取全表的命令写在 i18n.rs:117-120 的文档注释里。

### 4.3 若确实需要新词条（如快捷键页那三条 GPUI 专属动作的描述）

**绝不能直接手改 `dict.rs`** —— 它是 `crates/mt-i18n/tools/gen_from_ts.mjs` 从 TS 生成的，
下次重生成就没了（i18n.rs:22-31 明写）。流程是：

1. 在 `src/i18n/locales/<ns>.ts` 的 zh 与 en 两侧同时加词条
2. `node crates/mt-i18n/tools/gen_from_ts.mjs`
3. 把 `crates/mt-i18n/tests/consistency.rs` 里的条目总数对账常量改成新数目
4. 往 `USED_KEYS` 里加

### 4.4 有两条词条在 GPUI 里注定用不上

`settings.aiNotification.audioFilter` 与 `settings.editor.executableFilter` ——
gpui 的文件选择框没有扩展名过滤（见 §7 坑 2）。**不要删词条**（TS 侧还在用，删了会让重生成对不上），
留着即可；或者改用途（例如变成「所选文件不是支持的格式」提示的一部分）。

---

## 5. 四项热更新的准确语义

audit #19 那句「连字/scrollback/UI 字号字族/终端字号热更新是缺口（现只作用于新终端）」
说的是 **GPUI 侧的现状**。**原版这四项全部是热更新的**，逐条钉死如下：

| 项 | 原版行为 | 证据 | GPUI 现状 | 本批要做 |
|---|---|---|---|---|
| `terminalScrollback` | **热更新全部已开终端**，调小时 xterm 当场裁掉多余历史并释放内存 | SettingsModal.tsx:626-631 调 `updateAllTerminalScrollback(v)`；实现在 terminalCache.ts:627-634 | **完全没有消费点**：`Term::new(Config::default(), ..)`（mt-terminal/src/lib.rs:109），alacritty 默认 `scrolling_history: 10000`（恰好与配置默认值相同，纯属巧合） | 建终端时把 `terminal_scrollback` 喂进 `alacritty_terminal::term::Config`；热更新走 `Term::set_options(Config)`（它内部会调 `grid.update_history`） |
| `terminalFontSize` | 热更新全部已开终端 | 见 `updateAllTerminalThemes` 同形的订阅链路 | **只作用于新建终端**——`AppStore::set_terminal_font_size`（store.rs:1264-1272）只写配置，注释 :1262-1263 自己写了「已开终端沿用创建时的样式」 | 给所有活着的 `TerminalView` 下发 `set_style(TerminalStyle{..})`（mt-ui/src/terminal/view.rs:386），与 `apply_theme_from_config`（store.rs:1168-1190）同形 |
| `terminalFontFamily` | 热更新，且用户字体会自动补 CJK 回退串 | terminalCache.ts:53-58 | **完全没有消费点**：`TerminalStyle::default()` 硬编码 `"Cascadia Mono"` + 4 条 fallback（mt-ui/src/terminal/theme.rs:120-135） | 同上，走 `set_style`；CJK 回退语义映射到 `TerminalStyle.font_fallbacks` |
| `uiFontSize` / `uiFontFamily` | 改 `html` 的 `fontSize` / 两个 CSS 变量，**全 UI 即时跟随** | SettingsModal.tsx:872；fontManager.ts:8-18 | **完全没有消费点**：`ui.rs` 里上百处 `px(12.0)` / `px(13.0)` 是硬编码字面量，没有任何字号/字族的全局来源 | 这是本批**最大的结构性工作量**：需要一个 `ui::font_scale()` / `ui::ui_font()` 的 thread_local 快照（与 `ui::set_palette` 同一模式，ui.rs:190-208），并把 `ui.rs` 与各视图里的字号改成走它 |
| `terminalLigatures` | 切开关后对每个已开终端重做 ligatures + WebGL 顺序（`reloadLigaturesForPty`，terminalCache.ts:514-522） | — | **底层无此能力**（见 §7 坑 1） | 见坑 1 |

另外 `terminalFollowTheme` / `theme` / `customThemeId` 的热更新在 GPUI 已经做好了：
`AppStore::apply_theme_from_config`（store.rs:1168-1190）会遍历 `self.terminals` 逐个下发新配色，
`set_theme_mode`（store.rs:1203-1209）/ `set_theme_pack`（store.rs:1215-1228）/
`set_terminal_follow_theme`（store.rs:1232-1244）三个 setter 也都写好了，
只是**都带 `#[allow(dead_code)] // 设置面板「外观」页的落点(下一批)`**——本批就是那个「下一批」。

三个 setter 的精确签名：

```rust
pub fn set_theme_mode(&mut self, mode: &str, window: &mut Window, cx: &mut Context<Self>)          // store.rs:1203
pub fn set_theme_pack(&mut self, theme_id: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> bool  // store.rs:1215
pub fn set_terminal_follow_theme(&mut self, follow: bool, window: &mut Window, cx: &mut Context<Self>)           // store.rs:1232
```

- `set_theme_mode` 内部**自己就会清 `custom_theme_id`**（store.rs:1205），与原版
  `handleThemeChange` 的 `clearCustomTheme()` 同语义，页面侧不必再清一遍。
- `set_theme_pack(None, ..)` = 退出皮肤；返回 `false` 表示装不上，**此时不落盘**
  （store.rs:1223-1225，注释：内存里已回落内置，配置里那条 `customThemeId` 不该被这次失败改掉）。
  页面侧据此弹 `settings.themes.applyFailed`。
- `set_terminal_follow_theme` 自带「值没变就直接 return」的短路（store.rs:1238-1240）。

---

## 6. 样式要点（只列对还原有决定性的）

### 6.1 CSS 变量取值（暗色 `:root`，`src/styles.css`）

| 变量 | 暗色 | 亮色 | GPUI `Palette` 字段 |
|---|---|---|---|
| `--bg-base` | `#080706`（:10） | `#ffffff`（:87） | `bg_base`（ui.rs:69/102） |
| `--bg-elevated` | `#1c1a18`（:12） | `#ebebeb`（:89） | `bg_elevated` |
| `--accent-muted` | `#c8805a33`（:18，即 accent @ 20%） | `#b0683033`（:94） | **缺，需补** |
| `--accent-subtle` | `#c8805a18`（:19，≈9.4%） | `#b0683018`（:95） | `accent_subtle`（暗色实现里用的是 `a: 0.10`，ui.rs:78-81，与 `0x18/255 ≈ 0.094` 有微小出入） |
| `--border-strong` | `rgba(255,255,255,0.12)`（:29） | `rgba(0,0,0,0.15)`（:103） | **缺，需补**（Toggle 关态、Modal 面板边框都要） |
| `--color-warning` | `#d4a84a`（:35） | `#b08620`（:108） | **缺，需补**（hook 页「旧版本 n/total」徽章） |
| `--radius-sm` | `4px`（:62） | 同 | ui.rs 里已按 `px(4.0)` 硬编码 |
| `--radius-md` | `6px`（:63） | 同 | — |
| `--app-font-family` | `'DM Sans Variable', 'DM Sans', system-ui, -apple-system, sans-serif`（:82） | — | — |
| `--app-font-mono` | `'JetBrains Mono', 'Cascadia Code', Consolas, monospace`（:83） | — | — |

其余（`--bg-surface` / `--bg-overlay` / `--text-*` / `--accent` / `--border-subtle` /
`--border-default` / `--color-success` / `--color-error`）在 `crates/mt-app/src/ui.rs:67-130`
已逐值抄好，直接用 `ui::accent()` 等函数即可（ui.rs:212-289）。

### 6.2 字号换算

`body { font-size: 13px }`（styles.css:131），但 Tailwind 的 `text-base = 1rem` 跟的是
`html` 的 inline `fontSize`（由 `uiFontSize` 设，`src/App.tsx:141`）。默认 13px 下：

| Tailwind | rem | 默认像素 |
|---|---|---|
| `text-lg`（Modal 标题） | 1.125rem | ≈14.6px |
| `text-base`（设置行标题 / 菜单项 / 输入框） | 1rem | 13px |
| `text-sm`（说明文字 / Hint / 小按钮） | 0.875rem | ≈11.4px |
| `text-xs`（LanguageToggle / 代码片段） | 0.75rem | ≈9.75px |

GPUI 现有代码里 `px(13.0)` 对应 `text-base`、`px(11.0)` 对应 `text-sm`、`px(12.0)` 是折中值。

### 6.3 常用间距

- 设置行：`px-3 py-2.5`（12px / 10px）、圆角 `--radius-md`(6px)
- 分节间距：页根 `space-y-6`（24px），节内 `space-y-2`（8px）
- 右栏内边距：`px-5 py-4`（20px / 16px）

---

## 7. 坑与边界

### 坑 1 —— 三项「UI 做得出、底层做不到」的功能

必须在动手前决定怎么处理，**不要做成看着能点、点了没反应**：

| 功能 | 底层缺口 | 证据 |
|---|---|---|
| 终端连字（`terminalLigatures`） | mt-ui 是自绘渲染器，**全仓 grep `ligature`/`calt` 只命中 mt-config 的字段定义本身** | `crates/mt-config/src/config.rs:85` 是唯一命中 |
| 内置皮肤 blueprint / fluent2（`skin`） | GPUI 侧没有内置皮肤色表，一律按 `none` 处理 | `crates/mt-app/src/theme.rs:31-35` 自述 |
| UI 字号 / 字族（`uiFontSize` / `uiFontFamily`） | `ui.rs` 上百处硬编码 `px()` 与隐式默认字体，没有全局来源 | `crates/mt-app/src/ui.rs:297-376` 等 |

建议：连字与皮肤两段**先不渲染**（或渲染但整段置灰 + 一句说明）；
UI 字号字族**建 thread_local 快照并真接上**（与 `ui::set_palette` 同一模式），
因为它是 audit #19 明确点名的四项热更新之一。

### 坑 2 —— 平台能力缺口（三个都在 `about` / `ai-notification` / `editor` 页上）

1. **没有 HTTP 客户端**：整个 workspace 只有 `mt-ai` 的 `tiny_http`（**服务端**）。
   `about` 页的「检查更新」要打 `api.github.com`，必须新增依赖（`ureq` 最轻、无 tokio；
   `reqwest` 会拖 tokio + TLS，注意 macOS 上已知的 `vendored-openssl` 坑）。
   **这是本批唯一需要动根 `Cargo.toml` 的地方，实现前先与主会话确认**。
   若不愿加依赖，`about` 页可以只做「当前版本 + 打开 GitHub Releases 页」（`cx.open_url`，gpui-0.2.2 app.rs:1078）。
2. **文件选择框没有扩展名过滤**：`gpui::PathPromptOptions` 只有
   `files` / `directories` / `multiple` / `prompt` 四个字段（gpui-0.2.2 platform.rs:1330-1339），
   原版提示音的 6 种格式过滤与编辑器的 `.exe` 过滤**都做不到**。
   只能选完之后自己校验扩展名并给提示。
3. **提示音能力比原版窄一档，且会静默降级**：`crates/mt-app/src/notify.rs:234-267`——
   自定义路径**只认 `.wav`**（`PlaySoundW` 的边界），其余一律回落 `MessageBeep(MB_OK)`；
   默认音也是 `MessageBeep` 而不是原版的 880Hz→660Hz 双音 WebAudio（`src/utils/notificationSound.ts:10-34`）。
   非 Windows 是空实现（notify.rs:266-267）。
   **叠加坑 2.2：用户在选择框里挑了个 mp3，看着设置成功了，实际每次响的都是系统提示音。**
   这一条 audit 已经记档（`docs/gpui-parity-audit.md:74`），本批至少要在选到非 `.wav` 时给出提示。

### 坑 3 —— 「UI 做了但没人消费」 vs 「后端已消费只差 UI」，两类必须分清

| 字段 | GPUI 后端消费 | 本批做 UI 后的实际效果 |
|---|---|---|
| `aiCompletionPopup` / `TaskbarFlash` / `Sound` / `SoundPath` / `aiAttentionNotify` | ✅ store.rs:1032, 1062-1065 → notify.rs:117-164 | **立刻生效** |
| `aiAutoResume` | ✅ terminal_area.rs:364、store.rs:702 | **立刻生效** |
| `editors` / `defaultEditor` | ✅ file_tree.rs:194,203 → mt-project/editor.rs:26-34 | **立刻生效** |
| `selectionAutoCopySecs` | ✅ store.rs:884-886 → `TerminalView::set_selection_dwell` | 生效，但**要记得给存量终端下发**（store.rs:879-883 的注释已写明） |
| `hookEnabled` | 🟡 仅启动时读（main.rs:868 / ai.rs:70-74） | 需在 `AiBridge` 上补运行时开关直通 |
| `theme` / `customThemeId` / `terminalFollowTheme` | ✅ store.rs:1203/1215/1232 三个 setter + `apply_theme_from_config`（:1168）全就绪（现带 dead_code） | **立刻生效** |
| `skin` | ❌ theme.rs 按 none 处理 | **无效果** |
| `terminalScrollback` / `terminalFontFamily` / `terminalLigatures` / `uiFontSize` / `uiFontFamily` | ❌ 零消费点 | **无效果**，除非本批一并接上（见 §5） |
| `smartCopyPaste` | ❌ 零消费点 | **无效果**（终端剪贴板批） |
| `longPasteToFile` / `LineThreshold` / `CharThreshold` | ❌ 零消费点 | **无效果**（audit #30） |
| `remotePasteDir` | ❌ 零消费点 | **无效果**（audit #28，SSH 未迁移） |
| `trayStatusEnabled` / `trayMaxProjects` / `trayClickFocus` | ❌ 零消费点 | **无效果**（audit #21） |

建议：无效果的那几项**照原版做出来**（字段与磁盘格式都已在，做出来不亏），
但在 `docs/gpui-parity-audit.md` 里对应条目注明「设置项已就位，功能待 #NN」。

### 坑 4 —— 不要用 gpui-component 的 `Switch` / `setting` 模块

- `gpui_component::switch::Switch` 的配色走**它自己的 theme token**
  （`cx.theme().primary` / `switch` / `switch_thumb`，switch.rs:96-99），与壳的 `ui::Palette` 对不上。
  几何倒是恰好一致（`Size::Medium` = 36×20px，thumb 16px，与原版 `w-9 h-5` + `w-4 h-4` 一模一样），
  所以自绘的工作量极小。
- `gpui_component::setting`（`SettingPage` / `SettingGroup` / `SettingItem` / `fields/*`）
  是一整套**有自己布局语言 + reset 按钮 + rust_i18n** 的设置框架，与原版的
  「左侧两级侧栏 + 自定义行」完全不同形，硬套只会打架。
- 这与 N 批对 `ContextMenu` 的判断是同一类问题（见 `docs/gpui-parity-audit.md:32` 记档的四条硬伤），
  **结论一致：自绘**。可以复用 `gpui_component::kbd`（快捷键页）与 `input`（已在用）。

### 坑 5 —— `NumberRow` 的草稿语义

原版注释（SettingsModal.tsx:167-171）：**边打字边 clamp 会让「1000」在敲到「1」时就被吃掉**。
所以必须：输入期间只改草稿 → 失焦/回车才归一 → 归一失败回落已保存值 → 只有变了才提交。
GPUI 的 `InputState` 需要挂 blur 与 Enter 两个提交点。
`FontSizeSlider` 则相反，是**拖动即时提交**（:771）——两种控件语义不同，别统一。

### 坑 6 —— 保存失败与只读模式

- `AppStore` 加载配置失败时 `token = 0`，**后续所有保存都被自己挡下**（store.rs:117-123, 1481-1483），
  这是防「一次读盘故障把用户项目列表清空」的红线。设置页在这种状态下**改什么都不会落盘且没有提示**——
  原版也是同样的静默（`saveConfigToDisk` 无 UI 反馈）。本批不改这个语义，但要知道它在。
- 落盘防抖 500ms（store.rs:1459-1473），代号 `save_generation` 只有最后一次排上的任务真写盘。
  滑块连续拖动天然被这个防抖吃掉，不必额外节流。

### 坑 7 —— 焦点与弹窗叠开

- `open_guarded`（prompt.rs:47）按 `kind` 防叠开；设置页用 `kind::SETTINGS`（prompt.rs:71）。
  **同一种类不可叠、不同种类可叠**——所以设置页里再弹 confirm（删皮肤）是允许的。
- N 批的菜单基建已经确立了「焦点开时收走、关时先还回去再跑动作」的规矩（防动作里开的输入框被反抢光标），
  设置页里凡是「点按钮 → 弹另一个对话框 / 打开输入框」的路径都要遵守。

### 坑 8 —— `terminal` 页与 `font` 页的字号重复

GPUI 现在的设置对话框把「终端字号 −/+」放在 shell 页（modal.rs:277-313），
并配了一条原版没有的提示 `settings.terminal.fontSizeNewOnly`（「改动作用于新建的终端」，modal.rs:308-310）。
本批把字号挪进 `font` 页并改成滑块之后，**那条提示词条就该删掉调用点**
（词条本身留在字典里，删调用点即可；同时从 `USED_KEYS` 里摘掉 `settings.terminal.fontSizeNewOnly`）。

---

## 8. GPUI 侧现状汇总 + 差异清单

### 8.1 现有设置面板全貌

`crates/mt-app/src/modal.rs:109-448`，单页 `Dialog`，`w(px(560.0))`，标题 `settings.title`。三段：

1. `render_language_section`（:317-373）—— 对应原版 `appearance` 页第 1 节，**已完全对齐**
2. shell 列表（:122-276）—— 对应原版 `terminal` 页第 1 节，**已对齐**（差 Section 2「终端行为」）
3. 终端字号 `−` / `44px 宽数字` / `+`（:277-313）—— 对应原版 `font` 页的滑块，**形态不同**

用到的 12 条 settings key 见 i18n.rs:213-227。

### 8.2 差异清单（按「本批要补什么」排序）

| # | 差异 | 原版落点 | GPUI 落点 | 规模 |
|---|---|---|---|---|
| 1 | 无分页外壳（两级侧栏 + 10 页 + ↑↓ 导航） | SettingsModal.tsx:2084-2170 | modal.rs:109 | 中 |
| 2 | 无通用原语（Toggle / NumberRow / ChoiceGroup / Slider / SettingRow / Section / Hint） | SettingsModal.tsx:52-256, 744-778 | ui.rs（只有 3 个按钮 + section_title） | 中 |
| 3 | `Palette` 缺 `border_strong` / `accent_muted` / `color_warning` | styles.css:29, 18, 35 | ui.rs:36-55 | 小 |
| 4 | clipboard 页 6 项全缺 | :640-740 | — | 中 |
| 5 | appearance 页主题/皮肤/跟随/外置皮肤四段全缺（setter 已就绪） | :782-859, 1798-2013 | store.rs:1203 `set_theme_mode` / :1215 `set_theme_pack` / :1232 `set_terminal_follow_theme`（均 dead_code）、theme.rs:87 `list_packs`（dead_code） | 大 |
| 6 | font 页全缺（现有 −/+ 按钮要改滑块） | :863-941 | modal.rs:277-313 | 中 |
| 7 | ai-notification 页 6 项全缺（后端已消费） | :1257-1344 | store.rs:1062-1065 | 中 |
| 8 | ai-hook 页全缺（mt-ai API 全就绪，只差 `AiBridge` 两个直通方法） | :986-1253 | hook_registry.rs:910-957、perception.rs:137 | 大 |
| 9 | system 页 4 项全缺（托盘三项无消费方） | :1348-1398 | — | 小 |
| 10 | editor 页全缺（后端已消费） | :1402-1562 | file_tree.rs:194-203 | 中 |
| 11 | shortcuts 页全缺，且**没有键位表模块**（main.rs 是裸 KeyBinding 串） | :1665-1702、hotkeys.ts:48-79 | main.rs:815-843 | 中 |
| 12 | about 页全缺，且**没有 HTTP 客户端** | :1566-1655、updateChecker.ts | — | 中 |
| 13 | 四项热更新（scrollback / 终端字号 / 终端字族 / UI 字号字族） | §5 表 | §5 表 | 大 |
| 14 | `settings.terminal.fontSizeNewOnly` 是 GPUI 独有的降级提示，接上热更新后应删调用点 | — | modal.rs:308-310 | 小 |

### 8.3 建议实施顺序

1. **原语 + Palette 三 token + 分页外壳**（差异 1/2/3）——不接任何字段，先把壳跑起来
2. **纯配置页**：`system` → `clipboard` → `editor` → `ai-notification`（差异 4/7/9/10）——
   全是 Toggle/Number/文本行，验证原语
3. **`appearance`**（差异 5）——setter 全就绪，主要工作量在外置皮肤卡片的预览绘制
4. **`font` + 四项热更新**（差异 6/13）——最大的一块，涉及 mt-ui `set_style` 下发与 `ui::` 字号来源
5. **`ai-hook`**（差异 8）——需要 `AiBridge` 补两个方法 + 后台执行器
6. **`shortcuts`**（差异 11）——需要新建键位表模块并让 `main.rs` 改用它
7. **`about`**（差异 12）——HTTP 依赖要先与主会话确认

### 8.4 测试要求（沿用本项目惯例）

- 既有断言零改动
- 新增单测至少覆盖：
  - `NumberRow` 归一规则（含 `selectionAutoCopySecs` 的 `0 / <0.2 / >60 / 非法` 四分支）
  - `resolveScrollback` 等价物的钳制（0 / 负 / 超上限 / NaN）
  - appearance 三字段联动（切主题清 `customThemeId`、`ChoiceGroup` 在皮肤激活时全不选中）
  - hook 页默认勾选逻辑（一家已装 → 只勾那家；一家都没装 → 全勾）
  - `system` 页托盘子项在总开关关闭时**不渲染**（而非置灰）
  - 分页 id 字符串与原版一致（防深链失效）
- `crates/mt-app` 与 `crates/mt-i18n` 全绿；`cargo test -p mt-app` 里
  `key_清单有序且不重复` 会盯着 `USED_KEYS` 的字典序
