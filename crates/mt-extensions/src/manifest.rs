//! 扩展清单 `extensions.json`(schema 1)的解析与校验。
//!
//! ```json
//! {
//!   "schema": 1,
//!   "appVersion": "1.15.0",
//!   "tag": "v1.15.0",
//!   "platform": "windows-x64",
//!   "offlineBundle": "Mini-Term_1.15.0_windows-x64-extensions.zip",
//!   "components": [
//!     {
//!       "id": "git-helper-linux-x86_64",
//!       "asset": "mt-ext_git-helper-linux-x86_64_0123456789ab.zip",
//!       "sha256": "<zip 的 sha256>",
//!       "size": 2712345,
//!       "files": [
//!         { "name": "mt-git-helper", "sha256": "<解包后>", "size": 7740000, "executable": true }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! 由 CI 的 package-release 用 `scripts/pack-extensions.mjs manifest` 生成,随安装包放在
//! 主程序 exe 同目录。解析口径:
//!
//! - 未知字段忽略;`schema` 高于 [`SCHEMA_VERSION`] → 整份不认([`ManifestError::TooNew`],
//!   界面提示「清单版本过新」);
//! - 组件 id 只认已知集合([`ComponentId`]),不认识的条目整条忽略(前向兼容);认识的
//!   条目任何一处不合规 → 整份不认 —— 清单与 exe 同一信任等级,它坏了说明安装本身坏了,
//!   不该挑着用;
//! - 文件名只许单段:无分隔符、无 `..`、不以 `.` 开头(见 [`is_safe_file_name`]);
//! - 体积上限:单文件 [`MAX_FILE_BYTES`]、zip [`MAX_ZIP_BYTES`];
//! - 资产名必须是内容寻址的 `mt-ext_<id>_<sha256 前 12 位>.zip`,与 `sha256` 字段对得上。

use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use crate::ComponentId;

/// 本版本认识的清单 schema。
pub const SCHEMA_VERSION: u64 = 1;
/// 单个解包后文件的体积上限。
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 单个扩展 zip 的体积上限。
pub const MAX_ZIP_BYTES: u64 = 64 * 1024 * 1024;
/// 一个扩展最多几个文件(ssh-tools 是两个,留足余量)。
pub(crate) const MAX_FILES_PER_COMPONENT: usize = 16;
/// 清单文件本身的读取上限。
pub(crate) const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 资产名前缀。
pub(crate) const ASSET_PREFIX: &str = "mt-ext_";
/// 资产名里内容哈希的位数。
const ASSET_HASH_CHARS: usize = 12;
/// 本机存放目录名(`{root}/<id>/<这一段>/`)取 zip sha256 的位数。
pub(crate) const VERSION_DIR_CHARS: usize = 16;
/// 文件名的字节上限。
const MAX_NAME_BYTES: usize = 128;
/// 版本号 / tag / 平台键的字节上限。
const MAX_LABEL_BYTES: usize = 64;

/// 一份校验过的清单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub app_version: String,
    /// 发布 tag(下载地址模板的 `{tag}`)。
    pub tag: String,
    /// 桌面平台键:`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`。
    pub platform: String,
    /// 本平台离线扩展包的资产名(界面上给用户看的那个文件名)。
    pub offline_bundle: Option<String>,
    /// 已知 id 的组件,按清单原序;不认识的 id 已丢弃。
    pub components: Vec<Component>,
}

impl Manifest {
    pub fn component(&self, id: ComponentId) -> Option<&Component> {
        self.components.iter().find(|component| component.id == id)
    }

    /// 按资产名找(离线包条目按它匹配)。
    pub fn component_by_asset(&self, asset: &str) -> Option<&Component> {
        self.components
            .iter()
            .find(|component| component.asset == asset)
    }

    /// 按 zip 的 sha256 + 字节数找(用户直接导入单个扩展 zip 时)。
    pub fn component_by_digest(&self, sha256: &str, size: u64) -> Option<&Component> {
        self.components
            .iter()
            .find(|component| component.size == size && component.sha256 == sha256)
    }
}

/// 清单里的一个扩展。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub id: ComponentId,
    /// 发布资产名(`mt-ext_<id>_<12 位>.zip`)。
    pub asset: String,
    /// zip 的 sha256(小写十六进制)。
    pub sha256: String,
    /// zip 的字节数。
    pub size: u64,
    pub files: Vec<ComponentFile>,
}

impl Component {
    /// 本机存放目录名:zip sha256 的前 16 位。
    pub fn version_dir(&self) -> &str {
        &self.sha256[..VERSION_DIR_CHARS]
    }

    pub fn file(&self, name: &str) -> Option<&ComponentFile> {
        self.files.iter().find(|file| file.name == name)
    }

    /// 解包后的总字节数(界面上「安装后占用」)。
    pub fn unpacked_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

/// 扩展 zip 里的一个文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// Unix 上装成 0755(否则 0644);Windows 上不起作用。
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    TooNew { schema: u64 },
    Invalid(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooNew { schema } => write!(f, "manifest schema {schema} is too new"),
            Self::Invalid(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for ManifestError {}

/// 读清单的结局。`Missing` 是 dev / 源码构建的常态,不是错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestState {
    Missing,
    TooNew { schema: u64 },
    Invalid(String),
    Loaded(Manifest),
}

impl ManifestState {
    pub fn manifest(&self) -> Option<&Manifest> {
        match self {
            Self::Loaded(manifest) => Some(manifest),
            _ => None,
        }
    }
}

impl From<Result<Manifest, ManifestError>> for ManifestState {
    fn from(result: Result<Manifest, ManifestError>) -> Self {
        match result {
            Ok(manifest) => Self::Loaded(manifest),
            Err(ManifestError::TooNew { schema }) => Self::TooNew { schema },
            Err(ManifestError::Invalid(detail)) => Self::Invalid(detail),
        }
    }
}

/// 内容寻址的资产名。`sha256` 必须是 64 位小写十六进制(调用方已校验)。
pub fn asset_name(id: ComponentId, sha256: &str) -> String {
    format!("{ASSET_PREFIX}{id}_{}.zip", &sha256[..ASSET_HASH_CHARS])
}

/// 清单 / zip 里的文件名是不是**单段、安全**的:非空、≤128 字节、无路径分隔符与 `:`、
/// 无 `..`、不以 `.` 开头、不以 `.` / 空格结尾、无控制字符与 Windows 保留字符、不是
/// Windows 设备名(`CON` / `NUL` / `COM1` …)。
pub fn is_safe_file_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return false;
    }
    if name.starts_with('.') || name.ends_with('.') || name.ends_with(' ') || name.contains("..") {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    !device
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    app_version: String,
    tag: String,
    platform: String,
    #[serde(default)]
    offline_bundle: Option<String>,
    components: Vec<Value>,
}

#[derive(Deserialize)]
struct RawComponent {
    asset: String,
    sha256: String,
    size: u64,
    files: Vec<RawFile>,
}

#[derive(Deserialize)]
struct RawFile {
    name: String,
    sha256: String,
    size: u64,
    #[serde(default)]
    executable: bool,
}

fn invalid(detail: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(detail.into())
}

/// 解析并校验一份清单(口径见模块注释)。
pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("not valid JSON: {e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("top level must be a JSON object"))?;
    let schema = object
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("missing integer `schema`"))?;
    if schema > SCHEMA_VERSION {
        return Err(ManifestError::TooNew { schema });
    }
    if schema != SCHEMA_VERSION {
        return Err(invalid(format!("unsupported schema {schema}")));
    }
    let raw: RawManifest =
        serde_json::from_value(value).map_err(|e| invalid(format!("malformed manifest: {e}")))?;
    validate_label("appVersion", &raw.app_version)?;
    validate_label("tag", &raw.tag)?;
    validate_label("platform", &raw.platform)?;
    if let Some(bundle) = &raw.offline_bundle
        && !is_safe_file_name(bundle)
    {
        return Err(invalid(format!("unsafe offlineBundle name {bundle:?}")));
    }

    let mut components: Vec<Component> = Vec::new();
    for entry in raw.components {
        // 不认识的 id(更新版本才有的扩展)整条忽略,连形状都不看
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .and_then(ComponentId::parse)
        else {
            continue;
        };
        if components.iter().any(|component| component.id == id) {
            return Err(invalid(format!("duplicate component {id}")));
        }
        let raw: RawComponent =
            serde_json::from_value(entry).map_err(|e| invalid(format!("component {id}: {e}")))?;
        components.push(validate_component(id, raw)?);
    }
    Ok(Manifest {
        app_version: raw.app_version,
        tag: raw.tag,
        platform: raw.platform,
        offline_bundle: raw.offline_bundle,
        components,
    })
}

fn validate_label(field: &str, value: &str) -> Result<(), ManifestError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("invalid `{field}` {value:?}")))
    }
}

/// 64 位十六进制 → 小写。
fn normalize_sha256(context: &str, value: &str) -> Result<String, ManifestError> {
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(invalid(format!("{context}: sha256 must be 64 hex digits")))
    }
}

fn validate_component(id: ComponentId, raw: RawComponent) -> Result<Component, ManifestError> {
    let sha256 = normalize_sha256(&format!("component {id}"), &raw.sha256)?;
    if raw.size == 0 || raw.size > MAX_ZIP_BYTES {
        return Err(invalid(format!(
            "component {id}: zip size {} outside 1..={MAX_ZIP_BYTES}",
            raw.size
        )));
    }
    let expected = asset_name(id, &sha256);
    if raw.asset != expected {
        return Err(invalid(format!(
            "component {id}: asset {:?} is not the content-addressed name {expected:?}",
            raw.asset
        )));
    }
    if raw.files.is_empty() || raw.files.len() > MAX_FILES_PER_COMPONENT {
        return Err(invalid(format!(
            "component {id}: needs 1..={MAX_FILES_PER_COMPONENT} files, got {}",
            raw.files.len()
        )));
    }
    let mut files: Vec<ComponentFile> = Vec::with_capacity(raw.files.len());
    for file in raw.files {
        if !is_safe_file_name(&file.name) {
            return Err(invalid(format!(
                "component {id}: unsafe file name {:?}",
                file.name
            )));
        }
        // Windows / macOS 默认大小写不敏感:两个只差大小写的文件会互相覆盖
        if files
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&file.name))
        {
            return Err(invalid(format!(
                "component {id}: duplicate file {:?}",
                file.name
            )));
        }
        if file.size > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "component {id}: file {:?} exceeds {MAX_FILE_BYTES} bytes",
                file.name
            )));
        }
        let sha256 = normalize_sha256(
            &format!("component {id} file {:?}", file.name),
            &file.sha256,
        )?;
        files.push(ComponentFile {
            name: file.name,
            sha256,
            size: file.size,
            executable: file.executable,
        });
    }
    Ok(Component {
        id,
        asset: raw.asset,
        sha256,
        size: raw.size,
        files,
    })
}
