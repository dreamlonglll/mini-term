//! 按需扩展的下载服务(远程 Git helper、SSH 工具 `ssh-tools`、语法高亮扩展包 `syntax-pack`)。
//!
//! # 分工
//!
//! - **本机那一半**在 [`mt_extensions`]:清单解析、查找、校验安装、删除、GC、离线导入。
//!   进程级的那一份是 [`mt_extensions::global`],mt-remote 的 helper 部署、mt-ai 的 SSH CLI
//!   定位与宿主语法包加载器 [`crate::syntax_pack`] 共用。
//! - **这里**:下载那一跳(共享 HTTP 客户端 [`crate::http`] 流式写盘)、每个扩展的状态表
//!   (下载中 x% / 校验中 / 失败原因)、同一扩展并发 [`ensure`] 去重、通知 UI(设置页实时
//!   刷新、自动触发的下载推原地更新的 toast、安装后触发 SKILL.md 校正与语法包热加载),
//!   以及注入 mt-remote 的 helper 补齐器。
//!
//! # 什么时候下载
//!
//! **原则上只在用户的显式动作里**:设置页点「下载」/「全部下载」、发起托管 Git 部署
//! (项目已授权远端 Git 但缺对应服务器平台的 helper —— 见 `mt_remote::set_helper_provisioner`
//! 与 `git_service::deploy_with_provisioning`)、「关联 SSH」保存时缺 `mt-ssh-cli`
//! ([`Origin::EnableSshTools`])、或文件预览打开语法包语言时点顶部提示条的「下载」
//! ([`Origin::SyntaxBanner`])。部署触发的失败在 [`AUTO_RETRY_COOLDOWN`] 内不再自动重下
//! (Git 面板轮询每次都会撞到缺 helper,不能每次都拉网刷 toast);其它显式动作均无冷却。
//! **唯一的非显式下载**是 [`Origin::SelfHeal`] 升级自愈:启动时若有本地项目启用着 SSH 工具
//! 而本机缺 `mt-ssh-cli`,后台静默下载,失败只记日志并在设置页标原因、不推 toast。
//!
//! # 下载
//!
//! 地址 = 模板(配置 `extensionsDownloadUrl`,空 = [`DEFAULT_DOWNLOAD_TEMPLATE`])里的
//! `{tag}` / `{asset}` 换成清单的 tag 与资产名。写到 `{root}/.downloads/` 的暂存文件,累计
//! 字节数一超过清单 size 立刻断;连接(到响应头)[`CONNECT_TIMEOUT`],[`STALL_TIMEOUT`] 没收到
//! 数据判失败。下完交给 [`mt_extensions::Extensions::install_zip`] 做完整校验 —— 任何来源都过
//! 同一道,下载源不可信也无妨。下载跑在每个任务自己的线程上(网络 IO 本身在共享 tokio 运行时,
//! 校验解包是阻塞 IO,不占 GPUI 线程池也不占运行时的工作线程)。

use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, Entity, Global};
use mt_extensions::{
    Component, ComponentId, ComponentKind, ComponentStatus, ExtensionError, Extensions,
    ImportReport, ManifestState,
};

use crate::i18n::{t, tr};
use crate::settings::SettingsPage;
use crate::store::AppStore;

/// 默认下载地址模板:本版本的 GitHub release 资产。不内置任何第三方代理。
pub const DEFAULT_DOWNLOAD_TEMPLATE: &str =
    "https://github.com/dreamlonglll/mini-term/releases/download/{tag}/{asset}";
/// 连接到收到响应头的时限(含重定向)。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 这么久没收到一个字节判卡死。
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 部署触发的下载失败后,这段时间内同一扩展不再自动重下(见模块注释)。
const AUTO_RETRY_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// 下载进度报不报:界面(设置页一行、toast)只显示整数百分比,所以**百分比变了才报**,
/// 一次下载至多 101 条。不按时间节流 —— 按时间节流只报窗口里的第一块,数据随后停住
/// (卡在半路等 [`STALL_TIMEOUT`])时最后那一截永远报不出去,界面停在一个偏小的数上。
#[derive(Default)]
struct ProgressGate {
    reported: Option<u8>,
}

impl ProgressGate {
    /// 收到 `received / total` 字节时要不要报;要报就返回该报的百分比。
    fn check(&mut self, received: u64, total: u64) -> Option<u8> {
        let percent = JobState::Downloading { received, total }.percent();
        if self.reported == Some(percent) {
            return None;
        }
        self.reported = Some(percent);
        Some(percent)
    }
}

/// 后来的调用方加入一个已在跑的任务:它要推 toast、而发起那一方不推(设置页 / 升级自愈先
/// 点的)时,任务改按它的来源推进度与结局 —— 否则从提示条 / 部署那一侧看,下载失败了也
/// 没有任何原因。两边都推或都不推就照旧。返回是否改了。
fn adopt_origin(current: &mut Origin, incoming: Origin) -> bool {
    if incoming.toasts() && !current.toasts() {
        *current = incoming;
        true
    } else {
        false
    }
}

/// 谁要的这次下载。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// 设置页的按钮:总是立刻重试,进度只在设置页里显示。
    User,
    /// 托管 Git 部署缺 helper:进度推 toast,失败后有冷却。
    Deploy,
    /// 「关联 SSH」启用 SSH 工具时 CLI 不在:用户的显式动作,总是立刻重试,进度推 toast。
    EnableSshTools,
    /// 升级自愈(阶段 B):启动时有项目启用了 SSH 工具而 CLI 不在。唯一的非显式下载 ——
    /// 进度只在设置页里显示,失败安静记日志 + 设置页标原因,不推 toast(每次启动都会撞)。
    SelfHeal,
    /// 文件预览的语法包提示条点了「下载」(阶段 C):用户的显式动作,总是立刻重试,进度推 toast。
    SyntaxBanner,
}

impl Origin {
    /// 进度与结局推不推 toast。
    fn toasts(self) -> bool {
        matches!(
            self,
            Self::Deploy | Self::EnableSshTools | Self::SyntaxBanner
        )
    }

    /// 上次失败后冷却期内是否直接报上次的原因、不重下。
    fn cools_down(self) -> bool {
        matches!(self, Self::Deploy)
    }
}

/// 下载 / 安装失败的原因(界面文案见 [`describe`])。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    /// 本机这一半报的错:没清单 / 不在清单里 / 校验失败 / 读写失败。
    Extension(ExtensionError),
    /// 下载地址模板无效。
    Template,
    /// 连不上下载源(或到响应头超时)。
    Connect(String),
    /// 下载源返回了非 2xx。
    Http(u16),
    /// [`STALL_TIMEOUT`] 没收到数据。
    Stalled,
    /// 下载内容超过清单声明的大小。
    TooLarge,
    /// 其它网络错误(含没下完就断了)。
    Network(String),
}

/// 一个正在进行的任务处在哪一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Downloading { received: u64, total: u64 },
    Verifying,
}

impl JobState {
    pub fn percent(self) -> u8 {
        match self {
            Self::Downloading { received, total } if total > 0 => {
                ((received.min(total) * 100) / total) as u8
            }
            Self::Downloading { .. } => 0,
            Self::Verifying => 100,
        }
    }
}

/// 设置页一行要的东西。
#[derive(Clone, Debug)]
pub struct ComponentRow {
    pub component: Component,
    pub status: ComponentStatus,
    pub job: Option<JobState>,
    pub failure: Option<DownloadError>,
}

/// 后台线程交回 UI 线程的事件。
enum ServiceEvent {
    /// 状态表变了:设置页重画。
    Changed,
    /// 部署 / 启用 SSH 工具 / 语法包提示条触发的下载开始 / 进度 / 结束:推 toast。
    AutoProgress(ComponentId, u8),
    AutoFinished(ComponentId, Result<(), DownloadError>),
    /// 一个扩展刚装好(下载或离线导入,不论谁发起):`ssh-tools` 装好后要把现存 SKILL.md
    /// 改指过去(启动校正当时 CLI 还不在,跳过了)。
    Installed(ComponentId),
}

/// 一次 [`ensure`] 的结果接收端:`.await` 它(GPUI 任务里不占线程),或在后台线程上
/// `futures::executor::block_on`。发送端被丢弃(任务线程 panic)时得到 `Canceled`。
pub type EnsureReceiver = oneshot::Receiver<Result<(), DownloadError>>;

struct Job {
    state: JobState,
    origin: Origin,
    waiters: Vec<oneshot::Sender<Result<(), DownloadError>>>,
}

struct Failure {
    error: DownloadError,
    at: Instant,
}

#[derive(Default)]
struct Service {
    jobs: Mutex<HashMap<ComponentId, Job>>,
    failures: Mutex<HashMap<ComponentId, Failure>>,
    /// 安装状态的缓存(只 stat,但不在渲染里做 IO):启动、任务结束、删除、导入后刷新。
    statuses: Mutex<HashMap<ComponentId, ComponentStatus>>,
    /// 用户配置的下载地址模板(`None` = 默认)。由 `AppStore` 在配置变化时同步。
    template: Mutex<Option<String>>,
    events: Mutex<Option<mpsc::UnboundedSender<ServiceEvent>>>,
}

fn service() -> &'static Service {
    static SERVICE: OnceLock<Service> = OnceLock::new();
    SERVICE.get_or_init(Service::default)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn emit(event: ServiceEvent) {
    if let Some(sender) = lock(&service().events).as_ref() {
        let _ = sender.unbounded_send(event);
    }
}

/// 进程级的扩展视图(清单 / 根目录 / 旧布局),与 mt-remote 共用。
pub fn extensions() -> &'static Extensions {
    mt_extensions::global()
}

// ─── 下载地址 ────────────────────────────────────────────────

/// 模板合不合规:`http(s)://` 开头、含 `{asset}`(`{tag}` 可省 —— 镜像可能按资产名平铺)。
pub fn is_valid_template(template: &str) -> bool {
    let template = template.trim();
    (template.starts_with("https://") || template.starts_with("http://"))
        && template.contains("{asset}")
        && !template.chars().any(char::is_whitespace)
}

/// 由模板(`None` / 空 = 默认)拼出某个资产的下载地址。
pub fn resolve_url(
    template: Option<&str>,
    tag: &str,
    asset: &str,
) -> Result<String, DownloadError> {
    let template = template
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_DOWNLOAD_TEMPLATE);
    if !is_valid_template(template) {
        return Err(DownloadError::Template);
    }
    Ok(template.replace("{tag}", tag).replace("{asset}", asset))
}

/// 配置里的模板变了(启动装配 + 设置页改动都走这里)。换了下载源 = 之前的失败作废。
pub fn set_download_template(template: Option<String>) {
    let next = template
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut current = lock(&service().template);
    if *current != next {
        *current = next;
        drop(current);
        lock(&service().failures).clear();
        emit(ServiceEvent::Changed);
    }
}

// ─── 状态 ────────────────────────────────────────────────────

/// 重算安装状态缓存(stat 几个文件,调用方放后台)。
pub fn refresh_statuses() {
    let extensions = extensions();
    let statuses: HashMap<ComponentId, ComponentStatus> = extensions
        .manifest()
        .map(|manifest| {
            manifest
                .components
                .iter()
                .map(|component| (component.id, extensions.status(component.id)))
                .collect()
        })
        .unwrap_or_default();
    *lock(&service().statuses) = statuses;
    emit(ServiceEvent::Changed);
}

/// 设置页的一份快照:清单里每个扩展一行。
pub fn rows() -> Vec<ComponentRow> {
    let Some(manifest) = extensions().manifest() else {
        return Vec::new();
    };
    let service = service();
    let statuses = lock(&service.statuses);
    let jobs = lock(&service.jobs);
    let failures = lock(&service.failures);
    manifest
        .components
        .iter()
        .map(|component| ComponentRow {
            component: component.clone(),
            status: statuses
                .get(&component.id)
                .cloned()
                .unwrap_or(ComponentStatus::Missing),
            job: jobs.get(&component.id).map(|job| job.state),
            failure: failures
                .get(&component.id)
                .map(|failure| failure.error.clone()),
        })
        .collect()
}

pub fn manifest_state() -> &'static ManifestState {
    extensions().manifest_state()
}

// ─── ensure ──────────────────────────────────────────────────

/// 确保 `id` 装好:已装 → 立刻 `Ok`;同一扩展已有任务在跑 → 排进它的等待者;否则起一个
/// 下载任务。返回的接收端在任务结束时收到结果(不在乎结果的调用方直接丢掉即可)。
pub fn ensure(id: ComponentId, origin: Origin) -> EnsureReceiver {
    let (sender, receiver) = oneshot::channel();
    match extensions().status(id) {
        ComponentStatus::Installed(_) | ComponentStatus::Bundled => {
            let _ = sender.send(Ok(()));
            return receiver;
        }
        ComponentStatus::Missing => {}
    }
    // 没清单(dev / 源码构建)或本平台没有这个扩展:当场报,不起任务、不推 toast
    let total = match available(id) {
        Ok(component) => component.size,
        Err(error) => {
            let _ = sender.send(Err(error));
            return receiver;
        }
    };
    let service = service();
    if origin.cools_down()
        && let Some(failure) = lock(&service.failures).get(&id)
        && failure.at.elapsed() < AUTO_RETRY_COOLDOWN
    {
        let _ = sender.send(Err(failure.error.clone()));
        return receiver;
    }
    {
        let mut jobs = lock(&service.jobs);
        if let Some(job) = jobs.get_mut(&id) {
            job.waiters.push(sender);
            // 改按后来者的来源推 toast 时,当场补一条眼下的进度(结局由 finish_job 照新来源推)
            let adopted = adopt_origin(&mut job.origin, origin);
            let percent = job.state.percent();
            drop(jobs);
            if adopted {
                emit(ServiceEvent::AutoProgress(id, percent));
            }
            return receiver;
        }
        jobs.insert(
            id,
            Job {
                state: JobState::Downloading { received: 0, total },
                origin,
                waiters: vec![sender],
            },
        );
    }
    lock(&service.failures).remove(&id);
    emit(ServiceEvent::Changed);
    if origin.toasts() {
        emit(ServiceEvent::AutoProgress(id, 0));
    }
    let spawned = std::thread::Builder::new()
        .name(format!("mt-ext-{id}"))
        .spawn(move || finish_job(id, download_and_install(id)));
    if let Err(error) = spawned {
        finish_job(
            id,
            Err(DownloadError::Network(format!(
                "starting download thread: {error}"
            ))),
        );
    }
    receiver
}

/// [`ensure`] 并阻塞等结果。**只能在后台线程上调**(mt-remote 的 Git 执行器线程)。
pub fn ensure_blocking(id: ComponentId, origin: Origin) -> Result<(), DownloadError> {
    futures::executor::block_on(ensure(id, origin)).unwrap_or_else(|_| Err(task_vanished()))
}

/// 下载任务没给结果就没了(线程 panic)。
pub fn task_vanished() -> DownloadError {
    DownloadError::Network("download task ended without a result".into())
}

fn finish_job(id: ComponentId, result: Result<(), DownloadError>) {
    let service = service();
    let job = lock(&service.jobs).remove(&id);
    match &result {
        Ok(()) => {
            lock(&service.failures).remove(&id);
        }
        Err(error) => {
            tracing::warn!("[extensions] {id}: {error:?}");
            lock(&service.failures).insert(
                id,
                Failure {
                    error: error.clone(),
                    at: Instant::now(),
                },
            );
        }
    }
    let status = extensions().status(id);
    lock(&service.statuses).insert(id, status);
    if result.is_ok() {
        emit(ServiceEvent::Installed(id));
    }
    if let Some(job) = job {
        if job.origin.toasts() {
            emit(ServiceEvent::AutoFinished(id, result.clone()));
        }
        for waiter in job.waiters {
            let _ = waiter.send(result.clone());
        }
    }
    emit(ServiceEvent::Changed);
}

fn set_job_state(id: ComponentId, state: JobState) -> Option<Origin> {
    let mut jobs = lock(&service().jobs);
    let job = jobs.get_mut(&id)?;
    job.state = state;
    Some(job.origin)
}

/// 清单里的这个扩展;没清单 / 清单读不懂 / 本平台没有它 → 对应的错误。
fn available(id: ComponentId) -> Result<&'static Component, DownloadError> {
    let manifest = match extensions().manifest_state() {
        ManifestState::Loaded(manifest) => manifest,
        ManifestState::Missing => return Err(DownloadError::Extension(ExtensionError::NoManifest)),
        ManifestState::TooNew { schema } => {
            return Err(DownloadError::Extension(ExtensionError::ManifestTooNew {
                schema: *schema,
            }));
        }
        ManifestState::Invalid(detail) => {
            return Err(DownloadError::Extension(ExtensionError::ManifestInvalid(
                detail.clone(),
            )));
        }
    };
    manifest
        .component(id)
        .ok_or(DownloadError::Extension(ExtensionError::NotInManifest(id)))
}

fn download_and_install(id: ComponentId) -> Result<(), DownloadError> {
    let extensions = extensions();
    let component = available(id)?;
    let tag = extensions
        .manifest()
        .map(|manifest| manifest.tag.as_str())
        .unwrap_or_default();
    let template = lock(&service().template).clone();
    let url = resolve_url(template.as_deref(), tag, &component.asset)?;
    let part = extensions
        .new_download_path(id)
        .map_err(DownloadError::Extension)?;
    let total = component.size;
    let mut gate = ProgressGate::default();
    let progress = move |received: u64| {
        let Some(percent) = gate.check(received, total) else {
            return;
        };
        // 来源每次现读:任务跑到一半可能被后加入的提示条 / 部署改成推 toast(见 adopt_origin)
        if let Some(origin) = set_job_state(id, JobState::Downloading { received, total }) {
            emit(ServiceEvent::Changed);
            if origin.toasts() {
                emit(ServiceEvent::AutoProgress(id, percent));
            }
        }
    };
    let fetched = futures::executor::block_on(crate::http::run_async(fetch_to_file(
        url,
        part.clone(),
        total,
        progress,
    )))
    .unwrap_or_else(|error| Err(DownloadError::Network(error)));
    let result = fetched.and_then(|()| {
        set_job_state(id, JobState::Verifying);
        emit(ServiceEvent::Changed);
        extensions
            .install_zip(id, &part)
            .map(|_| ())
            .map_err(DownloadError::Extension)
    });
    let _ = std::fs::remove_file(&part);
    result
}

/// 流式下载到 `path`。必须跑在共享 tokio 运行时上(经 [`crate::http::run_async`])。
async fn fetch_to_file(
    url: String,
    path: PathBuf,
    expected: u64,
    mut progress: impl FnMut(u64) + Send,
) -> Result<(), DownloadError> {
    let client = crate::http::try_client()
        .map_err(|error| DownloadError::Network(error.without_url().to_string()))?;
    let mut response =
        mt_remote::timeout_on_shared_runtime(CONNECT_TIMEOUT, client.get(&url).send())
            .await
            .ok_or_else(|| DownloadError::Connect("timed out".into()))?
            // URL 里可能带着用户镜像的令牌:错误一律 without_url
            .map_err(|error| DownloadError::Connect(error.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::Http(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > expected)
    {
        return Err(DownloadError::TooLarge);
    }
    let mut file = File::create(&path).map_err(|error| {
        DownloadError::Extension(ExtensionError::Io(format!(
            "creating {}: {error}",
            path.display()
        )))
    })?;
    let mut received = 0u64;
    loop {
        let chunk = mt_remote::timeout_on_shared_runtime(STALL_TIMEOUT, response.chunk())
            .await
            .ok_or(DownloadError::Stalled)?
            .map_err(|error| DownloadError::Network(error.without_url().to_string()))?;
        let Some(chunk) = chunk else {
            break;
        };
        received += chunk.len() as u64;
        if received > expected {
            return Err(DownloadError::TooLarge);
        }
        file.write_all(&chunk).map_err(|error| {
            DownloadError::Extension(ExtensionError::Io(format!(
                "writing {}: {error}",
                path.display()
            )))
        })?;
        progress(received);
    }
    file.sync_all().map_err(|error| {
        DownloadError::Extension(ExtensionError::Io(format!(
            "flushing {}: {error}",
            path.display()
        )))
    })?;
    if received != expected {
        return Err(DownloadError::Network(format!(
            "incomplete download: {received} of {expected} bytes"
        )));
    }
    Ok(())
}

// ─── 删除 / 导入(阻塞,调用方放后台) ─────────────────────────

/// 删除一个扩展。正在下载的不许删(按钮也是灰的)。
pub fn remove(id: ComponentId) -> Result<(), DownloadError> {
    if lock(&service().jobs).contains_key(&id) {
        return Ok(());
    }
    let result = extensions().remove(id).map_err(DownloadError::Extension);
    lock(&service().failures).remove(&id);
    let status = extensions().status(id);
    lock(&service().statuses).insert(id, status);
    emit(ServiceEvent::Changed);
    result
}

/// 导入离线扩展包(或单个扩展 zip)。
pub fn import_offline(path: &Path) -> Result<ImportReport, ExtensionError> {
    let result = extensions().import_offline(path);
    if let Ok(report) = &result {
        let mut failures = lock(&service().failures);
        for id in &report.installed {
            failures.remove(id);
        }
    }
    refresh_statuses();
    if let Ok(report) = &result {
        for id in &report.installed {
            emit(ServiceEvent::Installed(*id));
        }
    }
    result
}

// ─── 文案 ────────────────────────────────────────────────────

/// 服务器平台键的显示名。
pub fn platform_label(target: &str) -> &'static str {
    match target {
        "linux-x86_64" => t("settings", "extensions.platformLinuxX64"),
        "linux-aarch64" => t("settings", "extensions.platformLinuxArm64"),
        "macos-x86_64" => t("settings", "extensions.platformMacosX64"),
        "macos-aarch64" => t("settings", "extensions.platformMacosArm64"),
        _ => "?",
    }
}

/// 扩展的显示名。
pub fn component_name(id: ComponentId) -> String {
    match (id.kind(), id.helper_target()) {
        (ComponentKind::GitHelper, Some(target)) => tr!(
            "settings",
            "extensions.gitHelperName",
            platform = platform_label(target)
        ),
        (ComponentKind::SshTools, _) => t("settings", "extensions.sshToolsName").to_string(),
        (ComponentKind::SyntaxPack, _) => t("settings", "extensions.syntaxPackName").to_string(),
        (ComponentKind::GitHelper, None) => id.as_str().to_string(),
    }
}

/// 扩展的用途说明。
pub fn component_description(id: ComponentId) -> String {
    match (id.kind(), id.helper_target()) {
        (ComponentKind::GitHelper, Some(target)) => tr!(
            "settings",
            "extensions.gitHelperDesc",
            platform = platform_label(target)
        ),
        (ComponentKind::SshTools, _) => t("settings", "extensions.sshToolsDesc").to_string(),
        (ComponentKind::SyntaxPack, _) => t("settings", "extensions.syntaxPackDesc").to_string(),
        (ComponentKind::GitHelper, None) => String::new(),
    }
}

/// 失败原因的界面文案(本地化短语 + 技术细节)。
pub fn describe(error: &DownloadError) -> String {
    match error {
        DownloadError::Extension(error) => describe_extension_error(error),
        DownloadError::Template => t("settings", "extensions.errTemplate").to_string(),
        DownloadError::Connect(detail) => {
            format!("{}: {detail}", t("settings", "extensions.errConnect"))
        }
        DownloadError::Http(status) => tr!("settings", "extensions.errHttp", status = status),
        DownloadError::Stalled => tr!(
            "settings",
            "extensions.errStalled",
            secs = STALL_TIMEOUT.as_secs()
        ),
        DownloadError::TooLarge => t("settings", "extensions.errTooLarge").to_string(),
        DownloadError::Network(detail) => {
            format!("{}: {detail}", t("settings", "extensions.errNetwork"))
        }
    }
}

pub fn describe_extension_error(error: &ExtensionError) -> String {
    match error {
        ExtensionError::NoManifest => t("settings", "extensions.errNoManifest").to_string(),
        ExtensionError::ManifestTooNew { schema } => {
            tr!("settings", "extensions.manifestTooNew", schema = schema)
        }
        ExtensionError::ManifestInvalid(detail) => {
            tr!("settings", "extensions.manifestInvalid", detail = detail)
        }
        ExtensionError::NotInManifest(_) => {
            t("settings", "extensions.errNotInManifest").to_string()
        }
        ExtensionError::NoRoot => t("settings", "extensions.errNoRoot").to_string(),
        ExtensionError::Integrity(detail) => {
            format!("{}: {detail}", t("settings", "extensions.errIntegrity"))
        }
        ExtensionError::Io(detail) => format!("{}: {detail}", t("settings", "extensions.errIo")),
    }
}

/// 字节数的显示(`1.9 MB` / `512 KB`)。
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

// ─── 接线(UI 线程) ──────────────────────────────────────────

/// 设置页观察的那个实体:后台状态一变就 `notify` 它;某个扩展刚装好(下载或离线导入,不论
/// 谁发起)时另发一条 [`ExtensionInstalled`],要跟着重试的视图订阅它(Git 面板)。
pub struct ExtensionsHub;

/// 一个扩展刚装好。
#[derive(Clone, Copy, Debug)]
pub struct ExtensionInstalled(pub ComponentId);

impl ExtensionInstalled {
    /// 装好的是不是远程 Git helper(任一服务器平台)。
    pub fn is_git_helper(self) -> bool {
        self.0.kind() == ComponentKind::GitHelper
    }
}

impl gpui::EventEmitter<ExtensionInstalled> for ExtensionsHub {}

struct GlobalHub(Entity<ExtensionsHub>);
impl Global for GlobalHub {}

pub fn hub(cx: &App) -> Option<Entity<ExtensionsHub>> {
    cx.try_global::<GlobalHub>().map(|global| global.0.clone())
}

/// 启动接线:同步下载源、起事件泵、注入 mt-remote 的 helper 补齐器、后台跑一次 GC。
/// `AppStore` 装好之后调(要读配置里的下载源)。
pub fn init(cx: &mut App) {
    let template = AppStore::global(cx)
        .read(cx)
        .config()
        .extensions_download_url
        .clone();
    set_download_template(template);

    let hub = cx.new(|_| ExtensionsHub);
    cx.set_global(GlobalHub(hub.clone()));
    let (sender, mut receiver) = mpsc::unbounded::<ServiceEvent>();
    *lock(&service().events) = Some(sender);
    cx.spawn(async move |cx| {
        while let Some(first) = receiver.next().await {
            let mut batch = vec![first];
            while let Ok(more) = receiver.try_recv() {
                batch.push(more);
            }
            cx.update(|cx| dispatch(batch, &hub, cx));
        }
    })
    .detach();

    // 托管 Git 部署缺 helper 时在 Git 执行器线程上调它(部署 deadline 之外):阻塞到该平台的
    // 扩展装好或确定装不上。原因是给用户看的(进 GitFailure 的详情,界面上「设置 → 扩展」那句之前)
    mt_remote::set_helper_provisioner(|target| {
        let id = ComponentId::git_helper(target)
            .ok_or_else(|| t("settings", "extensions.errNotInManifest").to_string())?;
        ensure_blocking(id, Origin::Deploy).map_err(|error| describe(&error))
    });

    // 清单读一次(OnceLock)+ 清理不属于当前清单的旧版本 + 状态缓存。stat / 删目录都是 IO,丢后台
    cx.background_executor()
        .spawn(async {
            let report = extensions().gc();
            if !report.removed.is_empty() || !report.kept.is_empty() {
                tracing::info!(
                    "[extensions] gc removed {} entries, kept {} busy entries",
                    report.removed.len(),
                    report.kept.len()
                );
            }
            refresh_statuses();
        })
        .detach();
}

/// toast 的去重键:每个扩展一条,进度原地更新。
fn toast_key(id: ComponentId) -> String {
    format!("__extension__:{id}")
}

/// 状态缓存里 `id` 是否已在本机可用(已装 / exe 旁旧布局)。只读缓存,不做 IO。
pub fn cached_available(id: ComponentId) -> bool {
    matches!(
        lock(&service().statuses).get(&id),
        Some(ComponentStatus::Installed(_) | ComponentStatus::Bundled)
    )
}

fn dispatch(batch: Vec<ServiceEvent>, hub: &Entity<ExtensionsHub>, cx: &mut App) {
    let mut changed = false;
    // 同一批里同一扩展的进度只推最后那条
    let mut progress: Vec<(ComponentId, u8)> = Vec::new();
    for event in batch {
        match event {
            ServiceEvent::Changed => changed = true,
            ServiceEvent::Installed(id) => {
                let installed = ExtensionInstalled(id);
                if id == ComponentId::SshTools {
                    // SSH 工具装好了:把现存 SKILL.md 改指到这份 CLI(启动校正那时它还不在、
                    // 跳过了;「关联 SSH」在 CLI 缺失时只存了范围的那种也靠这一次补上)
                    let store = AppStore::global(cx);
                    store.read(cx).reconcile_ssh_skills_in_background(cx);
                } else if installed.is_git_helper() {
                    // 远程项目缺 helper 时文件树的 git 标记取不到(失败按空表落),装好了当场
                    // 重取一遍;Git 面板自己订阅下面这条事件,停在失败上的会重新发现仓库
                    crate::git::watch::request_refresh(crate::git::watch::Subscriber::FileTree);
                }
                hub.update(cx, |_, cx| cx.emit(installed));
            }
            ServiceEvent::AutoProgress(id, percent) => {
                progress.retain(|(other, _)| *other != id);
                progress.push((id, percent));
            }
            ServiceEvent::AutoFinished(id, result) => {
                progress.retain(|(other, _)| *other != id);
                let name = component_name(id);
                match result {
                    Ok(()) => crate::toast::push_keyed(
                        &toast_key(id),
                        crate::notify::ToastKind::Info,
                        t("settings", "extensions.toastReadyTitle").to_string(),
                        tr!("settings", "extensions.toastReady", name = name),
                        SettingsPage::Extensions,
                        cx,
                    ),
                    Err(error) => crate::toast::push_keyed(
                        &toast_key(id),
                        crate::notify::ToastKind::Error,
                        t("settings", "extensions.toastFailedTitle").to_string(),
                        tr!(
                            "settings",
                            "extensions.toastFailed",
                            name = name,
                            reason = describe(&error)
                        ),
                        SettingsPage::Extensions,
                        cx,
                    ),
                }
            }
        }
    }
    for (id, percent) in progress {
        crate::toast::push_keyed(
            &toast_key(id),
            crate::notify::ToastKind::Info,
            t("settings", "extensions.toastDownloadingTitle").to_string(),
            tr!(
                "settings",
                "extensions.toastDownloading",
                name = component_name(id),
                percent = percent
            ),
            SettingsPage::Extensions,
            cx,
        );
    }
    if changed {
        hub.update(cx, |_, cx| cx.notify());
        // 语法包刚下载 / 导入好:不必重启,当场加载(按需扩展阶段 C)
        crate::syntax_pack::on_extensions_changed(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 下载地址模板替换与校验() {
        assert_eq!(
            resolve_url(
                None,
                "v1.15.0",
                "mt-ext_git-helper-linux-x86_64_0123456789ab.zip"
            )
            .unwrap(),
            "https://github.com/dreamlonglll/mini-term/releases/download/v1.15.0/mt-ext_git-helper-linux-x86_64_0123456789ab.zip"
        );
        // 空串 / 纯空白 = 默认
        assert_eq!(
            resolve_url(Some("  "), "v1", "a.zip").unwrap(),
            resolve_url(None, "v1", "a.zip").unwrap()
        );
        assert_eq!(
            resolve_url(
                Some(" https://mirror.example/gh/{tag}/{asset} "),
                "v1",
                "a.zip"
            )
            .unwrap(),
            "https://mirror.example/gh/v1/a.zip"
        );
        // 镜像可以按资产名平铺,不要求 {tag}
        assert_eq!(
            resolve_url(Some("http://10.0.0.2/ext/{asset}"), "v1", "a.zip").unwrap(),
            "http://10.0.0.2/ext/a.zip"
        );
        for bad in [
            "ftp://mirror/{asset}",
            "https://mirror/{tag}",
            "file:///C:/ext/{asset}",
            "https://mirror/a b/{asset}",
            "mirror/{asset}",
        ] {
            assert!(!is_valid_template(bad), "{bad}");
            assert_eq!(
                resolve_url(Some(bad), "v1", "a.zip"),
                Err(DownloadError::Template)
            );
        }
        assert!(is_valid_template(DEFAULT_DOWNLOAD_TEMPLATE));
    }

    #[test]
    fn 进度百分比() {
        assert_eq!(
            JobState::Downloading {
                received: 0,
                total: 0
            }
            .percent(),
            0
        );
        assert_eq!(
            JobState::Downloading {
                received: 50,
                total: 200
            }
            .percent(),
            25
        );
        assert_eq!(
            JobState::Downloading {
                received: 300,
                total: 200
            }
            .percent(),
            100
        );
        assert_eq!(JobState::Verifying.percent(), 100);
    }

    /// 百分比变了才报;一阵密集的块之后数据停住(卡在半路等停滞超时),最后报出去的就是
    /// 眼下收到的那份 —— 旧的按 200ms 节流只报窗口里的第一块,E2E 里收到 9.6% 时界面
    /// 一直停在 0%。
    #[test]
    fn 进度按百分比变化上报_停住时不丢最后一截() {
        let total = 2_730_836;
        let mut gate = ProgressGate::default();
        assert_eq!(gate.check(16_384, total), Some(0), "第一块总要报");
        assert_eq!(gate.check(20_000, total), None, "同一个百分比不重复报");
        let mut last = Some(0);
        let mut reports = 1;
        let mut received = 20_000;
        while received < 262_144 {
            received = (received + 16_384).min(262_144);
            if let Some(percent) = gate.check(received, total) {
                last = Some(percent);
                reports += 1;
            }
        }
        assert_eq!(last, Some(9), "停住前的最后一截报出去了");
        assert_eq!(reports, 10, "0..=9 各一条");
        assert_eq!(gate.check(total, total), Some(100));
        assert_eq!(gate.check(total, total), None);
    }

    /// 加入已在跑的任务:后来者要推 toast、发起方不推时改按后来者;其余照旧。
    #[test]
    fn 加入任务时按需改推_toast() {
        let mut origin = Origin::User;
        assert!(adopt_origin(&mut origin, Origin::SyntaxBanner));
        assert_eq!(origin, Origin::SyntaxBanner, "设置页先点、提示条后点");
        let mut origin = Origin::SelfHeal;
        assert!(adopt_origin(&mut origin, Origin::Deploy));
        assert_eq!(origin, Origin::Deploy);
        let mut origin = Origin::SelfHeal;
        assert!(adopt_origin(&mut origin, Origin::EnableSshTools));
        assert_eq!(origin, Origin::EnableSshTools);
        // 发起方本来就推:不换(进度 / 结局 toast 已经在推了)
        let mut origin = Origin::SyntaxBanner;
        assert!(!adopt_origin(&mut origin, Origin::Deploy));
        assert_eq!(origin, Origin::SyntaxBanner);
        // 后来者不推:不换
        let mut origin = Origin::Deploy;
        assert!(!adopt_origin(&mut origin, Origin::User));
        assert_eq!(origin, Origin::Deploy);
        let mut origin = Origin::User;
        assert!(!adopt_origin(&mut origin, Origin::SelfHeal));
        assert_eq!(origin, Origin::User);
    }

    /// 起一个只答一次的环回 HTTP 服务,返回地址。
    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}/ext.zip", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        (url, server)
    }

    fn fetch(url: String, path: PathBuf, expected: u64) -> Result<(), DownloadError> {
        futures::executor::block_on(crate::http::run_async(fetch_to_file(
            url,
            path,
            expected,
            |_| {},
        )))
        .expect("共享运行时调用应成功")
    }

    fn temp_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mt-app-ext-fetch-{label}-{}.part",
            std::process::id()
        ))
    }

    #[test]
    fn 下载按清单大小收下并写盘() {
        let body = b"extension zip bytes";
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let (url, server) = serve_once(response);
        let path = temp_file("ok");
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        futures::executor::block_on(crate::http::run_async(fetch_to_file(
            url,
            path.clone(),
            body.len() as u64,
            move |received| lock(&sink).push(received),
        )))
        .expect("共享运行时调用应成功")
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(
            lock(&seen).last(),
            Some(&(body.len() as u64)),
            "每块都交给进度回调"
        );
        let _ = std::fs::remove_file(&path);
        server.join().unwrap();
    }

    #[test]
    fn 下载超过清单大小立刻断() {
        // 声明的长度就超了:看响应头就拒
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let path = temp_file("declared");
        assert_eq!(fetch(url, path.clone(), 10), Err(DownloadError::TooLarge));
        let _ = std::fs::remove_file(&path);
        server.join().unwrap();

        // 不声明长度、边下边超
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(&[7u8; 64]);
        let (url, server) = serve_once(response);
        let path = temp_file("streamed");
        assert_eq!(fetch(url, path.clone(), 10), Err(DownloadError::TooLarge));
        let _ = std::fs::remove_file(&path);
        server.join().unwrap();
    }

    #[test]
    fn 下载源报错与没下完都算失败() {
        let (url, server) = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let path = temp_file("404");
        assert_eq!(fetch(url, path.clone(), 10), Err(DownloadError::Http(404)));
        server.join().unwrap();

        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(b"short");
        let (url, server) = serve_once(response);
        let error = fetch(url, path.clone(), 10).unwrap_err();
        assert!(
            matches!(&error, DownloadError::Network(detail) if detail.contains("incomplete")),
            "{error:?}"
        );
        let _ = std::fs::remove_file(&path);
        server.join().unwrap();
    }

    #[test]
    fn 体积显示() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(2_712_345), "2.6 MB");
    }
}
