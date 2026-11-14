//! 文本文件的编码识别、解码与编码:让 GBK / GB18030 / Shift_JIS / UTF-16 这些非 UTF-8
//! 的源码也能预览、编辑、看 diff。
//!
//! 识别顺序、「大体是 UTF-8」的判据、交给检测器的样本截取与 UTF-16 的写出,移植自
//! Winc(Apache-2.0)`crates/winc-editor/src/document/encoding.rs`(`decode_in`、
//! `mostly_utf8`、`detection_sample`、`encode` 与地区提示那几段)。与原实现的差异:
//!
//! 1. **合法 UTF-8 一律原样返回、BOM 也留在文本里**(不单列「UTF-8 BOM」一种编码)。
//!    本仓的编辑器、Git 变更标记、按块暂存、`blame --contents` 全按「文本 = 原字节的
//!    UTF-8 解读」对齐 —— 把 BOM 剥掉,HEAD 那一侧(git 给的原字节)第一行就永远对不上。
//!    UTF-8 文件的行为因此与接入编码识别之前逐字节相同。
//! 2. **二进制判定排在合法 UTF-8 之后**:合法 UTF-8 里有 NUL 照旧当文本(此前的口径);
//!    不是 UTF-8 的,开头 8 KiB 里有 NUL 才算二进制(Winc 的 `looks_binary`)。
//! 3. **猜出的旧编码要能逐字节写回才可编辑**:解码后立刻按同一编码再编码一遍,与原字节
//!    不一致(同一个字有两种字节写法,如 GBK 的 0x80 与 GB18030 的 A2E3 都是 €)就按
//!    [`TextEncoding::Lossy`] 只读打开 —— 改一个字不能把文件里没碰过的字节也换掉。
//! 4. 没有「通过编码重新打开 / 保存」的菜单,`CHOICES` 那张表不搬。
//!
//! 编解码走 `encoding_rs`,猜编码走 `chardetng`(Firefox 同款)。`encoding_rs` 按网页
//! 规范不做 UTF-16 **编码**,UTF-16 的写出在 [`TextEncoding::encode`] 里自己拼。

use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// 再导出:[`TextEncoding::Legacy`] 装的就是它的编码常量(`encoding_rs::GB18030` …),
/// 下游(`mt-remote` 的单测等)不必为几个常量再直接依赖一份。
pub use encoding_rs;

/// 文件的编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// 合法的 UTF-8(带不带 BOM 都是它;BOM 原样留在文本开头,写回时原样写出)。
    Utf8,
    /// 带 BOM 的 UTF-16 小端。没有 BOM 的 UTF-16 认不出:满篇 NUL,按二进制处理。
    Utf16Le,
    /// 带 BOM 的 UTF-16 大端。
    Utf16Be,
    /// 其他编码(GB18030、Shift_JIS、Big5、windows-1252 …)。猜成 GBK 的一律记 GB18030
    /// (理由见 [`decode_in`])。
    Legacy(&'static encoding_rs::Encoding),
    /// 没法按任何编码无损解码:坏字节显示为 U+FFFD,**只读**,保存会把原文件写坏。
    Lossy,
}

impl TextEncoding {
    /// 界面上显示的短名(`UTF-8` / `GB18030` / `Shift_JIS` / `UTF-16 LE`)。
    /// [`TextEncoding::Lossy`] 没有编码名,返回空串,由界面自己给文案。
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Legacy(e) if e == encoding_rs::GB18030 => "GB18030",
            Self::Legacy(e) if e == encoding_rs::WINDOWS_1252 => "Windows-1252",
            Self::Legacy(e) => e.name(),
            Self::Lossy => "",
        }
    }

    /// 是原样的 UTF-8。按块暂存 / 丢弃这类「按 UTF-8 文本拼接后写回字节」的操作只认它。
    pub fn is_utf8(self) -> bool {
        self == Self::Utf8
    }

    /// 能不能按这个编码写回。
    pub fn writable(self) -> bool {
        self != Self::Lossy
    }

    /// 按这个编码把 `text` 编成要落盘的字节(UTF-16 带上 BOM;UTF-8 的 BOM 本来就在
    /// 文本里)。
    ///
    /// # Errors
    ///
    /// 编码表示不了文本里的某个字符时报第一个这样的字符(连同行号);
    /// [`TextEncoding::Lossy`] 一律拒绝。**宁可不存,也不能写出一个与编辑器里不一样的文件。**
    pub fn encode(self, text: &str) -> Result<Vec<u8>, EncodeError> {
        match self {
            Self::Utf8 => Ok(text.as_bytes().to_vec()),
            Self::Lossy => Err(EncodeError::ReadOnly),
            // `encoding_rs` 遵循网页规范,不做 UTF-16 编码(它的 UTF-16 编码器输出的是
            // UTF-8),这里自己拼
            Self::Utf16Le | Self::Utf16Be => {
                let le = self == Self::Utf16Le;
                let mut out = if le { UTF16LE_BOM } else { UTF16BE_BOM }.to_vec();
                out.reserve(text.len() * 2);
                for unit in text.encode_utf16() {
                    out.extend_from_slice(&if le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                Ok(out)
            }
            Self::Legacy(codec) => {
                let (bytes, _, had_errors) = codec.encode(text);
                if !had_errors {
                    return Ok(bytes.into_owned());
                }
                // 表示不了的字符被 `encoding_rs` 换成了 `&#…;`:逐字符单独编码,找出第一个
                let mut buf = [0u8; 4];
                let (index, ch) = text
                    .char_indices()
                    .find(|(_, c)| codec.encode(c.encode_utf8(&mut buf)).2)
                    .unwrap_or((0, '\u{fffd}'));
                Err(EncodeError::Unmappable {
                    ch,
                    line: text[..index].matches('\n').count() + 1,
                    encoding: self.label(),
                })
            }
        }
    }

    /// 同 [`Self::encode`],但不拒绝:表示不了的字符按 `encoding_rs` 的替换写法(`&#…;`)
    /// 写出,只读的按 UTF-8 写出。**只给「拿去比较、不落盘」的场合用**
    /// (`git blame --contents` 喂的缓冲区)。
    pub fn encode_lossy(self, text: &str) -> Vec<u8> {
        match self {
            Self::Legacy(codec) => codec.encode(text).0.into_owned(),
            other => other
                .encode(text)
                .unwrap_or_else(|_| text.as_bytes().to_vec()),
        }
    }
}

/// 序列化成编码名([`FileContentResult`](crate::fs::FileContentResult) 带着 `Serialize`)。
impl Serialize for TextEncoding {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Lossy => "lossy",
            other => other.label(),
        })
    }
}

/// Only canonical names emitted by this module are accepted. Web encoding aliases
/// such as replacement/UTF-7 must never silently become a writable codec.
impl<'de> Deserialize<'de> for TextEncoding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        let encoding = match name.as_str() {
            "UTF-8" => Self::Utf8,
            "UTF-16 LE" => Self::Utf16Le,
            "UTF-16 BE" => Self::Utf16Be,
            "lossy" => Self::Lossy,
            _ => {
                let codec = encoding_rs::Encoding::for_label(name.as_bytes())
                    .ok_or_else(|| serde::de::Error::custom("unknown text encoding"))?;
                if codec == encoding_rs::UTF_8
                    || codec == encoding_rs::UTF_16LE
                    || codec == encoding_rs::UTF_16BE
                    || codec == encoding_rs::REPLACEMENT
                {
                    return Err(serde::de::Error::custom("unsafe text encoding alias"));
                }
                let value = Self::Legacy(codec);
                if value.label() != name {
                    return Err(serde::de::Error::custom("noncanonical text encoding"));
                }
                value
            }
        };
        Ok(encoding)
    }
}

/// [`TextEncoding::encode`] 拒绝写回的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 第 `line` 行(1 起)的 `ch` 无法用 `encoding` 表示。
    Unmappable {
        ch: char,
        line: usize,
        encoding: &'static str,
    },
    /// 打开时就没能无损解码,只读。
    ReadOnly,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmappable { ch, line, encoding } => write!(
                f,
                "第 {line} 行的「{ch}」无法用 {encoding} 编码保存,文件未改动"
            ),
            Self::ReadOnly => write!(f, "该文件无法无损解码,只读打开,不能保存"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 解码的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Text {
        text: String,
        encoding: TextEncoding,
    },
    Binary,
}

/// 用户所在的地区(顶级域名写法,如 `cn`),猜编码时作参考;启动时按系统语言设置一次。
///
/// `chardetng` 拿网站的顶级域名作提示:很短的 GBK 文本与 EUC-KR、Big5 难以区分,知道
/// 用户用简体中文时倾向判为 GBK。
static REGION: OnceLock<String> = OnceLock::new();

/// 按系统语言(`zh-CN` / `zh-Hans-CN` / `zh_TW.UTF-8` / `ja`)设置地区。只有第一次
/// 调用有效;推不出地区时不设置。
pub fn set_region_from_language(language: &str) {
    if let Some(region) = region_of(language) {
        let _ = REGION.set(region);
    }
}

/// 当前已选的编码地区提示;远程 helper 应转交此值,而不是读取远端 locale。
pub fn region_hint() -> Option<&'static str> {
    REGION.get().map(String::as_str)
}

/// 接收另一个进程已选的地区提示(`cn` / `tw` / `jp` 等两字母地区码)。
/// 与语言初始化一样,只有第一次有效设置生效;空值、非两字母值不设置。
pub fn set_region_hint(region: &str) {
    if region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        let _ = REGION.set(region.to_ascii_lowercase());
    }
}

/// 系统语言 → 地区(顶级域名写法):带地区码时取地区码;简体中文、繁体中文、日文、
/// 韩文没写地区码时按语言补;其余返回 `None`。
fn region_of(language: &str) -> Option<String> {
    let language = language.split(['.', '@']).next()?;
    let parts: Vec<&str> = language.split(['-', '_']).collect();
    if let Some(region) = parts
        .iter()
        .skip(1)
        .find(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return Some(region.to_ascii_lowercase());
    }
    let script = parts.get(1).map(|s| s.to_ascii_lowercase());
    match (parts[0].to_ascii_lowercase().as_str(), script.as_deref()) {
        ("zh", Some("hant")) => Some("tw".into()),
        ("zh", _) => Some("cn".into()),
        ("ja", _) => Some("jp".into()),
        ("ko", _) => Some("kr".into()),
        _ => None,
    }
}

/// UTF-8 的 BOM:`EF BB BF`。
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
/// UTF-16 小端的 BOM:`FF FE`。
const UTF16LE_BOM: [u8; 2] = [0xFF, 0xFE];
/// UTF-16 大端的 BOM:`FE FF`。
const UTF16BE_BOM: [u8; 2] = [0xFE, 0xFF];

/// 二进制判定读的前缀长度(8 KiB,同 git 的 `FIRST_FEW_BYTES` 量级)。
const BINARY_SNIFF: usize = 8 << 10;

/// 交给 `chardetng` 的最多字节数(64 KiB)。只影响猜:猜出的编码仍须无损解码**整个**
/// 文件才采用;样本长度固定,猜测的耗时与文件大小无关。
const DETECT_SAMPLE: usize = 64 << 10;

/// 以 UTF-16 的 BOM 开头。这种文件满篇 NUL,但不是二进制(git 的 `is_binary` 却会把它
/// 判成二进制,diff 那一侧要先看这个)。
pub fn has_utf16_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&UTF16LE_BOM) || bytes.starts_with(&UTF16BE_BOM)
}

/// 是不是二进制:开头 [`BINARY_SNIFF`] 字节里有 NUL(带 UTF-16 BOM 的除外)。
pub fn looks_binary(bytes: &[u8]) -> bool {
    !has_utf16_bom(bytes) && bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0)
}

/// 按打开文件的规则把整份字节解成文字,地区提示取 [`set_region_from_language`] 设的那个。
pub fn decode(bytes: Vec<u8>) -> Decoded {
    decode_in(bytes, region_hint())
}

/// 同 [`decode`],地区提示由调用方给(`cn` / `tw` / `jp`)。依次:
///
/// 1. UTF-16 的 BOM(UTF-16 满篇 NUL,必须先于二进制判定);
/// 2. 合法 UTF-8 —— 原样返回,不复制,BOM 留在文本里(模块注释差异 1);
/// 3. 二进制(开头 8 KiB 有 NUL);
/// 4. 带 UTF-8 BOM 却有坏字节、或大体是 UTF-8 只坏了几处 → 按替换字符只读打开,免得被
///    猜成别的编码整篇乱码;
/// 5. `chardetng` 猜。GBK 一律记成 GB18030:`encoding_rs` 的 GBK 解码器认四字节扩展,
///    编码器却只会写两字节形式,含 emoji / 生僻字的文件记成 GBK 就存不回去。猜出的编码
///    必须能无损解码整个文件、且再编码逐字节一致才采用,否则只读。
pub fn decode_in(bytes: Vec<u8>, region: Option<&str>) -> Decoded {
    for (bom, encoding, codec) in [
        (UTF16LE_BOM, TextEncoding::Utf16Le, encoding_rs::UTF_16LE),
        (UTF16BE_BOM, TextEncoding::Utf16Be, encoding_rs::UTF_16BE),
    ] {
        if let Some(rest) = bytes.strip_prefix(&bom) {
            return match codec.decode_without_bom_handling_and_without_replacement(rest) {
                Some(text) => Decoded::Text {
                    text: text.into_owned(),
                    encoding,
                },
                // 不完整或损坏的 UTF-16:仍按 UTF-16 解(坏处换成 U+FFFD)、只读;按 UTF-8
                // 解只会得到夹满 NUL 的乱码
                None => Decoded::Text {
                    text: codec.decode_without_bom_handling(rest).0.into_owned(),
                    encoding: TextEncoding::Lossy,
                },
            };
        }
    }

    let bytes = match String::from_utf8(bytes) {
        Ok(text) => {
            return Decoded::Text {
                text,
                encoding: TextEncoding::Utf8,
            };
        }
        Err(err) => err.into_bytes(),
    };

    if looks_binary(&bytes) {
        return Decoded::Binary;
    }
    if bytes.starts_with(&UTF8_BOM) || mostly_utf8(&bytes) {
        return lossy(&bytes);
    }

    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(detection_sample(&bytes), true);
    let guess = detector.guess(region.map(str::as_bytes), false);
    // GB18030 写不回(同一个字两种写法,见模块注释差异 3)时再试 GBK 本身
    let candidates: &[&'static encoding_rs::Encoding] = if guess == encoding_rs::GBK {
        &[encoding_rs::GB18030, encoding_rs::GBK]
    } else {
        &[guess]
    };
    for &codec in candidates {
        if codec == encoding_rs::UTF_8 {
            continue;
        }
        let Some(text) = codec.decode_without_bom_handling_and_without_replacement(&bytes) else {
            continue;
        };
        let (again, _, had_errors) = codec.encode(&text);
        if !had_errors && again.as_ref() == bytes.as_slice() {
            return Decoded::Text {
                text: text.into_owned(),
                encoding: TextEncoding::Legacy(codec),
            };
        }
    }
    lossy(&bytes)
}

/// 交给 `chardetng` 的那一段:从第一个非 ASCII 字节所在行的行首起,最多
/// [`DETECT_SAMPLE`] 字节。纯 ASCII 的部分不提供区分编码的信息,跳过;保留所在行的
/// 行首,非 ASCII 字符前后的拉丁字母仍参与判断。
fn detection_sample(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|b| !b.is_ascii()).unwrap_or(0);
    let start = bytes[..first]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    &bytes[start..bytes.len().min(start + DETECT_SAMPLE)]
}

/// 大体是 UTF-8:合法的非 ASCII 字符数不少于坏处(非法字节序列)个数的 8 倍。
///
/// GBK、Big5 的字节恰好构成合法 UTF-8 多字节字符的情况很少,单字节编码(windows-1252)
/// 的非 ASCII 字节几乎都构不成;8 倍是 Winc 调参得来的,用于容忍拼接、截断造成的少量
/// 损坏。
fn mostly_utf8(mut bytes: &[u8]) -> bool {
    let (mut good, mut bad) = (0usize, 0usize);
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                good += text.chars().filter(|c| !c.is_ascii()).count();
                break;
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                good += std::str::from_utf8(valid)
                    .map_or(0, |t| t.chars().filter(|c| !c.is_ascii()).count());
                bad += 1;
                match e.error_len() {
                    Some(n) => bytes = &rest[n..],
                    // 末尾是不完整的字符
                    None => break,
                }
            }
        }
    }
    good >= 8 * bad
}

/// 按替换字符解码,只读。
fn lossy(bytes: &[u8]) -> Decoded {
    Decoded::Text {
        text: String::from_utf8_lossy(bytes).into_owned(),
        encoding: TextEncoding::Lossy,
    }
}

#[cfg(test)]
#[path = "encoding_tests.rs"]
mod tests;
