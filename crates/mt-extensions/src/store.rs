//! 本机扩展目录:定位、查找、安装、删除、GC、离线导入。

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use zip::ZipArchive;

use crate::archive::{self, copy_hashed, sha256_reader};
use crate::error::ExtensionError;
use crate::manifest::{self, Component, MAX_MANIFEST_BYTES, MAX_ZIP_BYTES, ManifestState};
use crate::{ComponentId, Manifest};

/// 数据目录覆盖(与 `mt_config::paths::DATA_DIR_ENV` 同一个变量;本 crate 不依赖 mt-config,
/// 名字在这里再写一份)。
pub const DATA_DIR_ENV: &str = "MT_APP_DATA_DIR";
/// 默认数据目录的 app identifier(与 `mt_config::APP_IDENTIFIER` 同值)。
pub const APP_IDENTIFIER: &str = "com.mini-term.app";
/// 数据目录下的扩展根目录名。
pub const EXTENSIONS_DIR: &str = "extensions";
/// exe 同目录的清单文件名。
pub const MANIFEST_FILE: &str = "extensions.json";
/// 给 dev / 测试指定清单路径的环境变量。
pub const MANIFEST_ENV: &str = "MT_EXTENSIONS_MANIFEST";

/// 下载暂存目录(`{root}/.downloads/`)。
pub(crate) const DOWNLOADS_DIR: &str = ".downloads";
/// 解包暂存目录前缀(`{root}/<id>/.tmp-<随机>/`)。
pub(crate) const TMP_PREFIX: &str = ".tmp-";
/// 删除时先挪进的回收区前缀(`{root}/.trash-<随机>` / `{root}/<id>/.trash-<随机>`,见 [`discard`])。
pub(crate) const TRASH_PREFIX: &str = ".trash-";
/// GC 只清理比这更旧的暂存物 —— 正在下载 / 解包的那份 mtime 一直是新的。
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(10 * 60);
/// GC 时每个已知 id 在当前清单那份之外再留几份其它版本(见 [`Extensions::gc`])。
const KEEP_OTHER_VERSIONS: usize = 1;
/// 离线包本身的体积上限(四个 helper 合计约 10 MB,阶段 C 的语法包再加三十来 MB)。
const MAX_BUNDLE_BYTES: u64 = 1024 * 1024 * 1024;
/// 离线包的条目数上限。
const MAX_BUNDLE_ENTRIES: usize = 256;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 扩展根目录的口径(纯函数,便于单测):`MT_APP_DATA_DIR` 非空 → `{它}/extensions`;
/// 否则 `{local_data_dir}/com.mini-term.app/extensions`。
///
/// Windows 上 `local_data_dir` 是 `%LOCALAPPDATA%` —— **Local 不是 Roaming**:几十 MB 的
/// 二进制不该进漫游配置。NSIS 真卸载时删的就是这个目录。
pub fn root_for(
    data_dir_override: Option<&OsStr>,
    local_data_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    match data_dir_override.filter(|value| !value.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(EXTENSIONS_DIR)),
        None => local_data_dir.map(|dir| dir.join(APP_IDENTIFIER).join(EXTENSIONS_DIR)),
    }
}

/// 当前进程的扩展根目录(见 [`root_for`])。**不保证已存在**。
pub fn default_root() -> Option<PathBuf> {
    root_for(
        std::env::var_os(DATA_DIR_ENV).as_deref(),
        dirs::data_local_dir(),
    )
}

/// 读一份清单。文件不在 = [`ManifestState::Missing`](dev / 源码构建的常态)。
pub fn load_manifest(path: &Path) -> ManifestState {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ManifestState::Missing;
        }
        Err(error) => {
            return ManifestState::Invalid(format!("reading {}: {error}", path.display()));
        }
    };
    let mut text = String::new();
    let mut limited = file.take(MAX_MANIFEST_BYTES + 1);
    if let Err(error) = limited.read_to_string(&mut text) {
        return ManifestState::Invalid(format!("reading {}: {error}", path.display()));
    }
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return ManifestState::Invalid(format!(
            "{} exceeds {MAX_MANIFEST_BYTES} bytes",
            path.display()
        ));
    }
    manifest::parse_manifest(&text).into()
}

/// [`Extensions::locate`] 找到的文件来自哪一层。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocatedSource {
    /// exe 同目录的旧布局(dev 的 stage 脚本 / 源码构建):与 exe 同信任等级,不校验清单。
    Bundled,
    /// 调用方给的开发根目录(仅 helper:`MT_REMOTE_GIT_HELPERS_DIR`)。
    DevRoot,
    /// 本机已装扩展(内容寻址目录)。
    Installed,
}

/// 查找结果。`expected` 只在 [`LocatedSource::Installed`] 时有:清单里这个文件的
/// `(sha256, size)`,调用方读它的时候可以顺手核一遍。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub source: LocatedSource,
    pub expected: Option<(String, u64)>,
}

/// 界面上一个扩展的安装状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentStatus {
    /// 清单列出的文件在 exe 同目录的旧布局里全都在(dev 实例):不需要也不提供下载。
    Bundled,
    /// 已装进本机扩展目录。
    Installed(PathBuf),
    /// 没装。
    Missing,
}

/// [`Extensions::gc`] 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub removed: Vec<PathBuf>,
    /// 该删但没删掉的(被占用 / 权限),下次启动再试。
    pub kept: Vec<PathBuf>,
}

/// [`Extensions::import_offline`] 的结果。
#[derive(Debug, Default)]
pub struct ImportReport {
    /// 装好(或本来就装着)的扩展。
    pub installed: Vec<ComponentId>,
    /// 离线包里不认识的条目数(别的平台 / 别的版本的资产;随包的 `extensions.json` 副本不计)。
    pub skipped: usize,
    /// 认出来了但没装上的(校验失败 / 写盘失败)。
    pub failed: Vec<(ComponentId, ExtensionError)>,
}

/// 一次运行里的扩展视图:exe 目录(旧布局)+ 本机根目录 + 启动时读到的清单。
///
/// 清单在进程生命周期内**不变**(它随安装包换,换安装包就是换进程),所以启动时读一次
/// 就够,见 [`global`]。
#[derive(Debug)]
pub struct Extensions {
    exe_dir: Option<PathBuf>,
    root: Option<PathBuf>,
    manifest: ManifestState,
}

/// 进程级的那一份([`Extensions::detect`] 懒建一次)。mt-app 的设置页 / 下载服务与
/// mt-remote 的 helper 部署共用它。
pub fn global() -> &'static Extensions {
    static GLOBAL: OnceLock<Extensions> = OnceLock::new();
    GLOBAL.get_or_init(Extensions::detect)
}

impl Extensions {
    pub fn new(exe_dir: Option<PathBuf>, root: Option<PathBuf>, manifest: ManifestState) -> Self {
        Self {
            exe_dir,
            root,
            manifest,
        }
    }

    /// 按当前进程现算:exe 目录取 `current_exe` 的父目录;清单取 `MT_EXTENSIONS_MANIFEST`
    /// (非空时),否则 exe 同目录的 `extensions.json`;根目录见 [`default_root`]。
    pub fn detect() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let manifest = match std::env::var_os(MANIFEST_ENV).filter(|value| !value.is_empty()) {
            Some(path) => load_manifest(Path::new(&path)),
            None => match &exe_dir {
                Some(dir) => load_manifest(&dir.join(MANIFEST_FILE)),
                None => ManifestState::Missing,
            },
        };
        Self::new(exe_dir, default_root(), manifest)
    }

    pub fn manifest_state(&self) -> &ManifestState {
        &self.manifest
    }

    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.manifest()
    }

    /// 本机扩展根目录(**不保证已存在**;界面「打开扩展目录」前自己建)。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn component(&self, id: ComponentId) -> Option<&Component> {
        self.manifest()?.component(id)
    }

    fn require_manifest(&self) -> Result<&Manifest, ExtensionError> {
        match &self.manifest {
            ManifestState::Loaded(manifest) => Ok(manifest),
            ManifestState::Missing => Err(ExtensionError::NoManifest),
            ManifestState::TooNew { schema } => {
                Err(ExtensionError::ManifestTooNew { schema: *schema })
            }
            ManifestState::Invalid(detail) => Err(ExtensionError::ManifestInvalid(detail.clone())),
        }
    }

    fn require_component(&self, id: ComponentId) -> Result<&Component, ExtensionError> {
        self.require_manifest()?
            .component(id)
            .ok_or(ExtensionError::NotInManifest(id))
    }

    fn require_root(&self) -> Result<&Path, ExtensionError> {
        self.root.as_deref().ok_or(ExtensionError::NoRoot)
    }

    /// 已装扩展的正式目录:`{root}/<id>/<zip sha256 前 16 位>`。清单里没有它 = `None`。
    pub fn installed_dir(&self, id: ComponentId) -> Option<PathBuf> {
        let component = self.component(id)?;
        Some(
            self.root
                .as_ref()?
                .join(id.as_str())
                .join(component.version_dir()),
        )
    }

    /// exe 同目录的旧布局这一层认不认:**只在本机没有有效清单时认**(清单缺失 / 过新 / 读不懂
    /// = dev、源码构建)。有效清单在场 = 正式安装包(或 dev 用 `MT_EXTENSIONS_MANIFEST` 模拟的
    /// 正式包),旧布局一律不认 —— tar.gz 解压覆盖旧版目录留下的 `remote-git-helpers/`、
    /// `mt-ssh-cli` 不会压住按需下载的那份。
    fn bundled_dir(&self) -> Option<&Path> {
        match self.manifest {
            ManifestState::Loaded(_) => None,
            _ => self.exe_dir.as_deref(),
        }
    }

    /// 找一个扩展文件,顺序见 crate 注释:① exe 同目录旧布局(只在没有有效清单时,见
    /// [`Self::bundled_dir`])→ ② `dev_root`(只对 helper 生效,布局 `<dev_root>/<target>/<file>`)
    /// → ③ 本机已装扩展。
    ///
    /// ①② 只看「那个位置上有没有东西」(`symlink_metadata` 成功即算找到,哪怕是坏的):
    /// 合不合规由调用方判 —— 在场却坏掉的旧布局**不会**静默落到下一层去用别的文件
    /// (mt-remote 的口径)。③ 要求是常规文件且字节数与清单一致,否则当没装。
    /// `NotFound` 以外的 IO 错误原样上抛。
    pub fn locate(
        &self,
        id: ComponentId,
        file: &str,
        dev_root: Option<&Path>,
    ) -> std::io::Result<Option<Located>> {
        if let Some(exe_dir) = self.bundled_dir() {
            let bundled = bundled_path(exe_dir, id, file);
            if exists(&bundled)? {
                return Ok(Some(Located {
                    path: bundled,
                    source: LocatedSource::Bundled,
                    expected: None,
                }));
            }
        }
        if let (Some(target), Some(dev_root)) = (id.helper_target(), dev_root) {
            let candidate = dev_root.join(target).join(file);
            if exists(&candidate)? {
                return Ok(Some(Located {
                    path: candidate,
                    source: LocatedSource::DevRoot,
                    expected: None,
                }));
            }
        }
        let (Some(component), Some(dir)) = (self.component(id), self.installed_dir(id)) else {
            return Ok(None);
        };
        let Some(spec) = component.file(file) else {
            return Ok(None);
        };
        let path = dir.join(&spec.name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == spec.size => {
                Ok(Some(Located {
                    path,
                    source: LocatedSource::Installed,
                    expected: Some((spec.sha256.clone(), spec.size)),
                }))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// 界面上的安装状态(只 stat,不算哈希)。`Bundled` 要求清单列的全部文件都在旧布局里 ——
    /// 而旧布局只在没有有效清单时认([`Self::bundled_dir`]),没有清单又没有组件可报,所以
    /// 正式安装包上它永远不会出现;保留这个分支是为了口径与 [`Self::locate`] 一致。
    pub fn status(&self, id: ComponentId) -> ComponentStatus {
        let Some(component) = self.component(id) else {
            return ComponentStatus::Missing;
        };
        if let Some(exe_dir) = self.bundled_dir()
            && component
                .files
                .iter()
                .all(|file| exists(&bundled_path(exe_dir, id, &file.name)).unwrap_or(false))
        {
            return ComponentStatus::Bundled;
        }
        match self.installed_dir(id) {
            Some(dir) if is_healthy(component, &dir) => ComponentStatus::Installed(dir),
            _ => ComponentStatus::Missing,
        }
    }

    /// 给一次下载准备暂存路径:`{root}/.downloads/<asset>.<随机>.part`(目录不在就建)。
    pub fn new_download_path(&self, id: ComponentId) -> Result<PathBuf, ExtensionError> {
        let component = self.require_component(id)?;
        let dir = self.require_root()?.join(DOWNLOADS_DIR);
        fs::create_dir_all(&dir)
            .map_err(|e| ExtensionError::io(format!("creating {}", dir.display()), e))?;
        Ok(dir.join(format!("{}.{}.part", component.asset, unique_suffix())))
    }

    /// 校验并安装一个扩展 zip(任何来源都走这一道):zip 的字节数 + sha256 对上清单 →
    /// 解进 `{root}/<id>/.tmp-<随机>/` 并逐文件校验 → rename 成正式目录。任何一步失败
    /// 都把暂存目录删干净,不留半成品;正式目录已经是好的就直接返回(幂等)。
    pub fn install_zip(&self, id: ComponentId, zip_path: &Path) -> Result<PathBuf, ExtensionError> {
        let component = self.require_component(id)?;
        let root = self.require_root()?;
        let mut file = open_regular(zip_path)?;
        let len = file
            .metadata()
            .map_err(|e| ExtensionError::io(format!("stat {}", zip_path.display()), e))?
            .len();
        if len != component.size {
            return Err(ExtensionError::integrity(format!(
                "{}: {len} bytes, manifest says {}",
                component.asset, component.size
            )));
        }
        let digest = sha256_reader(&mut file, component.size)?;
        if digest != component.sha256 {
            return Err(ExtensionError::integrity(format!(
                "{}: sha256 mismatch",
                component.asset
            )));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|e| ExtensionError::io(format!("rewinding {}", zip_path.display()), e))?;

        let id_dir = root.join(id.as_str());
        let final_dir = id_dir.join(component.version_dir());
        if is_healthy(component, &final_dir) {
            return Ok(final_dir);
        }
        fs::create_dir_all(&id_dir)
            .map_err(|e| ExtensionError::io(format!("creating {}", id_dir.display()), e))?;
        let staging = id_dir.join(format!("{TMP_PREFIX}{}", unique_suffix()));
        fs::create_dir(&staging)
            .map_err(|e| ExtensionError::io(format!("creating {}", staging.display()), e))?;
        if let Err(error) = archive::extract_component(file, component, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        // 正式目录在但不健康(半成品 / 被改过):先挪走再换入(挪不走 = 里面有在跑的 exe,报错)
        if fs::symlink_metadata(&final_dir).is_ok()
            && let Err(error) = discard(&final_dir)
        {
            let _ = fs::remove_dir_all(&staging);
            return Err(ExtensionError::io(
                format!("replacing {}", final_dir.display()),
                error,
            ));
        }
        match fs::rename(&staging, &final_dir) {
            Ok(()) => Ok(final_dir),
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                // 另一个安装方(同进程的另一路 / 另一个实例)抢先落位了同一份内容
                if is_healthy(component, &final_dir) {
                    Ok(final_dir)
                } else {
                    Err(ExtensionError::io(
                        format!("publishing {}", final_dir.display()),
                        error,
                    ))
                }
            }
        }
    }

    /// 删除一个扩展在本机的全部版本(`{root}/<id>/`)。不在 = 成功。要么整个删掉、要么原样
    /// 不动(见 [`discard`]):Windows 上 mt-ssh-cli 的 daemon 正跑在里面时报错,不会删成半截。
    pub fn remove(&self, id: ComponentId) -> Result<(), ExtensionError> {
        let dir = self.require_root()?.join(id.as_str());
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ExtensionError::io(format!("stat {}", dir.display()), error)),
            Ok(_) => discard(&dir)
                .map_err(|e| ExtensionError::io(format!("removing {}", dir.display()), e)),
        }
    }

    /// 启动后台的清理:每个**已知** id 留当前清单那份 + 最近装的一份其它版本(当前清单里
    /// 没有这个 id 时就只留最近一份),其余版本目录删掉;未知 id 目录整个删;放了超过
    /// 10 分钟的下载 / 解包暂存物删掉。删不掉的(被占用)留到下次,不报错。
    ///
    /// 多留一份其它版本,是为了预览版 / 正式版共用数据目录来回切换时不必每次重下。
    /// 「最近装的」按版本目录里文件的最新 mtime 认(解包时新写的文件,mtime 就是安装时刻;
    /// 目录里没有文件时退回目录自身的 mtime)—— 不直接看目录 mtime:Windows 上删一个被
    /// daemon 占着的旧版本目录会删掉一部分文件,目录 mtime 随之变成「现在」,下次启动它就会
    /// 冒充最近的一份把真正完整的那份挤掉。[`Self::locate`] / [`Self::status`] 仍只认当前
    /// 清单那份。
    ///
    /// **没有清单(或清单读不懂)时什么都不删**:认不出哪份是「当前」,宁可不动 ——
    /// dev 实例没设 `MT_APP_DATA_DIR` 时与装机版共用这个目录。
    pub fn gc(&self) -> GcReport {
        self.gc_at(SystemTime::now())
    }

    pub(crate) fn gc_at(&self, now: SystemTime) -> GcReport {
        let mut report = GcReport::default();
        let (Some(manifest), Some(root)) = (self.manifest(), self.root.as_deref()) else {
            return report;
        };
        let Ok(entries) = fs::read_dir(root) else {
            return report;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if name == DOWNLOADS_DIR {
                if metadata.is_dir() {
                    sweep_stale(&path, now, &mut report);
                }
                continue;
            }
            if name.starts_with(TRASH_PREFIX) {
                // 上次挪进回收区却没删干净的(见 [`discard`])
                purge_into(&path, &mut report);
                continue;
            }
            if name.starts_with('.') || !(metadata.is_dir() || metadata.is_symlink()) {
                // 别的点目录与散落的文件不归扩展管
                continue;
            }
            let Some(id) = ComponentId::parse(&name).filter(|_| metadata.is_dir()) else {
                // 未知 id / 不是真目录:整个删
                remove_into(&path, &mut report);
                continue;
            };
            let current = manifest.component(id).map(Component::version_dir);
            let Ok(children) = fs::read_dir(&path) else {
                continue;
            };
            // 当前清单以外的版本目录:按「最近装的」排,留一份(见 [`Self::gc`])
            let mut others: Vec<(SystemTime, PathBuf)> = Vec::new();
            for child in children.flatten() {
                let child_path = child.path();
                let child_name = child.file_name();
                let child_name = child_name.to_string_lossy();
                if Some(child_name.as_ref()) == current {
                    continue;
                }
                if child_name.starts_with(TMP_PREFIX) {
                    // 正在解包的那一份不动,放久了的才删
                    if is_stale(&child_path, now) {
                        purge_into(&child_path, &mut report);
                    }
                    continue;
                }
                if child_name.starts_with(TRASH_PREFIX) {
                    purge_into(&child_path, &mut report);
                    continue;
                }
                match fs::symlink_metadata(&child_path) {
                    Ok(meta) if meta.is_dir() && !child_name.starts_with('.') => {
                        others.push((installed_at(&child_path, &meta), child_path));
                    }
                    // 散落的文件 / 符号链接 / 别的点目录:不是版本目录
                    _ => remove_into(&child_path, &mut report),
                }
            }
            // 最近的排最前;mtime 相同按路径定序(结果确定)
            others.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, stale) in others.into_iter().skip(KEEP_OTHER_VERSIONS) {
                remove_into(&stale, &mut report);
            }
        }
        report
    }

    /// 导入离线扩展包(或单个扩展 zip)。
    ///
    /// 1. 文件本身的 sha256 + 字节数等于本机清单里某个扩展资产 → 当单个扩展装;
    /// 2. 否则当离线包:逐条目按**本机清单**的资产名匹配(取条目路径最后一段,重新打过
    ///    一层目录的包也认),匹配上的解出来走 [`Self::install_zip`] 的完整校验;不认识的
    ///    条目跳过并计数。随包的 `extensions.json` 副本**不信任**、也不计数。
    ///
    /// 越界路径(`..` / 绝对路径 / 盘符 / 反斜杠)、符号链接、非常规文件条目、条目数或体积
    /// 超限 → 整包拒绝,什么都不装。
    pub fn import_offline(&self, path: &Path) -> Result<ImportReport, ExtensionError> {
        let manifest = self.require_manifest()?;
        self.require_root()?;
        let mut file = open_regular(path)?;
        let len = file
            .metadata()
            .map_err(|e| ExtensionError::io(format!("stat {}", path.display()), e))?
            .len();
        if len <= MAX_ZIP_BYTES {
            let digest = sha256_reader(&mut file, len)?;
            if let Some(component) = manifest.component_by_digest(&digest, len) {
                let id = component.id;
                self.install_zip(id, path)?;
                return Ok(ImportReport {
                    installed: vec![id],
                    ..ImportReport::default()
                });
            }
            file.seek(SeekFrom::Start(0))
                .map_err(|e| ExtensionError::io(format!("rewinding {}", path.display()), e))?;
        }
        if len > MAX_BUNDLE_BYTES {
            return Err(ExtensionError::integrity(format!(
                "offline bundle exceeds {MAX_BUNDLE_BYTES} bytes"
            )));
        }
        let mut archive = ZipArchive::new(file).map_err(|e| {
            ExtensionError::integrity(format!(
                "not an extension or offline bundle of this version: {e}"
            ))
        })?;
        if archive.len() > MAX_BUNDLE_ENTRIES {
            return Err(ExtensionError::integrity(format!(
                "offline bundle has more than {MAX_BUNDLE_ENTRIES} entries"
            )));
        }

        // 先把条目过一遍:有一个越界 / 符号链接就整包拒绝(什么都还没装)
        let mut report = ImportReport::default();
        let mut plan: Vec<(usize, &Component)> = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index).map_err(|e| {
                ExtensionError::integrity(format!("offline bundle entry #{index}: {e}"))
            })?;
            let name = entry
                .name()
                .map_err(|e| {
                    ExtensionError::integrity(format!("offline bundle entry #{index} name: {e}"))
                })?
                .into_owned();
            if !is_enclosed_entry_name(&name) {
                return Err(ExtensionError::integrity(format!(
                    "offline bundle entry {name:?} escapes the archive"
                )));
            }
            if entry.is_dir() {
                continue;
            }
            archive::require_regular(&entry, "offline bundle", &name)?;
            let base = name.rsplit('/').next().unwrap_or(&name);
            if base == MANIFEST_FILE {
                continue;
            }
            match manifest.component_by_asset(base) {
                Some(component) if !plan.iter().any(|(_, planned)| planned.id == component.id) => {
                    plan.push((index, component));
                }
                _ => report.skipped += 1,
            }
        }

        for (index, component) in plan {
            match self.install_bundle_entry(&mut archive, index, component) {
                Ok(()) => report.installed.push(component.id),
                Err(error) => report.failed.push((component.id, error)),
            }
        }
        Ok(report)
    }

    fn install_bundle_entry(
        &self,
        archive: &mut ZipArchive<File>,
        index: usize,
        component: &Component,
    ) -> Result<(), ExtensionError> {
        let temp = self.new_download_path(component.id)?;
        let result = (|| {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| ExtensionError::integrity(format!("{}: {e}", component.asset)))?;
            if entry.size() != component.size {
                return Err(ExtensionError::integrity(format!(
                    "{}: {} bytes, manifest says {}",
                    component.asset,
                    entry.size(),
                    component.size
                )));
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| ExtensionError::io(format!("creating {}", temp.display()), e))?;
            copy_hashed(&mut entry, &mut out, component.size)?;
            drop(out);
            self.install_zip(component.id, &temp).map(|_| ())
        })();
        let _ = fs::remove_file(&temp);
        result
    }
}

/// 旧布局里一个扩展文件的位置:helper 在 `remote-git-helpers/<target>/`,其余平铺 exe 旁。
fn bundled_path(exe_dir: &Path, id: ComponentId, file: &str) -> PathBuf {
    match id.helper_target() {
        Some(target) => exe_dir.join("remote-git-helpers").join(target).join(file),
        None => exe_dir.join(file),
    }
}

/// 那个位置上有没有东西(不跟随符号链接)。
fn exists(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// 正式目录健康 = 是真目录,清单列的每个文件都是常规文件且字节数对得上。
fn is_healthy(component: &Component, dir: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return false;
    };
    metadata.is_dir()
        && component.files.iter().all(|file| {
            fs::symlink_metadata(dir.join(&file.name))
                .is_ok_and(|meta| meta.is_file() && meta.len() == file.size)
        })
}

/// 打开一个常规文件(拒绝符号链接与目录)。
fn open_regular(path: &Path) -> Result<File, ExtensionError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| ExtensionError::io(format!("stat {}", path.display()), e))?;
    if !metadata.is_file() {
        return Err(ExtensionError::integrity(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    File::open(path).map_err(|e| ExtensionError::io(format!("opening {}", path.display()), e))
}

/// 删一个目录项:符号链接只删链接本身,目录整个删,文件删文件。
fn remove_entry(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        // Windows 上指向目录的符号链接要按目录删
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// 删一个版本目录 / id 目录,**要么整个删掉、要么原样不动**:先 rename 成同级的
/// `.trash-<随机>` 再删。Windows 上目录里有运行中的 exe(mt-ssh-cli 的 daemon,它的工作目录也
/// 就是这个版本目录)时 rename 失败 → 原样留着、报错;直接 `remove_dir_all` 则会删掉其它文件、
/// 只剩那个 exe,留下一个半截目录。回收区没删干净的由下次 GC 收掉。符号链接与文件直接删。
fn discard(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let Some(parent) = path.parent().filter(|_| metadata.is_dir()) else {
        return remove_entry(path);
    };
    let trash = parent.join(format!("{TRASH_PREFIX}{}", unique_suffix()));
    fs::rename(path, &trash)?;
    let _ = fs::remove_dir_all(&trash);
    Ok(())
}

/// GC 删一个版本目录 / id 目录(经 [`discard`]:删不了就原样留着,记进 `kept`)。
fn remove_into(path: &Path, report: &mut GcReport) {
    match discard(path) {
        Ok(()) => report.removed.push(path.to_path_buf()),
        Err(_) => report.kept.push(path.to_path_buf()),
    }
}

/// GC 删暂存物 / 回收区(本来就是废料,能删多少删多少)。
fn purge_into(path: &Path, report: &mut GcReport) {
    match remove_entry(path) {
        Ok(()) => report.removed.push(path.to_path_buf()),
        Err(_) => report.kept.push(path.to_path_buf()),
    }
}

/// 一个版本目录是什么时候装的:目录里常规文件的最新 mtime;一个文件都没有时退回目录自身的
/// mtime;都读不到 = 纪元(排最旧)。理由见 [`Extensions::gc`]。
fn installed_at(dir: &Path, dir_meta: &fs::Metadata) -> SystemTime {
    let newest_file = fs::read_dir(dir).ok().and_then(|entries| {
        entries
            .flatten()
            .filter_map(|entry| fs::symlink_metadata(entry.path()).ok())
            .filter(fs::Metadata::is_file)
            .filter_map(|meta| meta.modified().ok())
            .max()
    });
    newest_file
        .or_else(|| dir_meta.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn is_stale(path: &Path, now: SystemTime) -> bool {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_AFTER)
}

fn sweep_stale(dir: &Path, now: SystemTime, report: &mut GcReport) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_stale(&path, now) {
            purge_into(&path, report);
        }
    }
}

/// 离线包条目名有没有越界:非空、不含 NUL / 反斜杠 / 盘符冒号、不以 `/` 开头、
/// 没有 `..` 或空的路径段(目录条目末尾那个 `/` 除外)。
fn is_enclosed_entry_name(name: &str) -> bool {
    if name.is_empty() || name.contains(['\0', '\\', ':']) || name.starts_with('/') {
        return false;
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    !trimmed.is_empty()
        && trimmed
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{nanos:x}-{:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
pub(crate) fn is_enclosed_entry_name_for_test(name: &str) -> bool {
    is_enclosed_entry_name(name)
}
