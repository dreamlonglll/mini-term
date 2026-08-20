# 批规格：拖放基建 / 项目分组 / 三列表补全（audit #8 #13 #12 #14 #15 #9 残项）

> 2026-08-19 由规格提取 agent 逐文件对照产出。基线：Tauri 前端 `src/`（权威），
> 对照 GPUI `crates/`。**所有行号以提取时源码为准，实现前回读核对**（工作树里有并行
> 批次的未提交改动，`src/` 不受影响，`crates/mt-app/` 可能已前移）。
>
> 覆盖 `docs/gpui-parity-audit.md` 的：**#8 拖放基建**（❌ 整条）、**#13 项目分组**（❌ 整条）、
> **#12 项目列表**剩余项、**#14 文件树**剩余项、**#15 tab 栏**剩余项、**#9 图标体系**遗留
> 的 dirKinds 探测与文件树 git 着色。已完成部分（N 批四处右键菜单、M 批图标消费、
> K 批 StatusDot/FileIcon/TechIcon）不重复展开，只标「已在，需改哪里」。
>
> **rem 换算通则**（下文所有 Tailwind 尺寸）：`document.documentElement.style.fontSize =
> config.uiFontSize`（App.tsx:140-141，默认 13）。故 `text-base`=13px、`text-sm`≈11.4px、
> `text-xs`≈9.75px、`gap-2`=8px、`py-1.5`=6px 皆按 13px 基准算，**不是** 16px。
> GPUI 侧现在逐处硬编码 px，本批不改这个决定，但换算基准要按 13 来对。

---

## A. 拖放基建（audit #8）

原版三条链路互相独立，**两条自研鼠标事件 + 一条 Tauri 原生事件**。之所以自研，
根因写在 `src/utils/fileDragState.ts:1-6` 与 `src/utils/projectDragState.ts:1-6`：
**Tauri v2 在 Windows/WebView2 上开 `dragDropEnabled` 后，OLE 原生拖放会吃掉内部
HTML5 `dragover`/`drop`**，于是所有窗口内部拖拽一律改 `mousedown/mousemove/mouseup`。
**这个约束在 GPUI 侧完全不存在** —— gpui 把外部文件拖入也翻译成了内部 drag（见 A.4），
两套可以合并成一套，这是本批最大的简化点。

### A.1 链路①：项目列表内拖拽排序（项目 + 分组，同一套）

| 项 | 事实 | 源 |
|---|---|---|
| 机制 | 自研 pointer（**不是** HTML5 dnd）。模块级单例 `_payload/_dragging/_cleanup` | `src/utils/projectDragState.ts:12-14` |
| 拖拽把手 | **无把手，整行可拖**。`mousedown` 左键即 `initProjectDrag`；`(e.target).closest('input')` 命中时不起拖（重命名输入框内选字不该变成拖行） | `ProjectList.tsx:500-518` |
| 起拖阈值 | **曼哈顿距离 > 5px**（`abs(dx)+abs(dy)>5`），越过阈值才置 `_dragging=true` | `projectDragState.ts:46` |
| 拖起反馈 | 源元素 `style.opacity='0.4'`；`document.body` 加 class `project-dragging` | `projectDragState.ts:48-50` |
| 光标 | `body.project-dragging { cursor: grabbing !important; user-select: none !important }`（与 `file-dragging`/`ssh-conn-dragging` 共用一条规则） | `styles.css:167-171` |
| 拖影 | **没有**跟随鼠标的幽灵元素，只有源行变淡 | — |
| 落点判定 | 目标行 `mousemove` 取 `ratio = (clientY - rect.top)/rect.height`：分组行（`allowInside=true`）`0.25<ratio<0.75` → `inside`，否则 `<0.5` → `before` / 其余 → `after`；项目行 `allowInside=false`，只有 before/after | `ProjectList.tsx:520-561` |
| 合法性 | `inside` 走 `canDrop(tree,targetGroupId,dragged)`；组的 before/after 走 `canDropAt`。两者都判「不能落进自己的后代」+「`targetDepth+1+subtreeMaxDepth<=MAX_DEPTH`」，`MAX_DEPTH=3` | `utils/projectTree.ts:3,63-90` |
| 非法反馈 | `dropIndicator.forbidden=true`：分组行渲染 `border border-dashed border-[var(--color-error)] cursor-not-allowed`，**指示线不画**（`renderDropLine` 遇 forbidden 直接 return null） | `ProjectList.tsx:603-610,939-941` |
| 指示线样式 | `absolute left-1 right-1 h-0.5 bg-[var(--accent)] rounded-full z-10`，before 时 `top:-1`、after 时 `bottom:-1`。h-0.5 = 2px | `ProjectList.tsx:606-609` |
| inside 高亮 | 分组行 `bg-[var(--accent-subtle)] border border-dashed border-[var(--accent)]` | `ProjectList.tsx:942` |
| 落地 | `mouseup` 在目标行上 → `handleMouseUpDrop`。`inside` → `moveItem(itemId, targetId)`；before/after → 算 `parentGroupId=findParentGroupId(tree,targetId)`、`targetIdx`、`insertIdx = after? idx+1 : idx`，**若被拖项原本在同一父级且下标更小则 `insertIdx--`**（补偿先删后插的位移），再 `moveItem(itemId, parentGroupId ?? null, insertIdx)`。末尾 `saveConfig()` | `ProjectList.tsx:568-599` |
| click 抑制 | 真拖过的话，`mouseup` 时挂一次性 capture 的 `click` 监听 `stopPropagation+preventDefault`，防止松手顺带切项目 / 折叠分组 | `projectDragState.ts:60-68` |
| 子项目豁免 | `isChild`（`parentProjectId` 有值的 worktree 子项目）**不作为落点**：`onMouseMove/onMouseUp` 直接不挂（位置是派生的），但**自身可拖走** = 脱离父项目 | `ProjectList.tsx:679-684` |
| 收尾 | `onProjectDragEnd(() => setDropIndicator(null))` 注册在每次 mousedown，`onUp` 里统一调 | `ProjectList.tsx:507,517`；`projectDragState.ts:71` |

### A.2 链路②：从资源管理器拖文件夹加项目

| 项 | 事实 | 源 |
|---|---|---|
| 事件源 | `getCurrentWebview().onDragDropEvent(...)`，payload 四态 `enter / over / drop / <其他=leave>` | `ProjectList.tsx:321-351` |
| 命中判定 | `isOverProjectList(position)`：`position` 是**物理像素**，要先 `/ (window.devicePixelRatio||1)` 再和 `getBoundingClientRect()` 比。**GPUI 侧这步不需要**（gpui 已换算成逻辑 px，见 A.4） | `ProjectList.tsx:286-293` |
| 三态判定 | `enter` 时异步 `invoke('filter_directories',{paths})`：空 → `forbidden`；全部已在 `config.projects` 里 → `duplicate`；否则 `valid` | `ProjectList.tsx:326-335` |
| 高亮框 | `absolute inset-0 z-20 border-2 border-dashed rounded-[var(--radius-md)] pointer-events-none`，居中一行 `text-sm font-medium`。配色：valid=`bg-[var(--accent)]/10 border-[var(--accent)]`；forbidden=`--color-error` 同构；duplicate=`--color-warning,#f59e0b` 同构 | `ProjectList.tsx:1044-1066` |
| 文案 | `projectList.dragHint.valid` 「拖放文件夹以添加项目」/ `.forbidden`「仅支持拖入文件夹」/ `.duplicate`「项目已存在，松手切换」 | dict.rs 参见 A.6 |
| drop 语义 | `filter_directories` 过滤出目录；逐个：已存在 → 记 `existingId` 跳过；否则 `addProject({id:genId(),name:basename,path})`。**新增过任何一个 → 只 `saveConfig()` 不切换**；一个没新增但有重复 → `setActiveProject(existingId)` | `ProjectList.tsx:295-319` |
| 后端 | `filter_directories(paths) -> Vec<PathBuf>`（保留是目录的那些） | `crates/mt-project/src/fs.rs:270`（已在 GPUI 侧就绪） |

### A.3 链路③：拖文件进终端（两个来源）

**来源 A：外部资源管理器**（`src/hooks/useExternalFileDrop.ts`，App.tsx:383 挂一次，全局单例）

- 命中：`document.elementFromPoint(x/scale, y/scale)` → `closest('[data-pty-id]')` → 读属性拿 ptyId（`useExternalFileDrop.ts:5-12`）
- 高亮：模块内 `hoveredPtyId` 变化时派发 `CustomEvent('external-file-drag',{detail:ptyId})`，各 `TerminalInstance` 监听并比对自己的 ptyId（`useExternalFileDrop.ts:20-23`；`TerminalInstance.tsx:220-226`）
- 去抖：`drop` 距上次 `<100ms` 直接丢弃（同一次拖放事件重入保护，`useExternalFileDrop.ts:35-37`）
- 写入：**多路径**`paths.map(p => `'${p}'`).join(' ')` → `writePtyInput(ptyId, formatted)` → `term.focus()`（`useExternalFileDrop.ts:41-43`）

**来源 B：内部 FileTree**（自研 pointer）

- 起拖：文件树行 `onMouseDown` 左键 → `initFileDrag(entry.path, x, y)`（`FileTree.tsx:326-328`）
- 阈值同为 **5px 曼哈顿**；置 `body.file-dragging`（`fileDragState.ts:45-51`）
- **穿透**：`body.file-dragging [data-terminal-drop] > * { pointer-events: none !important }` —— 让鼠标事件穿过 xterm 打到 drop-zone 上（`styles.css:163-165`）
- Esc 取消：window 级 **capture** keydown（抢在 xterm 的 textarea keydown 之前，否则这次 Esc 会被写成 `\x1b` 进 PTY）；只有真拖起来了才 `preventDefault+stopPropagation` 并派发 `FILE_DRAG_CANCEL_EVENT`（`fileDragState.ts:54-69`）
- 高亮维护：`onMouseMove` 里 `isFileDragging()` → `setFileDrag(true)`；`onMouseLeave` 撤；`FILE_DRAG_CANCEL_EVENT` 也撤（鼠标不动收不到事件）（`TerminalInstance.tsx:231-237,320-326`）
- 落地：`onMouseUp` → `getFileDragPath()` 有值 → `writePtyInput(ptyId, `'${path}'`)` + `term.focus()`（`TerminalInstance.tsx:328-335`）

**引号规则（两来源一致，逐字）**：单引号包裹，**不做任何转义**，多路径用单个空格 join。
即 `'D:\a b\c.txt'`。含单引号的文件名会破（原版已知缺陷，**照抄不修**）。

**虚线高亮框样式**（`TerminalInstance.tsx:430-442`）：
```
absolute inset-1 z-10 flex items-center justify-center pointer-events-none rounded-[var(--radius-md)]
style: background: var(--accent-subtle); border: 2px dashed var(--accent)
内层文字: text-[var(--accent)] text-xs px-3 py-1.5 rounded-[var(--radius-md)]，background: var(--bg-overlay)
文案 key: terminal.dropToInsertPath
```
`--accent-subtle` dark=`#c8805a18` / light=`#b0683018`；`--radius-md` dark/light=6px，blueprint/fluent2=3px（`styles.css:19,63,95,1003,1052`）。

### A.4 GPUI 0.2.2 拖放 API 调查（源码实证）

读的是 `~/.cargo/registry/src/index.crates.io-*/gpui-0.2.2/`。

**内部拖放（元素级）** —— `src/elements/div.rs`：

| API | 签名 | 位置 |
|---|---|---|
| `on_drag<T,W>` | `(value: T, constructor: impl Fn(&T, Point<Pixels>, &mut Window, &mut App) -> Entity<W>) -> Self`，`W: Render` | div.rs:1132（需 `StatefulInteractiveElement`，即元素必须 `.id()`） |
| `on_drag_move<T>` | `(impl Fn(&DragMoveEvent<T>, &mut Window, &mut App))` | div.rs:818 |
| `DragMoveEvent<T>` | 字段 `event: MouseMoveEvent`、`bounds: Bounds<Pixels>`（= 本元素 hitbox 矩形）；方法 `drag(&App) -> &T` | div.rs:62-85 |
| `on_drop<T>` | `(impl Fn(&T, &mut Window, &mut App))` —— **不带位置**，落点即「鼠标抬起时命中的那个元素」 | div.rs:976 |
| `can_drop` | `(impl Fn(&dyn Any, &mut Window, &mut App) -> bool)` —— 谓词为假则该元素不吃这次 drop | div.rs:986 |
| `drag_over<S>` | `(impl Fn(StyleRefinement, &S, &mut Window, &mut App) -> StyleRefinement)` —— 拖着 `S` 悬停时的样式 | div.rs:938 |
| `group_drag_over<S>` | 同上但作用于 group 名 | div.rs:956 |

关键实现细节（写代码前必须知道）：

1. **起拖阈值 `DRAG_THRESHOLD = 2.0`，欧氏距离**（div.rs:47,2159）。原版是 **5px 曼哈顿**。差异会让拖拽更"灵敏"，如需对齐得自己在 `on_drag` 的 constructor 里再加一层，或接受差异（建议接受并记档）。
2. **拖起自动吃掉 click**：`clicked_state` 被重置（div.rs:2166），且 MouseUp 后 `cx.active_drag=None`（window.rs:3721-3726）。**原版那套「一次性 capture click 抑制」在 GPUI 完全不需要**。
3. `on_drop` 的分发在 **MouseUp 的 Bubble 阶段**，条件 `hitbox.is_hovered(window)`，命中后 `cx.stop_propagation()`（div.rs:2089-2119）。→ 嵌套 drop 目标（行 in 列表容器）会按命中顺序竞争，**内层先吃到就不再冒泡**，正好是我们要的语义。
4. `on_drag_move` 只在 **Capture 阶段**触发，且要求 `active_drag.value` 的 TypeId 精确等于 `T`（div.rs:289-305）。→ 判 before/inside/after 就用它：`(event.event.position.y - event.bounds.origin.y) / event.bounds.size.height` 即原版的 `ratio`。

**外部文件拖入** —— 这是本次调查最重要的发现：

- `ExternalPaths(pub(crate) SmallVec<[PathBuf;2]>)`，只有 `paths() -> &[PathBuf]`；`impl Render for ExternalPaths` 返回 `Empty`（注释：平台自己画文件图标）。（interactive.rs:495-510）
- `FileDropEvent` 四态 `Entered{position,paths} / Pending{position} / Submit{position} / Exited`（interactive.rs:513-535）
- **gpui 在 `window.rs:3620-3661` 把外部拖放翻译成内部 drag**：`Entered` 时若 `active_drag.is_none()` 就造一个 `AnyDrag{ value: Arc::new(paths), view: cx.new(|_| paths).into() }`，然后把事件改写成 `MouseMove{pressed_button: Some(Left)}`；`Pending`→MouseMove；`Submit`→`MouseUp{Left}`（并 `cx.activate(true)`）；`Exited`→清 `active_drag`。
- **结论：`.on_drop::<ExternalPaths>(...)` / `.drag_over::<ExternalPaths>(...)` / `.on_drag_move::<ExternalPaths>(...)` 对外部文件拖入直接可用，与内部拖拽是同一套 API。**
- Windows 侧 `RegisterDragDrop` 是**建窗时无条件注册**的（platform/windows/window.rs:1240-1246），只认 `CF_HDROP`（同文件 881-935），坐标已 `ScreenToClient` + `logical_point(scale_factor)` 换算成逻辑 px。→ **原版那段 `position / devicePixelRatio` 的手工换算 GPUI 侧删掉**。

**gpui-component 0.5.1 有无现成可用**：

- `dock/tab_panel.rs` 是最完整的 drag-reorder 参考实现（`on_drag` + `drag_over::<DragPanel>` + `on_drop` + `on_drag_move` 算插入位，tab_panel.rs:666,755-801,864,898-924）——**当参考代码抄，不当依赖用**（它绑死自己的 DockArea 模型）。
- `tree.rs`（534 行）有 `TreeState`（items/expanded/selected_index/scroll_to_item）与**内建 up/down/left/right 键盘导航**（tree.rs:20-23,277-335），但**没有拖放**，且 `TreeItem` 是 `{id,label,children}` 固定模型、无懒加载。→ 项目分组树与文件树都**不建议**直接用；键盘导航的 action 命名与行为可以照抄。
- 没有现成的「排序列表 / drop indicator」组件。

### A.5 方案建议

**统一成一套 gpui 原生 drag，删掉原版两套自研鼠标事件的全部脚手架。**

1. 定义三个拖拽载荷类型（放 `crates/mt-app/src/dnd.rs` 新模块）：
   - `DragProjectItem { id: String, is_group: bool }`（链路①）
   - `DragFilePath(PathBuf)`（链路③来源 B）
   - 外部文件直接用 `gpui::ExternalPaths`（链路②③来源 A 共用）
2. **拖影**：`on_drag` 的 constructor 返回一个渲染「行名 + 图标」的小 Entity（gpui 会跟着鼠标画）。原版没有拖影只有源行变淡 —— 建议**两个都做**（源行 `.opacity(0.4)` 靠 `cx.active_drag` 判定，拖影用 gpui 免费给的），比原版更好且零成本。
3. **落点指示**：`on_drag_move::<DragProjectItem>` 存 `DropIndicator{id,position,forbidden}` 进 ProjectList 的 view state（与原版 `useState` 同构），行渲染时按它画 2px accent 线 / inside 虚线框；`on_drop` 里读同一个 state 落地。**`on_drop` 不带位置，位置必须来自上一次 drag_move —— 这是硬约束。**
4. **`can_drop` 只用来做类型闸**（比如终端只吃 `DragFilePath` 与 `ExternalPaths`），**合法性（深度/循环）仍在 drag_move 里算并存进 indicator**，因为 `can_drop` 拿不到位置、算不出 before/inside/after。
5. **链路②的三态判定**：原版是异步 `filter_directories`。GPUI 侧 `Path::is_dir()` 是同步 stat，在 `on_drag_move` 里逐帧调**会在网络盘上卡主线程**。建议：`ExternalPaths` 的 `Entered` 只到一次（gpui 造 active_drag 那一下），拿 `cx.background_executor().spawn` 跑一次 `mt_project::fs::filter_directories`，结果回主线程存进 view state，drag_move 只读缓存。
6. **链路③的引号与多路径**照抄（单引号裹、空格 join、不转义），写入走 `AppStore::write_to_pane`（store.rs:618-639，它刻意走 `TerminalPane::write` 以保住 AI 输入检测那条链路，**不许改成裸 PTY 写**）。
7. **`body.file-dragging` 那条 `pointer-events:none` 穿透规则不需要移植** —— GPUI 侧终端是自绘 Element，drop 目标就是它的容器 div，没有 xterm 那样的子 DOM 挡路。
8. **Esc 取消**：gpui 没有内建。若要保留，在有 active_drag 时于 Workspace 层拦 `escape` action 并 `cx.active_drag.take()`。**注意 pane 已绑了终端 Esc 透传，需要同源判定**（参考 N 批 `prefers_local_handling` 的做法）。原版这条只对内部文件拖拽有效，**可以降级为"本批不做，记档"**。

### A.6 i18n 结论

| key | 结论 |
|---|---|
| `projectList.dragHint.valid` / `.forbidden` / `.duplicate` | **已在** dict.rs:637-639(zh) / EN 对应位 |
| `terminal.dropToInsertPath` | **已在** dict.rs:1427(zh) / 1438(en) |
| 其余拖放路径无新文案 | — |

### A.7 GPUI 现状差异

- `crates/mt-app/src/file_tree.rs:11` 模块注释白纸黑字写着「文件拖进终端本轮不做，gpui 的拖放另开一批」——就是这一批。
- `project_list.rs` 零拖放代码；`pane.rs` 零 drop 目标；`main.rs` 无窗口级 file drop 处理。
- `crates/mt-project/src/fs.rs:270 filter_directories` 已就绪，签名 `(Vec<PathBuf>) -> Vec<PathBuf>`（Tauri 版是 `Vec<String>`）。

---

## B. 项目分组（audit #13）

### B.1 数据结构与持久化格式（好消息：两侧已经一模一样）

| | Tauri (`src/types.ts`) | GPUI (`crates/mt-config/src/config.rs`) |
|---|---|---|
| 树项 | `type ProjectTreeItem = string \| ProjectGroup`（types.ts:3） | `enum ProjectTreeItem { ProjectId(String), Group(ProjectGroup) }`，`#[serde(untagged)]`，**variant 顺序不可换**（config.rs:38-44） |
| 分组 | `{id,name,collapsed,children}`（types.ts:5-10） | `ProjectGroup{id,name,collapsed,children}` + `rename_all="camelCase"`（config.rs:46-53） |
| 配置字段 | `projectTree?: ProjectTreeItem[]`（types.ts:14） | `project_tree: Option<Vec<ProjectTreeItem>>` + `skip_serializing_if`（config.rs:69） |
| 旧格式 | `projectGroups?/projectOrdering?` 仅迁移用（types.ts:16-17） | `project_groups: Option<Vec<OldProjectGroup>>` / `project_ordering`（config.rs:70-73），迁移函数 config.rs:646-676 |
| 子项目 | `ProjectConfig.parentProjectId?`，**不进 projectTree**（types.ts:188-189） | `parent_project_id: Option<String>`（config.rs:387-389） |

**结论：磁盘格式零改动，GPUI 侧的数据层已经完整，纯粹是渲染层与 action 层没写。**
序列化已有回归测试钉住（config.rs:1500-1520）。

### B.2 分组行渲染（`ProjectList.tsx:928-1038`）

```
容器: relative（承载 before/after 指示线）
行:  flex items-center gap-1.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-sm
     transition-all duration-150 select-none
     paddingLeft = depth*16 px；paddingRight = 10px
     常态色: text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]
     inside 落点: bg-[var(--accent-subtle)] border border-dashed border-[var(--accent)]
     inside 非法: border border-dashed border-[var(--color-error)] cursor-not-allowed
a11y: role="treeitem" aria-expanded={!collapsed} tabIndex=0
```
子元素顺序：
1. **折叠箭头**：`<span class="text-xs w-3 text-center transition-transform duration-150">▾</span>`，
   collapsed 时 `transform: rotate(-90deg)`。**是文本 ▾ 不是 SVG**（ProjectList.tsx:1010-1013）
2. **Boxes 图标**：`size=13 strokeWidth=1.5`，色 `--color-folder`（「分组=空间」语义，ProjectList.tsx:1015）
3. **组名**：`truncate flex-1 font-medium`；编辑态换成内联 input（见 C.1）
4. **计数徽章**：`<span class="text-xs text-[var(--text-muted)]">({count})</span>`，
   `count = countProjectsInGroup(group)` = **递归含子组内项目**（projectTree.ts:336-346）。
   **不是徽章底色的 pill，就是括号里一个数字。**

`gap-1.5`=6px，`py-1.5`=6px，`text-sm`≈11.4px，`text-xs`≈9.75px（13px 基准）。
`--color-folder` dark=`#d4c8a0` / light=`#8a7a40` / blueprint=`#93c5fd` / fluent2=`#0284c7`（styles.css:33,106,986,1035）。

### B.3 五个 store action 的完整语义

全在 `src/store.ts:1266-1313`，树操作函数在 `src/utils/projectTree.ts`。
**共同前置：所有树操作就地改，调用前必须 `deepCloneTree`**（projectTree.ts:94-100）。

| action | 语义 | 边界 |
|---|---|---|
| `createGroup(name, parentGroupId?)` | `ensureTree(config)` → 新建 `{id:genId(),name,collapsed:false,children:[]}` → `insertIntoTree(tree, parentGroupId ?? null, group)`（**追加到末尾**，不传 index） | parentGroupId 找不到时 `insertIntoTree` 返回 false → **静默丢弃**（store.ts:1266-1273） |
| `removeGroup(groupId)` | `removeGroupAndPromoteChildren`：`tree.splice(i, 1, ...item.children)` —— **组员（含子组）原位晋升到父级**，一个都不删 | 递归查找；组不存在则无操作（projectTree.ts:164-176） |
| `renameGroup(groupId, name)` | `updateGroupInTree(tree, id, g => ({...g, name}))` | 空名由调用方 `name.trim()` 过滤（ProjectList.tsx:439-444） |
| `toggleGroupCollapse(groupId)` | 同上，翻 `collapsed` | 折叠只影响渲染，`getProjectsWithGroupPath`（移动端快照）**刻意不跳过**折叠组（projectTree.ts:290-295） |
| `moveItem(itemId, targetGroupId\|null, index?)` | `removeFromTree` → `insertIntoTree(tree, targetGroupId, removed, index)`。**若 `removeFromTree` 落空**（= 该 id 是 worktree 子项目，本就不在树里）→ 把它的 `parentProjectId` 清成 `undefined` 并把裸 id 当节点插进去 = **脱离父项目转普通树节点** | 找不到且不是子项目 → `return state` 原样返回（store.ts:1296-1313） |

**嵌套允许**：允许，上限 `MAX_DEPTH = 3`（projectTree.ts:3）。
判据 `canDrop`：`targetDepth + 1 + getSubtreeMaxDepth(dragged) <= 3`，
其中 `getSubtreeMaxDepth`：项目=0、空组=0、含项目的组=1、含子组的组=2+（projectTree.ts:30-38）。
「新建子组」菜单项的显隐条件是 `groupDepth < MAX_DEPTH - 1`（ProjectList.tsx:979）。

**渲染展平** `getOrderedTree(config)`（projectTree.ts:221-282）产出 `OrderedItem{type,depth,parentGroupId}`：
- 折叠组不递归其 children
- worktree 子项目**紧随父项目之后、depth+1** 注入（`pushProject` 递归，`pushed` 集合兼做环路保护）
- 收尾兜底：既不在树里、父项目也不存在的项目追加到顶层（**折叠组里的项目要靠 `inTree` 集合排除，不能只看 `pushed`**，否则折叠一下项目就跑到底部去了 —— projectTree.ts:266-279 有逐字注释）

### B.4 拖拽进出组

已在 A.1 全表。补两条只与分组有关的：
- 分组行 `allowInside=true`（`ProjectList.tsx:950`），项目行 `allowInside=false`（`:679`）→ **只能往组里放，不能往项目里放**
- 拖组进组：`canDrop` 先判 `isDescendant(tree, draggedId, targetGroupId)` 防自环（projectTree.ts:70）
- 「移出分组」= `moveItem(id, null)`（插到根层末尾）

### B.5 分组右键菜单全表（`ProjectList.tsx:965-1007`）

| # | key | 条件 | 动作 |
|---|---|---|---|
| 1 | `projectList.menu.renameGroup` | 恒显 | `startRenameGroup` → **内联编辑**（见 C.1） |
| 2 | `projectList.menu.addProject` | 恒显 | `handleAddProject(group.id)`：选目录 → `addProject` → `moveItem(id, groupId)` → **目标组若折叠则自动展开**（ProjectList.tsx:358-372） |
| 3 | `projectList.menu.addRemoteProject` | 恒显 | 打开 AddRemoteProjectModal（SSH，**GPUI 侧无此功能 → 本批不放这项**） |
| 4 | `projectList.menu.moveOutOfGroup` | `depth > 0` | `moveItem(group.id, null)` |
| 5 | `projectList.menu.newSubgroup` | `groupDepth < MAX_DEPTH-1`（=<2） | `showPrompt(projectList.newSubgroup, projectList.newSubgroupPlaceholder)` → `createGroup(name, group.id)` |
| 6 | `projectList.menu.deleteGroup`（**danger**） | 恒显 | `showConfirm(projectList.deleteGroupConfirm.title, .message{name,count})` → `removeGroup` |

**无分隔线**（六项连排）。删组确认文案：「确定要删除分组「{name}」吗？组内 {count} 个项目会移到上一级，不会被删除。」

**另有两处入口**：
- 列表标题栏（"PROJECTS"）空白右键 → 单项 `projectList.newGroup`（ProjectList.tsx:1069-1074）
- 底部 `+` 按钮 → 同一个 `handleCreateGroup`（见 C.3）

**项目行菜单里的分组段**（`ProjectList.tsx:795-822`，本批要补进 `project_list.rs`）：
- 前置 `{separator}`
- `isChild` → `projectList.menu.detachFromParent`「脱离父项目（转为顶层项目）」→ `moveItem(id, null)`
- 否则 `parentGroupId` 有值 → `projectList.menu.moveOutOfGroup`
- `moveToEntries.length>0` → `projectList.menu.moveToGroup` + 树形子菜单
- 整段的出现条件：`moveToEntries.length>0 || isChild || parentGroupId`

**「移动到分组」树形子菜单** `buildMoveToGroupMenu`（ProjectList.tsx:76-110，注释很关键）：
- **按层级逐级展开，不拍平**
- 含子组的组「既是落点又是入口」：因为带 submenu 的父项本身不可点，所以把
  `projectList.menu.moveToThisGroup`「移动到此分组」放进它子菜单的**第一项**，
  然后 `{separator}`，然后才是子组
- 当前所在组：label 前缀 `✓ `并 `disabled`；其余前缀**全角空格 `　`**（与 N 批 `check_prefix` 同一方案，project_list.rs:96-98 已有）
- 深度闸：`selectable = !isCurrent && depth+1 <= MAX_DEPTH`
- 子项目传 `currentParentId=undefined`（不在树里，无"当前组"可言 —— 选任意组都是有效动作）

### B.6 i18n 结论：**全部已在**

`projectList.menu.{renameGroup,addProject,addRemoteProject,moveOutOfGroup,newSubgroup,deleteGroup,moveToGroup,moveToThisGroup,detachFromParent}`、
`projectList.{newGroup,newGroupPlaceholder,newSubgroup,newSubgroupPlaceholder}`、
`projectList.deleteGroupConfirm.{title,message}` —— dict.rs:630-680(zh) / 683-733(en) 逐条在位，
中英双份齐。**本节零新增词条。**

### B.7 GPUI 现状差异

- `project_list.rs:264-281` 直接 `store.projects().iter().map(...)` **平铺渲染**，
  完全忽略 `config.project_tree`。分组行、缩进、折叠一样都没有。
- `store.rs` 只有 `remove_from_tree`（store.rs:1555-1565，删项目时清树）与
  `add_project` 时 `tree.push(ProjectTreeItem::ProjectId(id))`（store.rs:270-271）。
  **五个 group action 一个都没有**，`get_ordered_tree` 等价物也没有。
- `project_switcher.rs:269-294` 已经有一份**只读**的树遍历（收集 `groupPath` 用于模糊匹配），
  可以当 `getOrderedTree` 的起点参考，但它不产 depth/parentGroupId。
- **建议**：树操作（`get_ordered_tree` / `can_drop` / `can_drop_at` / `get_depth` /
  `get_subtree_max_depth` / `count_projects_in_group` / `find_parent_group_id` /
  `remove_from_tree` / `insert_into_tree` / `remove_group_and_promote_children`）
  抽成 `crates/mt-app/src/project_tree.rs` **纯函数模块**，逐条对着
  `src/utils/projectTree.ts` 抄并单测（那边一共 ~420 行、无外部依赖，是全批最好测的部分）。

---

## C. 项目列表补全（audit #12 剩余）

### C.1 内联重命名 F2 —— ⚠️ 与 N 批的实现**不一致**，需要改

**原版是行内编辑，不是弹窗。** 且**右键菜单「重命名」与 F2 走的是同一个函数**：

- 项目：`startRenameProject(id, name)` → `setEditingProjectId(id) + setEditingName(name)` +
  `setTimeout(() => editProjectInputRef.current?.select(), 0)`（**全选**，ProjectList.tsx:415-419）
- 提交 `commitProjectRename`：`editingName.trim()` 非空才 `renameProject` + `saveConfig`，
  然后**无论如何** `setEditingProjectId(null)`（ProjectList.tsx:422-428）
- 输入框样式（ProjectList.tsx:866-879）：
  ```
  truncate flex-1 bg-transparent border-b border-[var(--accent)] outline-none
  text-base text-[var(--text-primary)] px-0 py-0 select-text
  ```
  —— **只有一条 accent 下划线，没有边框没有背景**，autoFocus
- 按键：`Enter` 提交、`Escape` 放弃（不提交）、`onBlur` 提交、`onClick` `stopPropagation`（防止点输入框切项目）
- 行的 `onKeyDown` 在 `editingProjectId === project.id` 时**整体 return**，把按键让给输入框（ProjectList.tsx:687）
- 分组同构：`startRenameGroup` / `commitRename`，输入框 `text-sm`（ProjectList.tsx:1016-1029）

**GPUI 现状**：`project_list.rs:159-177` 的 `Rename` 菜单项调 `crate::prompt::show_prompt`
弹对话框，标题 `projectList.menu.rename`、正文借用了 `fileTree.prompt.renameMessage`。
→ **本批要改成内联编辑**，`show_prompt` 那条路撤掉。
（顺带解决 N 批记档的痛点：`gpui-component` 的 `InputState::select_all` 是 `pub(super)`，
prompt 里做不到"默认值全选"—— 内联编辑自己管 InputState 就没这个限制。）

### C.2 键盘导航

原版**没有方向键在列表内上下移动**，只有每行自己的按键（行 `tabIndex=0`，靠 Tab 走）：

| 键 | 项目行（ProjectList.tsx:686-698） | 分组行（:954-964） |
|---|---|---|
| `Enter` / `Space` | `setActiveProject(id)` | `toggleGroupCollapse` + `saveConfig` |
| `F2` | `startRenameProject` | `startRenameGroup` |
| `Delete` | `setConfirmTarget({id,name})`（走与 ✕、右键「移除项目」同一条确认路径） | — |
| 编辑态 | 整个 handler `return`，交给 input | 同 |

a11y：列表容器 `role="listbox" aria-label={t('panels.projects')}`（:1079），
项目行 `role="option" aria-selected={isActive}`，分组行 `role="treeitem" aria-expanded`。

**GPUI 现状**：行既没 `track_focus` 也没 key handler，`F2` 全局绑的是 `RenamePane`（main.rs:951，改 tab 名）。
→ 项目列表要自己的 focus + key_context，且**必须与全局 `f2` 绑定不打架**（用 `key_context` 隔离，
参考 N 批 `prefers_local_handling` 那套同源判定）。

### C.3 底部三按钮（`ProjectList.tsx:1087-1113`）

容器 `p-2 flex gap-1.5`（padding 8px、间距 6px）。三个按钮**共用**
`border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm
text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)]
transition-all duration-200`：

| # | 尺寸 | 文本 | 动作 |
|---|---|---|---|
| 1 | `flex-1 px-3 py-2` | `projectList.addProject`「+ 添加项目」 | `handleAddProject()` 无参 → 选目录 → `addProject` → `saveConfig` |
| 2 | `px-2 py-2 font-mono` | 字面量 `SSH`（title/aria = `projectList.addRemoteProject`） | 打开 AddRemoteProjectModal。**GPUI 无 SSH → 本批不做这个按钮**，记档 |
| 3 | `px-3 py-2` | 字面量 `+`（title/aria = `projectList.newGroup`） | `handleCreateGroup` → `showPrompt(newGroup, newGroupPlaceholder)` → `createGroup` |

**GPUI 现状**：`project_list.rs:431-447` 只有**头部**一个 18×18 的 `+`，调 `modal::open_add_project`。
→ 底部按钮条要新建；头部那个 `+` 原版没有（原版头部只有 "PROJECTS" 文本 + 空白右键菜单）。

### C.4 AI 厂商图标堆叠（`ProjectList.tsx:636-650, 853-865`）

- 取数：`collectAiPanes(layout, aiAutoResume ?? true)` 递归收 `paneShowsAiSession(p, enabled)` 为真的 pane（:121-130）
- vendor：`inferVendor({ agent: p.aiSession?.agent ?? p.detectedAgent })`
- **去重**：按 vendor 字符串（null → key `'unknown'`），同款 AI 开多个 pane 只显示一枚
- **排序**：`a.localeCompare(b)` 字母序，**null（未知厂商）固定排最后**
- **数量：无上限**（不是"最多 N 个"—— 去重后厂商总共就 11 家，实际上限自然收敛）
- 尺寸常量 `AI_ICON_SIZE = 14`（:144）
- 容器：`flex items-center flex-shrink-0 text-[var(--text-secondary)]`，
  `style={{ marginLeft: -6, gap: 2 }}` —— **负 6px 抵掉行内 `gap-2`(8px)，与领位图标只留 2px；图标间也是 2px**
- **固定 text-secondary 色上下文**：单色品牌图标不随选中行的 accent 变色
- **领位图标恒显、AI 堆叠只追加不覆盖**（顺序：身份图标 → AI 堆叠 → 名字）

**GPUI 现状**：`project_list.rs:335` 只画 `project_icon(kind)`，零 AI 图标。
`mt_ui::icons::BrandIcon` + `AiVendor::from_session_type/infer` 已就绪（M 批），直接消费。

### C.5 worktree 徽章与子项目缩进

**worktree 徽章**（`ProjectList.tsx:186-216, 890-897`）：
- 数据：`invoke('get_worktree_branches', {paths})` 批量，入参是**所有非远程项目的 path**，
  返回 `(string|null)[]` 同序；`probeTick` 在**窗口重获焦点**时 +1 强制重探（分支切换发生在窗外）
- 样式：`flex-shrink-0 max-w-[100px] truncate text-xs leading-[14px] px-1 rounded font-mono
  text-[var(--text-muted)] bg-[var(--border-subtle)]`
- 内容：`⎇ {branch}`（U+2387，**文本不是图标**）；title = `projectList.worktreeBadgeTitle{branch}`
- 后端 `crates/mt-project/src/git.rs:1420 get_worktree_branches(&[PathBuf]) -> Vec<Option<String>>` **已在**（UNC 路径直接跳过）。**阻塞调用，必须丢 background executor**

**失效 worktree 自动清理**（`ProjectList.tsx:218-254`）：挂载时 + 窗口重获焦点时
`collectWorktreeProbePaths(projects)` → `filter_directories` → `findStaleWorktreeProjects`
→ 逐个 `removeProjectWithCleanup`。**父项目目录仍在才清**（排除盘符整树消失的误判）。
`src/utils/worktreeReconcile.ts` 是纯函数，可逐字移植。

**子项目缩进**（`ProjectList.tsx:660-666`，注释里有踩坑记录）：
```
paddingLeft = parentGroupId ? (depth-1)*16 + 16 : 10 + depth*16
paddingRight = 10px
```
两条公式**不能合并**：组内项目要对齐父级分组的倒三角区域；顶层项目及其 worktree 子项目
以 10px 为基准每层 +16（共用组内公式会把顶层 worktree 子项目的相对缩进压到 6px）。

**GPUI 现状**：均无。`parent_project_id` 字段在 config 里已有，`ProjectConfig` 结构完全一致。

### C.6 hover 250ms 缩略图（`ProjectList.tsx:446-496` + `ProjectPanePreview.tsx`）

**触发时序**（与 tab 缩略图同一套，见 E.3）：
- `onMouseEnter` → `clearTimeout` → `isProjectDragging()` 为真直接 return → 留住 DOM 引用 →
  `setTimeout(..., 250)`
- **250ms 到点时才判 AI**（不是进入时判）：`useAppStore.getState()` 取最新值，
  `hasAiPane(layout, aiAutoResume ?? true)` 为假就不弹 —— 这 250ms 里 AI 完全可能刚起来
- rect 也是**到点时才 `getBoundingClientRect()`**（悬停期间列表若有增删位置仍准）
- 关闭路径五条：`onMouseLeave` / `onMouseDown` / window `scroll`(capture) / window `wheel` /
  AI 退出后的 effect（`:480-485`，注释解释了为什么不能只在渲染时 return null）
- **双闸**：渲染时也再判一次 `hasAiPane`（effect 在 paint 之后跑，只靠它会先闪一帧过期的卡）
- **开闸条件**：只有跑着 AI 会话的项目才弹；没 AI 的项目退回行 `title`（原生 tooltip 显示绝对路径）——
  两者互斥，`title={aiVendors.length>0 ? undefined : project.path}`（:673）

**卡片规格**（`ProjectPanePreview.tsx`）：
- `CARD_WIDTH=520`、拼图区 `BOARD_HEIGHT=340`（:34-36）
- 定位：`left = min(anchorRect.right+8, innerWidth-520-8)`；
  `top = max(8, min(anchorRect.top, innerHeight-h-8))`（底部溢出上移，`useLayoutEffect` 量高）
- 容器：`overlay-menu fixed z-50 pointer-events-none rounded-md border`，
  `background: var(--bg-overlay); borderColor: var(--border-strong);
  boxShadow: var(--shadow-overlay); backdropFilter: blur(12px)`
  （用 `.overlay-menu` 类而非内联 animation，是为了吃到 `menuPopIn` 且落在
  `prefers-reduced-motion` 豁免名单里 —— styles.css:331-332,430）
- 卡头：项目名 `text-xs font-medium` + 绝对路径 `text-[11px] text-muted truncate`
- 拼图：按 SplitNode 树递归，split 用 `flexGrow: sizes[i]`+`flexBasis:0` 复现比例，`gap-[2px]`
- 每格：active pane 画 mini canvas（`extractPreviewGrid` 读 xterm buffer → `drawPreviewGrid`），
  `object-cover object-left-bottom`（**裁右裁顶，保住左下角最新输出/TUI 输入区**）
- 每格标签条：`absolute top-0 inset-x-0`，StatusDot + BrandIcon + 名字 + 隐藏 tab `+N` 徽章
  （附隐藏 tab 里 `STATUS_PRIORITY` 最高那个的状态点）
- 未起 PTY → `projectList.preview.notStarted`；exitedPtyIds 命中 → 半透明黑遮罩 + `projectList.preview.disconnected`；
  layout 为 null → `projectList.preview.neverOpened`
- **打开期间 500ms setInterval 重画**（预览是活的）

**GPUI 可行性评估：可做，且比原版更直接。**
- 画面来源：`mt_terminal::TerminalEmulator::with_term(|term| ...)`（lib.rs:151）直接读
  alacritty `Term` 的 grid —— 不需要原版那套 xterm buffer 提取。`AppStore.terminals: Map<pty_id, Entity<TerminalPane>>` 已经持有全部实体（含后台项目的），**与原版"隐藏 tab 的 buffer 一直在被更新"是同一个前提**。
- 渲染：**别复用 `TerminalElement`**（它按真实 cell_width 布局，缩不小）。建议新写一个
  `MiniTerminalElement`：只画背景 quad + 每行一条 `ShapedLine`，字号 ~4-5px、无光标无选区。
  或退一步只画彩色 quad 马赛克（每 cell 一个背景色块）—— 520×340 的卡在这个尺寸下
  文字本来就读不清，马赛克 + 标签条已经能回答"AI 跑到哪了"。**建议先做马赛克版，字形版留后手。**
- 浮层承载：走 N 批的 `menu.rs` 同款 `deferred + anchored` 方案（menu.rs:206 `show` 已验证过
  贴边收拢 + 全窗遮罩），但预览是 `pointer-events-none`，**不要挂遮罩**。
- 250ms 定时：`cx.spawn` + `Timer::after`，代号对账防晚到误触发（K 批停留复制已有同款做法，
  见 `mt-ui/src/terminal/selection_dwell.rs`）。
- 500ms 重画：`cx.notify()` 即可，但**必须在浮层关闭时停掉定时器**（G 收尾修过一次
  "用量面板 Task 句柄无界增长"，同一个坑）。

**i18n**：`projectList.preview.{notStarted,disconnected,neverOpened}` **已在** dict.rs:669-671(zh) / EN 对应位。

### C.7 其他与状态灯位置（audit #9 遗留）

**行尾元素顺序**（`ProjectList.tsx:890-922`）：worktree 徽章 → SSH 徽章 → **DoneTag 或 StatusDot** → ✕ 按钮。
- `showDoneTag = needsAttention && !isActive` → `<DoneTag/>`；否则 `projectStatus !== 'idle'` → `<StatusDot/>`；
  **idle 时两个都不画**
- `.done-tag`（styles.css:509-524）：`inline-flex; padding:2px 8px; background:var(--color-success);
  color:var(--bg-base); font-size:0.77rem; font-weight:700; letter-spacing:0.06em;
  border-radius:10px; font-family:system-ui,sans-serif;
  box-shadow: 0 0 0 1px rgba(107,184,122,.4), 0 0 8px rgba(107,184,122,.3);
  animation: tagFadeIn .3s ease-out`。light 主题 `color:#fff` 且阴影换色。文案 key `panels.done`
- ✕ 按钮：`hidden group-hover:inline`（**只在行 hover 时出现**）、`tabIndex=-1`、
  `text-[var(--text-muted)] hover:text-[var(--color-error)] text-sm`

**GPUI 现状差异**：`project_list.rs:337-340` 把 StatusDot 放在**领位图标之后**且 **idle 也画**；
`:365-373` 用一个裸 6px 绿点代替 DoneTag；✕ 常显不随 hover。→ 三处都要改。

**项目行主体差异**：原版行是 `py-1.5 gap-2 rounded-[var(--radius-sm)] text-base`，
选中态 `bg-[var(--accent-subtle)] text-[var(--accent)]`（**没有左侧竖条**，
竖条是行首那个 `w-0.5 h-4 rounded-full bg-accent` 的 `<span>`，:836-838）；
GPUI 现在用的是 `border_l_2` 边框（project_list.rs:309-317）。
原版**副行显示 path**这件事**不存在** —— 路径只在 title / 预览卡头里出现，
GPUI `project_list.rs:356-362` 多画了一行 path，要删。

**dirKinds 探测（audit #9 遗留）**：`src/hooks/useProjectKinds.ts` 全套 ——
`classifyProject(files, deps)` 优先级（projectKind.ts:43-56）：
pom/gradle→java、Cargo.toml→rust、go.mod→go、pyproject/requirements→python、
pubspec→flutter、composer→php、package.json→(vue>next>react>svelte>vite>nodejs 按 deps)。
`PROJECT_MARKER_FILES` 10 个文件名，出现 fs-change 就失效重探。缓存在 store 的
`dirKinds: Map<path, ProjectKind|null>` + `dirKindsVersion`。**远程项目不探测。**
GPUI 侧 `ProjectKind::from_str` 与 12 种 `ALL_PROJECT_KINDS` 已在（mt-ui icons），缺的是探测调度。

---

## D. 文件树补全（audit #14 剩余）

头部结构（`FileTree.tsx:722-791`）：
`px-3 pt-3 pb-1.5 flex items-center justify-between gap-2 flex-shrink-0`，
左侧 `text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium truncate`
文案 `panels.filesOf{project}`；右侧 `flex items-center flex-shrink-0 gap-1`。

### D.1 头部三按钮

按钮共用样式：`w-[26px] h-[26px] flex items-center justify-center text-[var(--text-muted)]
hover:text-[var(--text-primary)] transition-colors rounded-[var(--radius-sm)]
hover:bg-[var(--border-subtle)]`。

| # | 按钮 | 显隐 | 行为 | 图标（13×13, viewBox 16, stroke 1.4, round） |
|---|---|---|---|---|
| 1 | 搜索 | `!isRemote` | `setSearchModalOpen(true)` —— **就是 #24 的 SearchModal，不是文件名过滤**。title/aria = `fileTree.header.searchTitle{mod}`「搜索文件 ({mod}+Shift+F)」 | `circle(7,7,r=4.2)` + `path M10.2 10.2L14 14`（FileTree.tsx:736-739） |
| 2 | 刷新 | 恒显 | `loadRootEntries(isRemote)` + `loadGitStatus()`。title 本地 = `fileTree.header.refresh`，远程 = `fileTree.remote.refreshTitle` | `path M13.5 8a5.5 5.5 0 1 1-1.7-3.97` + `M13.6 2.6v3.2h-3.2`（:753-756） |
| 3 | 外部编辑器 | `!isRemote && config.editors.length>0` | 见下 | — |

**编辑器选择器**（FileTree.tsx:759-789）是**一个分裂按钮**，外层
`flex items-center ml-0.5 pl-1 border-l border-[var(--border-subtle)]`：
- 主体：`h-[26px] text-xs px-1.5 rounded-l-[var(--radius-sm)]`，
  文本 = `config.editors.find(e=>e.name===config.defaultEditor)?.name ?? editors[0].name`，
  点击 `handleOpenInEditor()`（不传名字，后端按 defaultEditor 挑）
- 下拉箭头：仅 `editors.length>1` 时出现，`h-[26px] text-xs pl-1 pr-1.5
  rounded-r-[var(--radius-sm)] border-l`，8×8 SVG `M1.5 3L4 5.5L6.5 3`。
  点击 → `showContextMenu(rect.left, rect.bottom+4, editors.map(...))`，
  **当前默认项的 label 尾部加 ` (*)`**；选中项走 `handleSwitchAndOpen`：
  **先把 `defaultEditor` 改掉并落盘，再打开**（:462-467）
- 无编辑器配置时点主体 → `message(fileTree.dialog.noEditorMessage, {title: noEditorTitle, kind:'warning'})`

**GPUI 现状**：`file_tree.rs:545-554` 头部只有一行 `panels.files` 文本，零按钮。
`search_modal.rs`（并行 P 批）已提供 `pub fn open(store, window, cx)` 与 toggle（:90,100），
按钮直接调它即可。`mt_project::editor` 已在（file_tree.rs:190-198 的 `open_file` 已在用）。

### D.2 loading / 错误态（`FileTree.tsx:793-816`）

三态互斥，且**都以 `rootEntries.length === 0` 为前置**（有缓存内容时不显示整块占位）：

| 态 | 条件 | 渲染 |
|---|---|---|
| loading | `loading && rootEntries.length===0` | `flex items-center justify-center py-8 text-[var(--text-muted)] text-sm` + `fileTree.empty.loading`「加载中...」 |
| 加载失败 | `loadError && rootEntries.length===0` | `flex flex-col items-center justify-center gap-2 py-8 px-3 text-center text-sm`：一行 `fileTree.empty.loadFailed`（`title={loadError}` 挂原始错误）+ 一个「重试」按钮 `px-2 py-1 rounded-[var(--radius-sm)] text-[var(--accent)] hover:bg-[var(--border-subtle)]` → `loadRootEntries()` |
| 刷新失败（有旧内容） | `loadError` 且列表非空 | 列表**上方**一条 `px-2 py-1 text-xs text-[var(--text-muted)] truncate` + `fileTree.empty.refreshFailed`「文件列表刷新失败，已保留缓存」 |
| 无项目 | `!project` | 整栏居中 `fileTree.empty.selectProject`「选择一个项目」 |

另有**目录级 loading**：`loadingChildren` 只在**远程**展开时置真（本地列目录近乎即时，
置 loading 反而闪一帧 —— FileTree.tsx:95-97）。行内 spinner 替换折叠箭头：
`inline-block w-2.5 h-2.5 border border-[var(--text-muted)] border-t-transparent rounded-full animate-spin`（:332-334）。

**GPUI 现状**：`file_tree.rs:40,124-127` 有 `loading: HashSet<PathBuf>` 但只用来去重并发请求，
**没有任何 loading/错误 UI**；`list_directory` 失败静默。→ 三态全要补。
注意 GPUI 侧本地列目录是**丢 background executor 的异步**（file_tree.rs:140），
所以本地也可能有可见延迟 —— **建议本地也接 loading，但按原版口径只在"没有缓存内容时"显示**。

### D.3 键盘导航全键位（`FileTree.tsx:197-209`）

行 `tabIndex=0`、`role="treeitem"`、`aria-expanded`（仅目录）、`aria-label={entry.name}`；
容器 `role="tree" aria-label={t('panels.files')}`。

| 键 | 目录 | 文件 |
|---|---|---|
| `Enter` / `Space` | `handleToggle()`（展开/折叠） | `handleToggle()` → `onViewFile(path)` 打开预览 |
| `ArrowRight` | 仅 `!expanded` 时展开 | 无 |
| `ArrowLeft` | 仅 `expanded` 时折叠 | 无 |
| `ArrowUp/Down` | **原版没有**（靠 Tab 走） | — |

`gpui-component` 的 `tree.rs:20-23` 有现成的 `SelectUp/Down/Left/Right` action 命名与
`up/down/left/right` 绑定可以照抄（但它的数据模型不适用，别直接用组件）。

### D.4 git 状态着色

**数据源**：`invoke('get_git_status', {projectPath})` → `GitFileStatus[]`，
前端存 `Map<relativePath, GitFileStatus>`（key 是 **`/` 分隔的相对路径**，`FileTree.tsx:496-507`）。
GPUI 侧 `crates/mt-project/src/git.rs:407 get_git_status(&Path) -> Result<Vec<GitFileStatus>>` **已在**，
`status_label` 单字母 M/A/D/R/?/C（git.rs:119,1443-1449 有测试钉住）。**阻塞调用，丢后台。**

**刷新时机四条**（这是最容易漏的）：
1. 切项目时与 `list_directory` 并发拉一次（`FileTree.tsx:606-624`，`.catch(()=>[])` 吞错）
2. `fs-change` 且 `payload.projectPath === project.path` → `debouncedRefresh()`，
   **500ms 去抖**（`:510-513, 661-665`）
3. **`pty-output` 里出现 git 关键字** → 同一个 debounce（`:667-674`）。
   `GIT_PATTERNS = [/create mode/, /Switched to/, /Already up to date/, /insertions?\(\+\)/, /deletions?\(-\)/]`，
   且 `isAiPty(ptyId)` 为真时**跳过**（AI 输出里全是这些词，会疯狂刷新）
4. 头部刷新按钮手动
- 远程项目**全程跳过** git 状态

**颜色映射**（`FileTree.tsx:362-369`）：
```
M → --color-warning    A → --color-success    D → --color-error
R → --color-info       ? → --color-success    C → --color-error
兜底 → --text-muted
```
**文件行**：`ml-1.5 text-xs font-bold flex-shrink-0` + 状态字母。
**目录行**：若自身无状态，扫 `gitStatusMap` 里所有 `path.startsWith(rel + '/')` 的条目，
按 `PRIORITY = {C:6, D:5, M:4, A:3, R:2, '?':1}` 取最高的那个字母，
样式同上再叠 `opacity-70`（`:377-398`）。
**注意：着色的是"状态字母徽章"，不是文件名本身的文字颜色**（文字色仍是
`ignored ? muted+opacity-50 : isDir ? --color-folder : --color-file`，`:188-190`）。

`--color-warning` dark=`#d4a84a`/light=`#b08620`/blueprint=`#f97316`/fluent2=`#c2410c`（styles.css:35,108,988,1037）。

右键菜单还有一项 `fileTree.menu.viewDiff`「查看变更」：**仅非目录且有 git 状态时**追加
（前置一条 separator），点开 DiffModal（`:314-323`）。**GPUI 侧 git UI 未建（audit #27），本批仍不放这项**
（`file_tree.rs:266-267` 已有同样的记档注释）。

### D.5 根级单链目录压缩（`compactDirChains`，FileTree.tsx:50-86）

IDE 的 "compact middle packages"：目录**一路只有唯一子目录、没有文件**时折成一行 `main/java/com/…`。

| 规则 | 值 |
|---|---|
| 跳过 | `!isDir` 或 `ignored` 的条目不参与 |
| 继续条件 | `kids.length === 1 && kids[0].isDir && !kids[0].ignored` |
| 链深上限 | `chain.length >= 8` 即停（每深一层多一次**串行** `list_directory` IPC，后端还要跑 gitignore 匹配；8 层覆盖 Java 式深包名） |
| 产出 | `name = "a/b/c"`（`/` 拼，**不用平台分隔符**），`path` 指向**链尾真实目录**，`chainPaths = [每段路径]` |
| 未压缩 | `name === e.name` 时原样返回（**不带 chainPaths 字段**） |
| 适用范围 | **仅本地**；远程 SFTP 逐级往返太贵，调用方跳过（`:539`） |
| 调用点 | 根列表（`:539, 610`）与每个目录展开（`:119`）—— **不只是根级** |

**watcher 联动**（`FileTree.tsx:130-149`，注释里全是坑）：
- `watchKey = (chainPaths ?? [path]).join('\n')`，`watchActive = expanded || chainPaths !== undefined`
- **链上每一段都要 watch**（后端 watcher 是 NonRecursive，折叠链的中段新增文件否则无人上报）
- **折叠时也保持注册**（只要是压缩链）
- 根级 `fs-change` 处理里还有一段 `midChainHit` 判定（`:649-658`）：
  压缩链的**非链尾段**出现直接子项 → 压缩前提破坏 → 重列根目录重新压缩

**GPUI 现状**：零压缩逻辑（`file_tree.rs:229-262` 的 `rows` 直接铺）。
`mt_project::watch::FsWatcher` 已接（file_tree.rs:6-8）。
→ 压缩算法可整体移植，但**串行 8 次 `list_directory` 必须整个丢 background executor**
（GPUI 侧 `list_directory` 是阻塞函数，file_tree.rs:4-5 已注明）。

### D.6 i18n 结论：**全部已在**

`fileTree.header.{searchTitle,refresh,openWithEditor,editorFallback}`、
`fileTree.empty.{loading,loadFailed,refreshFailed,retry,selectProject}`、
`fileTree.remote.{broken,refreshTitle}`、`fileTree.menu.{chooseOtherEditor,viewDiff}`、
`fileTree.dialog.{noEditorTitle,noEditorMessage,openEditorFailedTitle}` —— dict.rs 的
`FILE_TREE_ZH/EN` 段逐条在位。**唯一缺的仍是 N 批记档的 `fileTree.dialog.createFailed*`**
（原版那两处 `invoke` 压根没接 catch，属于"原版也没有"，不是回退）。

---

## E. tab 栏补全（audit #15 剩余）

tab 栏容器（`PaneGroup.tsx:395-400`）：
```
flex items-stretch bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)]
text-xs overflow-x-auto select-none shrink-0
role="tablist" aria-label={paneGroup.tablistLabel}
```

### E.1 新建按钮的 shell 选择菜单（`PaneGroup.tsx:218-232, 476-487`）

**是左键单击直接弹菜单，不是长按、不是下拉箭头、不是右键。**

```
if (remote || config.availableShells.length <= 1) {
    newTerminal(projectId, undefined, { targetPaneId: activePane?.id });   // 不弹菜单，直接开
    return;
}
showContextMenu(e.clientX, e.clientY, config.availableShells.map(shell => ({
    label: shell.name,
    onClick: () => newTerminal(projectId, shell, { targetPaneId: activePane?.id }),
})));
```
- **shell 列表来源**：`config.availableShells`（`ShellConfig{name, command, args?}`），
  在终端配置弹窗里增删（GPUI 侧 `modal.rs` 已有终端配置对话框）
- **无勾选标记、无分隔线**，就是一列 shell 名
- 按钮本体：`px-2 text-[var(--text-muted)] hover:text-[var(--accent)] transition-colors`，
  11×11 SVG `M8 3.5v9M3.5 8h9`（stroke 1.6 round）；
  title = `` `${t('terminalArea.newTerminal')} (${hotkeyLabel('newTerminal')})` ``（= Ctrl+Shift+T）

**GPUI 现状**：`terminal_area.rs:519-536` 的 `+` 直接 `store.new_terminal(&pid, None, Some(anchor), ...)`。
`new_terminal` 的签名 `(project_id, shell: Option<ShellConfig>, anchor_pane_id, window, cx)`
（store.rs:372-379）**已经收 shell 参数**，只差把菜单接上（`menu::show(event.position, entries, ...)`）。
→ 改动量：约 20 行。**注意 `<=1` 时不弹菜单这条闸不能漏**，否则单 shell 用户每次多一次点击。

### E.2 横向滚动

- 溢出行为：容器 `overflow-x-auto`，tab 本体 `min-w-[7.5rem]`（= 97.5px @13px 基准）
  + `whitespace-nowrap` + `px-3 py-[3px]`，**tab 不压缩**，超出即出横向滚动条
- `min-w` 的理由写在注释里（`PaneGroup.tsx:413-417`）：状态点+标题+关闭按钮要作为一组在 tab 内居中，
  没有最小宽度的话短标题（nushell）和长标题的 tab 会宽窄不一
- **滚轮映射：没有自定义**。纯靠浏览器/WebView 的原生「垂直滚轮在
  `overflow-x` 容器上转成横向滚动」行为
- **右侧控件区 `ml-auto`**（`:480`）—— 在同一个滚动容器内，tab 多到溢出时它会被推走

**GPUI 现状**：`terminal_area.rs:382-390` 是裸 `div().flex().items_center().flex_none().h(px(26.))`，
**没有 overflow 设置**，tab 多了会被 flex 挤扁或溢出裁掉。
→ 需要 `.id(...)` + `.overflow_x_scroll()`（gpui 的 `StatefulInteractiveElement`）。
⚠️ **gpui 的 `overflow_x_scroll` 默认吃垂直滚轮吗？需要实测**；不吃的话要自己在
`on_scroll_wheel` 里把 `delta.y` 映射到横向 offset（原版是 WebView 免费给的）。
⚠️ 右侧控件区若要**始终可见**（原版是会被推走的），那是行为改动，**别顺手改**。

### E.3 hover 缩略图（`PaneGroup.tsx:235-277` + `PaneTabPreview.tsx`）

时序与 C.6 完全同构（同一套「留 DOM 引用 → 250ms → 到点取 rect」），差别：
- **不做 AI 开闸**：无论跑不跑 AI，隐藏 tab 都同样不可见（`PaneTabPreview.tsx:11-16` 注释）
- **只对非激活 tab 触发**：`onMouseEnter={(e)=>{ if(!isActive) handleTabPreviewEnter(e, pane.id) }}`
- 双闸的第二道：effect 里 `if (!pane || pane.id === activePane?.id) closeTabPreview()`
  —— 用 × 关掉被悬停的 tab 时点击被 `stopPropagation` 拦下，`closeTabPreview` 不触发，
  旧锚点会留着（`:262-266` 注释）
- 关闭路径：`onMouseLeave` / `onClick`（切 tab 前先关）/ `onContextMenu`（弹菜单前先关）/
  window `scroll`(capture) / window `wheel`

**卡片规格**（`PaneTabPreview.tsx:21-22, 57-79`）：
- `CARD_WIDTH=380`、`CARD_HEIGHT=232`（固定高，不像项目卡那样按布局算）
- `left = max(8, min(anchorRect.left, innerWidth-380-8))`
- `top`：默认 `anchorRect.bottom + 6`；`below + 232 > innerHeight - 8` 时翻到
  `max(8, anchorRect.top - 232 - 6)`（底部分屏的 tab 栏下方放不下）
- 容器样式与 `ProjectPanePreview` 同配方（`overlay-menu fixed z-50 pointer-events-none
  rounded-md border overflow-hidden` + bg-overlay/border-strong/shadow-overlay/blur(12px)）
- 单格 canvas，`object-cover object-left-bottom`；500ms 重画
- 未起 PTY → `projectList.preview.notStarted`；exited → 黑遮罩 + `projectList.preview.disconnected`
  （**复用 projectList 命名空间的 key**，不是 paneGroup 的）

**GPUI 可行性**：与 C.6 同一套方案，且更简单（单格，无 SplitNode 递归）。
建议**两处共用一个 `preview.rs` 模块**，参数化成「一格 or 按布局树拼」。

### E.4 分支会话两项（fork）—— 概述，随 fork 批

出现在 **tab 右键菜单**（`PaneGroup.tsx:340-380`）与**终端本体右键菜单**
（`TerminalInstance.tsx:362-390`）**两处同权**（"用户在哪右键都找得到"）。

- **显隐**：`session = pane.aiSession`（hook 上报的会话身份，**权威**）且
  `branchCapsForAgent(session.agent)?.forkCommand?.(sessionId)` 有值。
  当前 claude / codex 有 fork 位；**grok 只有 resume 位、无 fork**；opencode/pi 无会话记录 → 全隐藏
- **降级提示**：`identityMissing = !session && !!pane.detectedAgent &&
  !!branchCapsForAgent(detectedAgent)?.forkCommand` → 追加一条 **disabled** 的
  `paneGroup.forkNeedsIdentity`（"输入检测认出 AI 在跑但没有 hook 身份"，不再静默消失让人以为功能坏了）
- 两项：`paneGroup.forkSession` → `forkPaneSession(projectId, paneId)`；
  `paneGroup.viewSessionBranches` → **`submenuRender` 悬停展开** `BranchFamilyPanel`
  （连线/标题/厂商图标由 React 渲染，用 `flushSync` 同步出首帧因为 contextMenu 依赖真实尺寸定位）
- 依赖：`scan_session_lineage` + `config.sessionLineage` 自记账边（mt-config 里
  `SavedLineageEdge` 已在，config.rs:198-205）

**结论：本批不做**，标注「随 fork 批」。但 `menu.rs` 需要一个
**`submenuRender`（自定义子菜单内容）的能力位** —— 现在 `MenuItem::submenu` 只收
`Vec<MenuEntry>`（menu.rs:122），做分支树时要扩。**这一点建议在本批就把接口留出来。**

### E.5 i18n 结论

| key | 结论 |
|---|---|
| `terminalArea.newTerminal` | **已在** dict.rs:1450(zh)/1457(en) |
| `projectList.preview.{notStarted,disconnected,neverOpened}` | **已在** |
| `paneGroup.{forkSession,viewSessionBranches,forkNeedsIdentity}` | 需在 dict.rs 的 paneGroup 段（dict.rs:1776 附近）核对；随 fork 批 |
| `paneGroup.tablistLabel` | 需核对；GPUI 侧 tab 栏当前无 a11y label |

---

## 附：实现顺序建议

1. **`project_tree.rs` 纯函数模块**（B.3 全套 + `get_ordered_tree`）+ 单测 —— 无 UI 依赖，最先做、最好测
2. **`dnd.rs` 拖放基建**（A.5）—— 三个载荷类型 + 一个 `DropIndicator` 结构
3. **项目列表重做**（B.2 分组行 + C.1 内联重命名 + C.3 底部按钮 + C.4 AI 图标 + C.7 行尾顺序）
   —— 这几条改的是同一个 `render`，一次性做完比分三次改省事
4. **拖放接线**（A.1 项目排序 / A.2 加项目 / A.3 终端）
5. **文件树头部与状态**（D.1 D.2）→ **git 着色**（D.4）→ **压缩链**（D.5）
6. **tab 栏**（E.1 shell 菜单 → E.2 横向滚动）
7. **缩略图共用模块**（C.6 + E.3）—— 最独立，可以最后做也可以并行
8. worktree 徽章 / dirKinds 探测（C.5 / C.7）—— 两条都要 background executor + 缓存 + 失效，
   建议合成一件事做（同一套"批量探测 + 缓存 + fs-change/焦点失效"骨架）
