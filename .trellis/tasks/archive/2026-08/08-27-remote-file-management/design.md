# 远程文件管理与项目路径选择优化 — Technical Design

## 1. Architecture Overview

本任务把“文件树 UI 直接调用某个本地函数”的现状改为显式分层：

```text
FileTree / RemoteProject / Settings
        ↓ 用户意图、项目身份、目标目录
mt-app::file_ops / mt-app::remote_ssh
        ↓ 本地或远程的强类型操作
mt-project::fs        mt-ssh::SftpHandle / pooled session
        ↓                         ↓
local filesystem              remote SFTP / guarded SSH exec
```

核心原则：

- 文件系统类型必须是显式枚举，不能再用 `Option<Connection>` 同时表示本地和断链远程。
- 所有后台请求都携带项目、连接和请求代号，结果只允许回写原上下文。
- UI 只描述操作和展示状态，递归、冲突、原子提交与路径边界由服务层负责。
- 远程路径始终按 POSIX `String` 处理；宿主平台 `Path` 只用于本地路径。

## 2. Context and Identity Contract

新增应用层上下文：

```rust
enum FileBackend {
    Local,
    Remote { connection: SshConnection },
    BrokenRemote,
}

struct FileOperationContext {
    project_id: String,
    root: String,
    backend: FileBackend,
    generation: u64,
}
```

- `project_id` 是 UI 与 pane 的身份锚点。
- 远程身份至少包含 `connection.id`；同一路径在不同连接上绝不等价。
- `generation` 在活动项目、项目根或连接配置变化时递增。
- 目录加载、写操作、传输和远程 picker 的异步结果回写前均校验身份/代号。
- `BrokenRemote` 的任何文件操作直接返回可见错误，不回退本地文件系统。

`FileEntry` 增加符号链接标记。目录展开只依据真实目录；符号链接可显示，但复制/上传/下载不跟随，删除只删除链接自身。

## 3. Layer Ownership

### `mt-project`

- 修复项目根边界校验：对叶子条目使用“canonicalize 父目录 + 原始 basename + `symlink_metadata`”，不跟随叶子符号链接。
- 提供本地新建、重命名、删除、递归复制和本地下载落盘原语。
- 提供 Keep Both 名称生成的纯函数或共享策略类型。
- 本地覆盖使用唯一临时项与 backup-swap；目录覆盖采用合并语义。

### `mt-ssh`

扩展 `SftpHandle`，使一个 channel 可复用于整批操作：

- `lstat`/条目类型、`mkdir`、排他创建、`remove_file`、`remove_dir`、`rename`。
- 流式上传、下载、远程到远程复制。
- 唯一临时路径、唯一备份路径、promote/rollback。
- 结构化错误至少区分：已存在、不存在、权限、传输、取消/超时、其他 SFTP 错误。
- 增加有超时和退出码的通用 SSH exec 原语；应用层不自行拼装 channel 协议。

### `mt-app::remote_ssh`

- 将连接、canonical 项目根与远程路径边界绑定在一起。
- 提供远程 CRUD、递归复制、上传、下载和删除编排。
- 提供不带 `.gitignore`/固定隐藏目录语义的 `browse_directory`。
- 远程目录删除：SFTP 完成安全预检；普通目录优先尝试 capability-gated 服务端删除，成功后用 SFTP 验证；不可用时回退单个复用 handle 的后序递归删除。
- 远程复制默认走 SFTP，避免不同服务器 `cp`/symlink/权限语义漂移。

### `mt-app::file_ops`（新增）

- 统一操作模型：Create、Rename、Delete、Copy、Paste、Upload、Download。
- 保存内部复制剪贴板，仅允许当前项目同一后端粘贴；跨边界使用上传/下载。
- 两阶段传输：后台扫描/预检 → UI 冲突选择 → 后台执行。
- 汇总成功、跳过、失败、未执行数量与受影响目录。
- 将进度事件回传 FileTree；FileTree 不包含递归算法。

## 4. Context Menu and Hit Testing

显式目标模型：

```rust
enum FileContextTarget {
    Entry(Row),
    DirectoryBackground { dir: String },
}
```

- 条目右键只从 `Entry` 生成菜单。
- rows 后增加 `flex_1` 空白 sibling；空白右键、粘贴和外部 drop 只挂在该 sibling，不依赖父容器冒泡。
- 行仍保持整行命中，符合树视图常见行为；真正空白是所有行下方区域。

菜单矩阵：

- 本地文件：打开、复制、复制路径、资源管理器定位、终端打开父目录、重命名、删除。
- 本地目录：复制、粘贴、复制路径、资源管理器定位、终端打开、新建、重命名、删除。
- 远程文件：复制、下载、复制路径、终端打开父目录、重命名、删除。
- 远程目录：复制、粘贴、上传文件、上传文件夹、下载、复制路径、终端打开、新建、重命名、删除。
- 远程空白：粘贴、上传文件、上传文件夹、终端打开项目根、新建文件/文件夹。

远程上下文隐藏本机默认程序、资源管理器、本地搜索和外部编辑器入口。远程文件单击不再把路径交给本地预览器；显示“远程预览尚未支持，可下载后打开”的明确反馈。

## 5. Copy/Paste and Conflict Semantics

复制剪贴板保存 `{project_id, backend, root, source_path, is_dir, is_symlink}`。

- 切换项目后粘贴不可用；不会凭路径跨项目猜测来源。
- 复制/粘贴同名默认 Keep Both，不弹冲突框。
- Keep Both 名称：`name.ext` → `name copy.ext` → `name copy 2.ext`；目录同理。
- 每个目标目录只列一次名称并在内存预留本批生成名；最终用排他创建/rename 处理并发竞态。

上传/下载冲突先扫描顶层目标；存在冲突时打开一次三选一对话框，选择应用于本批剩余冲突：

- Skip：跳过冲突项，继续非冲突项。
- Overwrite：
  - 文件覆盖文件：原子/可恢复替换。
  - 目录覆盖目录：递归合并，覆盖同名冲突项，保留目标独有内容。
  - 文件与目录类型不一致：backup-swap 整项替换。
- Keep Both：冲突项使用新的同级副本名；目录完整复制为新目录，不合并。

关闭冲突对话框等同取消，传输尚未开始，不产生副作用。执行期间发现竞态冲突时沿用已选策略。

## 6. Atomicity and Partial Failure

- 临时项使用唯一同级名，禁止固定 `.mt-sftp-partial` 导致并发碰撞。
- 新建/Keep Both：完整写临时项后 rename 到最终不存在目标。
- 文件 Overwrite：`target → unique backup`，`temp → target`；失败则恢复 backup，成功后删除 backup。
- 目录 Keep Both：构建同级 staging 目录，完成后 rename。
- 目录 Overwrite 合并无法整树原子化；每个文件独立原子提交，最终展示部分成功汇总。
- 任何跨边界复制失败都不删除来源。本任务不实现 Cut/Move。
- 符号链接、socket、FIFO、device 不递归跟随；传输时跳过并汇总警告。

## 7. Upload Entry Points and Drag-and-Drop

GPUI 的 `ExternalPaths` 作为唯一系统拖放载荷：

- 目录行：目标为该目录。
- 文件行：目标为文件父目录。
- 空白 sibling：目标为项目根。
- 仅远程 FileTree 注册上传 drop；本地项目不改变现有行为。
- `on_drag_move` 必须检查 bounds，记录唯一命中目标并显示高亮；drop 后清理状态。

系统选择器无法稳定地一次混选文件与目录，因此菜单拆为：

- 上传文件…：`files=true, directories=false, multiple=true`
- 上传文件夹…：`files=false, directories=true`

拖放天然允许同一批包含文件和文件夹，两条入口最终调用同一个 upload pipeline。

## 8. Download Directory Configuration

`AppConfig` 新增：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub download_dir: Option<String>
```

- `None`：动态使用 `dirs::download_dir()`；若不可用，回退 `home/Downloads`；仍不可定位时给出错误并引导设置。
- `Some`：用户在设置中选择的目录。
- System 设置页新增“下载目录”区域，展示有效路径，提供“选择…”与“恢复系统默认”。
- 选择只在目录有效时提交；取消保留旧值。
- 下载开始时再次验证/创建目标目录并检查可写性，处理目录在设置后被删除的情况。
- 不修改 sidecar 协议；旧版本会忽略新增字段，旧配置缺字段自动使用系统默认。

下载菜单直接使用该目录，不再每次打开目录选择器；操作完成后展示实际落盘路径。

## 9. Operation State and Refresh

FileTree 保存一个当前 mutation/transfer 状态：扫描中、等待冲突选择、执行中、完成/失败。

- 同一 FileTree 同时只执行一个破坏性或传输操作，避免冲突与临时文件竞争。
- UI 渲染线程只排任务和处理状态；本地阻塞 IO、SFTP 和 exec 均在后台。
- 远程无 watcher，成功后主动刷新目标目录。
- 本地删除前解除目标子树 watcher；结束后只重列父目录，避免大量逐文件 watcher 事件。
- 删除/重命名后清理 `entries`、`watched`、`chain_owner`、`row_focus` 中不可达子树。
- 任何回写先验证 `project_id + connection_id + generation`。

## 10. Delete Performance Strategy

当前本地 `remove_dir_all` 已在后台，不直接卡 UI；问题是无状态反馈、占用后台 worker、watcher 洪泛和 symlink 跟随。

改造后：

- 本地：symlink-safe 删除，预先 unwatch 子树，显示操作中状态，完成后一次刷新。
- 远程文件/符号链接：SFTP 单项 remove。
- 远程普通目录：SFTP 安全预检后优先服务端删除；exec 失败或不支持时，复用一个 SFTP handle 的后序递归删除。
- 服务端删除必须使用单引号安全转义、绝对路径、根目录拒绝和删除后 SFTP 验证。
- fake backend 记录请求数；禁止“每个文件重新 acquire/open SFTP channel”。

性能验收关注两个维度：UI 发起后立即返回并持续可重绘；远程 fallback 请求数符合 `readdir + remove` 的线性预算且不额外产生逐文件握手。

## 11. Open in Terminal

- 文件夹目标为自身；文件目标为 POSIX/本地规则下的父目录。
- 捕获当前 `project_id`，点击时确认项目仍是当前项目。
- 调用 `AppStore::new_terminal_with_cwd`。
- 修复 SSH 分支把 `project.path` 写死的问题，使用已经解析的 cwd override。
- 项目根传 `None`，子目录传 `Some(path)`；现有 pane 持久化与重连继续复用。

## 12. Remote Project Directory Picker

新增独立 `remote_directory_picker` overlay：

- 当前路径、Home、根目录、上一级、目录列表、loading/error/retry。
- 单击目录进入；“选择当前文件夹”确认并回填原路径输入框。
- 列表不应用项目 `.gitignore` 或固定隐藏目录，点目录可见。
- symlink 条目可显示并尝试进入；若目标不是目录或无权限，原位展示错误。
- 使用 `connection_id + request_id` 丢弃快速导航/连接切换后的迟到响应。
- 在添加远程项目弹窗切换连接时，把路径重置为 `~` 并清除旧错误。
- 最终创建仍再次执行现有 `validate_dir`，不改变 ProjectConfig 格式。

## 13. Compatibility and Migration

- 唯一持久化变更是可选 `downloadDir`，缺字段自动兼容。
- 远程项目、SSH 连接和布局格式不变。
- i18n 只修改 locale 源并重新生成字典；同步 `USED_KEYS` 与一致性计数。
- 不改变远程 preview/search/git 的数据协议；这些能力仍明确不在本任务内。

## 14. Rollback Shape

- UI 菜单、拖放、picker、设置入口可独立回滚，不影响已有项目数据。
- `downloadDir` 是可选字段，回滚版本会忽略。
- 低层 SFTP 新原语应保持新增 API，不改变现有剪贴板上传调用；若新传输 pipeline 出现问题，可暂时隐藏新菜单而保留低层修复。
- symlink-safe 本地删除与异步身份 guard 属数据安全修复，不应随功能回滚撤销。

## 15. GitHub Actions Validation Boundary

现有 `.github/workflows/release.yml` 仅由 `v*` 标签触发，职责是三平台发布打包；本任务不修改其触发语义，也不通过创建发布标签来验证功能。

新增独立 `.github/workflows/ci.yml`：

- 触发：Pull Request、普通分支 push、`workflow_dispatch`。
- Linux runner 复用 release workflow 已确认的 GPUI 系统依赖、Node 22、stable Rust 与 Rust cache 配置。
- Rust 质量门至少包含 `cargo check --workspace --all-targets` 和 `cargo test --workspace`；对本任务直接影响的 `mt-project`、`mt-ssh`、`mt-config`、`mt-app` 测试失败均阻止通过。
- Clippy 在 CI 中执行；若仓库既有告警使全工作区 `-D warnings` 无法直接启用，则先以受影响 package 为阻断范围，并把基线外告警与本任务新增告警区分记录。
- 仓库已有记录表明当前 HEAD 不是全仓 rustfmt-clean，因此不以 `cargo fmt --all -- --check` 直接阻断本任务。CI 只检查本任务改动的 Rust 文件格式，避免把历史格式差异混入本任务。
- i18n 生成器幂等性和 `git diff --check` 可在本地先执行，也在 CI 复核；本地不调用任何 Cargo 编译、Clippy 或测试命令。
- CI 失败后依据 Actions 日志修改并重新推送，直到质量门通过；无法触发或读取 Actions 结果时，不得把任务表述为“编译测试通过”。
