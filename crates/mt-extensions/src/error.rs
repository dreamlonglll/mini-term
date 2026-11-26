//! 扩展操作的错误。分类给宿主选文案(mt-app 按变体取 i18n 句子,详情原样附在后面),
//! 详情是不进界面翻译的技术细节(英文,带路径 / 期望值)。

use std::fmt;

use crate::ComponentId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionError {
    /// 此构建没有附带扩展清单(dev / 源码构建):不提供下载与离线导入。
    NoManifest,
    /// 清单的 schema 比本版本认识的新(装的是更新版本的清单?)。
    ManifestTooNew { schema: u64 },
    /// 清单存在但读不懂 / 不合规。
    ManifestInvalid(String),
    /// 本平台清单里没有这个扩展。
    NotInManifest(ComponentId),
    /// 定位不到本机数据目录(极少见:系统 API 失败且没设 `MT_APP_DATA_DIR`)。
    NoRoot,
    /// 校验没过:zip / 文件的字节数或 sha256 对不上、条目不合规、不是本版本的扩展。
    Integrity(String),
    /// 读写失败(权限 / 盘满 / 被占用)。
    Io(String),
}

impl ExtensionError {
    pub(crate) fn io(context: impl fmt::Display, error: std::io::Error) -> Self {
        Self::Io(format!("{context}: {error}"))
    }

    pub(crate) fn integrity(detail: impl Into<String>) -> Self {
        Self::Integrity(detail.into())
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoManifest => f.write_str("this build ships no extension manifest"),
            Self::ManifestTooNew { schema } => {
                write!(
                    f,
                    "extension manifest schema {schema} is newer than supported"
                )
            }
            Self::ManifestInvalid(detail) => write!(f, "invalid extension manifest: {detail}"),
            Self::NotInManifest(id) => {
                write!(f, "extension {id} is not available for this platform")
            }
            Self::NoRoot => f.write_str("local extension directory is unavailable"),
            Self::Integrity(detail) => write!(f, "extension verification failed: {detail}"),
            Self::Io(detail) => write!(f, "extension file operation failed: {detail}"),
        }
    }
}

impl std::error::Error for ExtensionError {}
