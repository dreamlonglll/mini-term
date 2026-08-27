# 远程文件管理与项目路径选择优化 — Implementation Plan

## Preconditions

- [ ] 最新 PRD 与 design 已获用户确认。
- [ ] 运行 `task.py start` 后再修改产品代码。
- [ ] 执行 `trellis-before-dev`，重新加载 mt-app / mt-project / mt-ssh / mt-config / mt-i18n 约定。
- [ ] 记录当前工作树，严格避开用户已有的 Trellis/bootstrap 变更。

## Phase A — Identity, Path Safety, and Local Primitives

1. [ ] 为文件树建立显式 `Local / Remote / BrokenRemote` 上下文和 source generation。
2. [ ] 给 `FileEntry` 补符号链接标记；更新本地/远程目录映射和相关测试构造器。
3. [ ] 修复 `mt-project::fs` 的叶子路径校验：canonicalize 父目录而非跟随叶子 symlink。
4. [ ] 增加 symlink-safe 本地删除测试：链接指向项目内文件/目录、项目外文件/目录，均只删除链接。
5. [ ] 增加本地递归复制、Keep Both 命名、覆盖 backup-swap 和目录合并原语。
6. [ ] 为大目录删除补缓存/watcher 子树清理接口，删除前 unwatch，失败时可重列恢复。

Rollback point：本阶段只改安全与本地原语；若后续远程功能受阻，仍可独立保留并验证。

## Phase B — Reusable SSH/SFTP Mutation and Transfer Primitives

1. [ ] 扩展 `mt-ssh::SftpHandle`：lstat/type、mkdir、排他创建、remove、rename。
2. [ ] 增加同一 handle 内的流式 upload/download/remote-copy，避免每文件重新开 channel。
3. [ ] 将固定 `.mt-sftp-partial` 改为唯一临时名；实现 promote、backup、rollback 和失败清理。
4. [ ] 结构化 SFTP 错误，至少能识别冲突/不存在/权限/transport。
5. [ ] 抽取通用、有限输出、可超时的 SSH exec 原语，复用 sidecar 已有协议模式。
6. [ ] 为新增原语增加纯测试/fake 测试：调用顺序、唯一临时名、覆盖回滚、并发 Keep Both。
7. [ ] 确认现有 `upload_paste` 行为和测试不回归。

Rollback point：保留旧公开 API；新 FileTree 菜单尚未接入时可单独回滚新 pipeline。

## Phase C — Remote Project-Bound Operations

1. [ ] 在 `remote_ssh.rs` 增加 canonical root/parent/basename 校验，禁止项目根、`..`、NUL 与分隔符逃逸。
2. [ ] 实现远程 create file/folder、rename、delete、copy、upload、download。
3. [ ] 远程递归操作不跟随 symlink；不支持的特殊项跳过并返回汇总。
4. [ ] 远程目录删除：SFTP 预检 → capability-gated server delete → SFTP post-verify；失败回退单 handle 后序删除。
5. [ ] 增加 `browse_directory`：不应用 `.gitignore`/ALWAYS_IGNORE，返回 canonical path 与可浏览条目。
6. [ ] fake backend 断言：一次目录一次 list、不逐候选 stat、删除后序、不同连接同路径隔离。

## Phase D — File Operation Orchestrator

1. [ ] 新增 `file_ops.rs`，定义操作上下文、复制剪贴板、冲突策略、传输计划、进度和结果汇总。
2. [ ] 复制/粘贴仅同项目同后端；切项目后粘贴禁用。
3. [ ] Copy/Paste 同名默认 Keep Both；实现 `name copy` / `name copy 2` 生成器。
4. [ ] Upload/Download 两阶段执行：扫描冲突 → 一次对话框选择 Skip/Overwrite/Keep Both → 执行。
5. [ ] Overwrite 目录使用合并语义，保留目标独有内容；类型冲突使用 backup-swap。
6. [ ] 批处理结果包含成功/跳过/失败/未执行与实际落盘路径。
7. [ ] 操作中拒绝同一 FileTree 的第二个 mutation/transfer；所有 I/O 离开 UI 线程。

## Phase E — FileTree Context Menus and CRUD

1. [ ] 把菜单生成改为 `FileContextTarget::Entry/DirectoryBackground` + backend-aware 矩阵。
2. [ ] rows 后添加 `flex_1` 空白 sibling，空白右键不再依赖父容器事件冒泡。
3. [ ] 接入本地/远程新建、重命名、删除并显示所有失败。
4. [ ] 远程隐藏“默认程序打开”“在文件夹中打开”、本地搜索和外部编辑器入口。
5. [ ] 远程文件单击不再调用本地 viewer，显示明确的下载提示。
6. [ ] 接入 Copy/Paste、Download、Open in Terminal；目录/文件/空白目标正确。
7. [ ] 操作完成后刷新受影响目录并清理删除子树缓存/watcher。
8. [ ] 加菜单矩阵、空白目标、远程守卫、迟到请求回写 guard 的纯测试。

## Phase F — Upload UI and External Drag-and-Drop

1. [ ] 菜单增加“上传文件…”与“上传文件夹…”，分别使用 GPUI 支持的文件/目录选择模式。
2. [ ] FileTree 注册 `ExternalPaths`：目录行→自身、文件行→父目录、空白→项目根。
3. [ ] `on_drag_move` 按 bounds 命中并绘制唯一 drop 高亮；拖拽结束清状态。
4. [ ] 菜单选择和拖放统一调用同一个 upload pipeline。
5. [ ] 验证混合文件/文件夹拖放、取消文件选择、本地项目不接管上传 drop。

## Phase G — Download Directory Setting

1. [ ] `AppConfig` 增加可选 `download_dir`，Default 为 `None`，serde 保持向后兼容。
2. [ ] 增加系统下载目录解析：`dirs::download_dir` → `home/Downloads` → 可见错误。
3. [ ] Settings System 页增加当前有效路径、选择目录、恢复系统默认。
4. [ ] 选择取消不修改配置；选择结果有效后才 `patch_config`。
5. [ ] 下载开始时再次验证/创建目录并检查错误，完成后展示实际路径。
6. [ ] 增加 config serde/DB 往返、默认/覆盖解析和设置状态测试。

## Phase H — Open in Terminal

1. [ ] 文件目标解析父目录，目录使用自身；远程路径使用 POSIX parent helper。
2. [ ] 捕获并校验 project id 后调用 `new_terminal_with_cwd`。
3. [ ] 修复 SSH start_pty 使用 `cwd` 而不是固定 `project.path`。
4. [ ] 增加本地/远程 cwd override、项目根 None、布局持久化/重连回归测试。

## Phase I — Remote Project Directory Picker

1. [ ] 新增 `remote_directory_picker.rs` 与独立 overlay kind/module registration。
2. [ ] 实现 Home、Root、Up、单击进入、选择当前文件夹、loading/error/retry。
3. [ ] 使用当前路径作为 initial path；空值回退 `~`。
4. [ ] request id + connection id 防快速导航/连接切换迟到响应。
5. [ ] 回填现有 Input；最终 Add 仍走 `validate_dir`。
6. [ ] 连接选择变化时重置路径为 `~` 并清除旧错误。
7. [ ] 增加 POSIX parent/root、请求竞态、选择/取消/回填状态测试。

## Phase J — i18n and Generated Files

1. [ ] 在 `crates/mt-i18n/locales/fileTree.ts`、`remoteProject.ts`、`settings.ts` 增加中英文词条。
2. [ ] 运行：

   ```bash
   node crates/mt-i18n/tools/gen_from_ts.mjs
   ```

3. [ ] 更新 `crates/mt-app/src/i18n.rs::USED_KEYS`。
4. [ ] 更新 `crates/mt-i18n/tests/consistency.rs` 条目计数。
5. [ ] 再次运行生成器，确认 `dict.rs` 幂等无变化。

## Phase K — GitHub Actions Validation and Performance Review

1. [ ] 新增 `.github/workflows/ci.yml`，支持 Pull Request、普通分支 push 与 `workflow_dispatch`；不改变 tag-only `release.yml` 的发布职责。
2. [ ] CI 复用 release workflow 的 Ubuntu GPUI 系统依赖、Node 22、stable Rust 与 Rust cache。
3. [ ] CI 运行 `cargo check --workspace --all-targets` 与 `cargo test --workspace`，覆盖本任务涉及的所有 crate 与集成测试。
4. [ ] CI 运行受影响 package 的 Clippy；本任务新增告警必须阻断，既有基线告警单独记录，不用全仓历史问题掩盖新增问题。
5. [ ] 当前 HEAD 已知不是全仓 rustfmt-clean，不运行全仓 `cargo fmt --all -- --check`；CI 仅对本任务改动的 Rust 文件执行格式检查。
6. [ ] 本地严禁运行 `cargo check`、`cargo build`、`cargo clippy`、`cargo test` 或其他会编译 Rust 的命令。
7. [ ] 本地仅执行以下非 Rust 编译检查：

   ```bash
   node crates/mt-i18n/tools/gen_from_ts.mjs
   python3 ./.trellis/scripts/task.py validate .trellis/tasks/08-27-remote-file-management
   git diff --check
   ```

8. [ ] 推送实现分支触发 GitHub Actions；根据 Actions 日志修复并重复推送，直到 CI 通过。没有可核验的成功结果时不得声称编译测试通过。

性能/行为验证：

- [ ] 本地 5k–10k 项删除在后台执行，UI 状态立即出现，watcher/缓存回到基线。
- [ ] fake SFTP 记录大目录删除请求数，无逐文件 channel handshake。
- [ ] 延迟响应从连接 A 返回时不会污染同路径连接 B。
- [ ] 上传/下载冲突三策略，目录覆盖合并且保留目标独有文件。
- [ ] 故障注入覆盖 temp/backup/promote/rollback/cleanup。
- [ ] 外部拖入文件、文件夹和混合批次落到正确远程目标。
- [ ] 默认下载目录与自定义目录均正确；取消设置不改变旧值。
- [ ] 远程 picker 手输与点选两条路径均可创建项目。

性能验证中需要 Rust fake/backend 测试的部分全部由上述 GitHub Actions 执行；本地只审阅代码证据和 CI 日志，不做替代性编译测试。

## Risky Files and Review Gates

- `crates/mt-project/src/fs.rs`：路径边界与删除，需独立安全 review。
- `crates/mt-ssh/src/sftp.rs`、`pool.rs`：传输原子性、锁范围、超时，需故障注入 review。
- `crates/mt-app/src/remote_ssh.rs`：远程根边界和 exec fast path，需注入/转义 review。
- `crates/mt-app/src/file_ops.rs`：冲突状态机与部分失败语义，需完整矩阵 review。
- `crates/mt-app/src/file_tree.rs`：事件命中、异步回写和缓存清理，需 UI/竞态 review。
- `crates/mt-config/src/config.rs`：可选字段兼容与默认解析 review。
- `.github/workflows/ci.yml`：确保普通分支/PR 可触发、依赖与 release 构建一致、质量门失败会阻断；不得借发布标签做测试。

Final gate：运行 `trellis-check`，所有 CRITICAL/WARNING 必须回到代码证据确认；GitHub Actions Rust 质量门必须有可核验的成功结果，未解决风险不得进入 finish-work。
