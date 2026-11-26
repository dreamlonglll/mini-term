//! 按需扩展(on-demand extensions)的本机侧。
//!
//! 主包瘦身:远程 Git helper(阶段 A)、SSH 工具 sidecar(阶段 B)、大语法包(阶段 C)不再
//! 随安装包分发,而是由官方 CI 构建、与主程序**同版本**绑定,按需下载到本机。本 crate 管
//! 「下载回来之后」的一切 —— 清单解析、存放目录、查找、校验安装、删除、GC、离线导入。
//! **不联网**:下载那一跳由 mt-app 注入(口径同 mt-usage 的 `models_dev`),这里只认
//! 本地文件。
//!
//! # 信任模型
//!
//! - 扩展清单 `extensions.json` 与 exe 同目录、由同一个安装器写入,信任等级**等同 exe
//!   本身**。任何来源的扩展(GitHub / 用户配的镜像 / 离线包)一律先按清单里 zip 的
//!   sha256 + 字节数校验,再解包,再逐文件校验 sha256 + 字节数([`Extensions::install_zip`])。
//!   所以镜像不可信也无妨。
//! - 不做签名、不在编译期嵌哈希:CI 里桌面构建与 helper 构建并行,编译期拿不到 helper 哈希。
//! - 没有清单(dev 构建、源码构建)= 不提供在线下载 / 离线导入([`ManifestState::Missing`])。
//!   `MT_EXTENSIONS_MANIFEST=<路径>` 给 dev / 测试指定一份清单。
//! - exe 同目录的**旧布局**(`remote-git-helpers/<target>/mt-git-helper`、`mt-ssh-cli.exe`、
//!   语法包库)视同可信、优先使用、不校验清单 —— 那是 dev(stage 脚本就位)与源码构建的路径,
//!   与 exe 同信任等级。**只在本机没有有效清单时认**(清单缺失 / 过新 / 读不懂):有效清单在场
//!   就是正式安装包,tar.gz 解压覆盖旧版目录留下的旧 helper / `mt-ssh-cli` 不会压住按需下载的
//!   那份。
//!
//! # 存放与查找
//!
//! - 根目录([`default_root`]):`MT_APP_DATA_DIR` 非空 → `{它}/extensions`;否则
//!   `dirs::data_local_dir()/com.mini-term.app/extensions`。
//! - 布局:`{root}/<id>/<zip sha256 前 16 位>/<文件>`;下载暂存 `{root}/.downloads/`;
//!   解包暂存 `{root}/<id>/.tmp-<随机>/`,全部校验通过后 rename 成正式目录(**目录在 =
//!   装好了**)。内容寻址目录是为了 Windows 上不覆盖正在运行的 exe(阶段 B 的 mt-ssh-cli
//!   daemon)。
//! - [`Extensions::locate`] 的顺序:① exe 同目录旧布局(仅无有效清单时)→ ②(仅 helper)调用方
//!   给的开发根目录(mt-remote 的 `MT_REMOTE_GIT_HELPERS_DIR`)→ ③ 本机已装扩展。都没有 = 未安装。
//! - [`Extensions::gc`]:每个已知 id 留当前清单那份 + 最近装的一份其它版本,其余版本目录与未知
//!   id 目录删掉,删不掉(被占用)就留到下次。**没有清单时什么都不删** —— 认不出哪份是「当前」,
//!   宁可不动。删除一律先 rename 进同级回收区再删(要么整个删掉、要么原样不动,见 store.rs 的
//!   `discard`)。
//!
//! # 资产与清单
//!
//! 每个扩展 = 一个 zip,资产名内容寻址:`mt-ext_<id>_<zip sha256 前 12 位>.zip`。清单格式
//! 见 [`manifest`] 模块注释;打包脚本是 `scripts/pack-extensions.mjs`(两边的往返由
//! `tests/fixtures/` 那份夹具对账:Node 测试保证脚本仍产出同样的字节,本 crate 的测试保证
//! 装得上)。

mod archive;
mod component;
mod error;
pub mod manifest;
mod store;

pub use component::{ComponentId, ComponentKind, GIT_HELPER_FILE, SSH_CLI_FILE, SSH_MCP_FILE};
pub use error::ExtensionError;
pub use manifest::{
    Component, ComponentFile, MAX_FILE_BYTES, MAX_ZIP_BYTES, Manifest, ManifestError,
    ManifestState, SCHEMA_VERSION, asset_name, is_safe_file_name, parse_manifest,
};
pub use store::{
    APP_IDENTIFIER, ComponentStatus, DATA_DIR_ENV, EXTENSIONS_DIR, Extensions, GcReport,
    ImportReport, Located, LocatedSource, MANIFEST_ENV, MANIFEST_FILE, default_root, global,
    load_manifest, root_for,
};

#[cfg(test)]
mod tests;
