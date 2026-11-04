//! PNG → 按显示尺寸缩好的 BGRA。全程跑在 reader 线程上、term 锁之外。
//!
//! # 先读头、再分配
//!
//! 解码前先从 IHDR 拿宽高算一遍([`MAX_DECODED_BYTES`]),超了连解码器都不建:传输
//! 大小限制挡不住解压炸弹。解码器自己的分配上限([`MAX_DECODER_ALLOC`])是第二道,
//! 16 位色深的图中间缓冲是 8 字节一像素。
//!
//! # 不留编码数据,也不留原尺寸
//!
//! 存下来的是显示尺寸的位图(一格多少像素由渲染层报上来),4K 截图摆在 40 列里就只占
//! 那 40 列的纹理。**只缩不放**:显示得比原图大时用原图,放大交给 GPU。
//!
//! 字节序是 BGRA,直通 alpha:`gpui::RenderImage` 的帧要 BGRA,Windows 后端的混合状态是
//! `SRC_ALPHA / INV_SRC_ALPHA`(`directx_renderer.rs` 的 `create_blend_state`)。缩放时
//! 先乘 alpha 再滤波、缩完除回去,否则半透明边缘会带出透明像素里的杂色。

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use image::imageops::{self, FilterType};
use image::{ImageFormat, ImageReader, Limits, RgbaImage};

/// 单图解码后的上限(RGBA8 口径)。
pub(super) const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

/// 解码器的分配上限。16 位 RGBA 一像素 8 字节,像素数已被 [`MAX_DECODED_BYTES`] 卡住,
/// 这里给够两倍。
const MAX_DECODER_ALLOC: u64 = MAX_DECODED_BYTES * 2;

/// 位图边长上限。总字节卡得住内存,卡不住 `16000000×1` 这种形状:GPU 纹理的边长有硬上限
/// (DirectX 11 是 16384,gpui 图集分配失败只会让这张图画不出来)。
const MAX_BITMAP_SIDE: u32 = 8192;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 解好的图。**按 Arc 共享**,渲染层按 [`Self::uid`] 缓存纹理。
pub struct KittyBitmap {
    uid: u64,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    bgra: Vec<u8>,
}

impl KittyBitmap {
    /// 进程内唯一。同一个 id 重传也是新的 uid。
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// 位图宽高(缩放之后)。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 原图宽高。协议里的裁切矩形按它算。
    pub fn source_size(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// 像素,BGRA 直通 alpha,逐行紧排。
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    pub(super) fn byte_len(&self) -> u64 {
        self.bgra.len() as u64
    }
}

impl std::fmt::Debug for KittyBitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KittyBitmap")
            .field("uid", &self.uid)
            .field("size", &self.size())
            .field("source_size", &self.source_size())
            .finish()
    }
}

/// 解码失败的原因。给协议应答用,文字就是应答里的错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    /// 不是 PNG,或者头就是坏的。
    NotPng,
    /// 宽高乘起来超了上限。
    TooLarge,
    /// 头是好的,数据解不开。
    Corrupt,
}

impl DecodeError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::NotPng | Self::Corrupt => "EBADPNG:cannot decode",
            Self::TooLarge => "ENOMEM:image too large",
        }
    }
}

/// 只读 IHDR 取宽高。
pub(super) fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || data[..8] != PNG_SIGNATURE || &data[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// 解一张 PNG。`target` 拿原图宽高给出想要的位图尺寸,只缩不放。
pub(super) fn decode_png(
    data: &[u8],
    target: impl FnOnce((u32, u32)) -> (u32, u32),
) -> Result<Arc<KittyBitmap>, DecodeError> {
    let (width, height) = png_dimensions(data).ok_or(DecodeError::NotPng)?;
    if u64::from(width) * u64::from(height) * 4 > MAX_DECODED_BYTES {
        return Err(DecodeError::TooLarge);
    }

    let mut reader = ImageReader::with_format(Cursor::new(data), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_DECODER_ALLOC);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .map_err(|_| DecodeError::Corrupt)?
        .into_rgba8();
    if rgba.dimensions() != (width, height) {
        return Err(DecodeError::Corrupt);
    }

    let (target_w, target_h) = fit_side(target((width, height)), (width, height));
    let scaled = if (target_w, target_h) == (width, height) {
        rgba
    } else {
        shrink(rgba, target_w, target_h)
    };

    let (out_w, out_h) = scaled.dimensions();
    let mut bgra = scaled.into_raw();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(KittyBitmap {
        uid: next_uid(),
        width: out_w,
        height: out_h,
        source_width: width,
        source_height: height,
        bgra,
    }))
}

fn next_uid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 目标尺寸收进「不比原图大、边长不超上限」。超边长时两边同比例缩,形状不变。
fn fit_side((width, height): (u32, u32), (source_w, source_h): (u32, u32)) -> (u32, u32) {
    let width = width.clamp(1, source_w);
    let height = height.clamp(1, source_h);
    let longest = width.max(height);
    if longest <= MAX_BITMAP_SIDE {
        return (width, height);
    }
    let shrink = |side: u32| u64::from(side) * u64::from(MAX_BITMAP_SIDE) / u64::from(longest);
    (shrink(width).max(1) as u32, shrink(height).max(1) as u32)
}

/// 缩小。全不透明的图(截图、绝大多数公式图)不必走乘除 alpha 那一遍。
fn shrink(mut rgba: RgbaImage, width: u32, height: u32) -> RgbaImage {
    let translucent = rgba.pixels().any(|pixel| pixel[3] != 255);
    if translucent {
        premultiply(&mut rgba);
    }
    // Triangle 在缩小时按比例放宽采样核,是真正的区域平均(与设置页缩略图同一口径)
    let mut out = imageops::resize(&rgba, width, height, FilterType::Triangle);
    if translucent {
        unpremultiply(&mut out);
    }
    out
}

fn premultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

fn unpremultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

/// 测试夹具:现场编一张 PNG。
#[cfg(test)]
pub(super) fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .expect("编码 PNG");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        encode_png(width, height, &pixels)
    }

    #[test]
    fn 头里读得到宽高() {
        assert_eq!(png_dimensions(&solid(7, 3, [0, 0, 0, 255])), Some((7, 3)));
        assert_eq!(png_dimensions(b"not a png at all, long enough"), None);
        assert_eq!(png_dimensions(&PNG_SIGNATURE), None, "截断");
    }

    #[test]
    fn 解出来是_bgra() {
        let bitmap = decode_png(&solid(2, 2, [10, 20, 30, 255]), |size| size).expect("解码");
        assert_eq!(bitmap.size(), (2, 2));
        assert_eq!(bitmap.source_size(), (2, 2));
        assert_eq!(bitmap.bgra()[..4], [30, 20, 10, 255]);
        assert_eq!(bitmap.byte_len(), 16);
    }

    /// 透明度原样留着,不预乘。
    #[test]
    fn 透明像素是直通_alpha() {
        let bitmap = decode_png(&solid(1, 1, [200, 100, 50, 128]), |size| size).expect("解码");
        assert_eq!(bitmap.bgra(), [50, 100, 200, 128]);
    }

    #[test]
    fn 按目标尺寸缩小且记住原尺寸() {
        let bitmap = decode_png(&solid(400, 200, [1, 2, 3, 255]), |_| (40, 20)).expect("解码");
        assert_eq!(bitmap.size(), (40, 20));
        assert_eq!(bitmap.source_size(), (400, 200));
        assert_eq!(bitmap.byte_len(), 40 * 20 * 4, "只留缩完的那一份");
    }

    /// 只缩不放:目标比原图大就用原图。
    #[test]
    fn 不放大() {
        let bitmap = decode_png(&solid(10, 10, [1, 2, 3, 255]), |_| (500, 500)).expect("解码");
        assert_eq!(bitmap.size(), (10, 10));
        let bitmap = decode_png(&solid(100, 10, [1, 2, 3, 255]), |_| (50, 500)).expect("解码");
        assert_eq!(bitmap.size(), (50, 10), "两个方向各算各的");
    }

    /// 细长的图边长也不能超纹理上限,形状不变。
    #[test]
    fn 边长不超上限() {
        assert_eq!(
            fit_side((20_000, 100), (20_000, 100)),
            (MAX_BITMAP_SIDE, 40)
        );
        assert_eq!(
            fit_side((100, 20_000), (100, 20_000)),
            (40, MAX_BITMAP_SIDE)
        );
        assert_eq!(
            fit_side((100_000, 1), (100_000, 1)),
            (MAX_BITMAP_SIDE, 1),
            "不缩成 0"
        );
        assert_eq!(
            fit_side((8192, 8192), (10_000, 10_000)),
            (8192, 8192),
            "正好"
        );
        assert_eq!(fit_side((0, 0), (10, 10)), (1, 1));
    }

    /// 缩小时透明像素里的杂色不能渗进来:全透明的红 + 不透明的白,缩完只能是白。
    #[test]
    fn 缩小时透明杂色不渗色() {
        let mut pixels = Vec::new();
        for x in 0..64 {
            pixels.extend_from_slice(if x % 2 == 0 {
                &[255, 255, 255, 255]
            } else {
                &[255, 0, 0, 0]
            });
        }
        let bitmap = decode_png(&encode_png(64, 1, &pixels), |_| (8, 1)).expect("解码");
        for pixel in bitmap.bgra().as_chunks::<4>().0 {
            assert!(
                pixel[0] >= 250 && pixel[1] >= 250,
                "蓝绿通道被透明红色拉低了:{pixel:?}"
            );
        }
    }

    /// **解压炸弹**:头上写着超大尺寸,一个字节都不该开始解。
    #[test]
    fn 超限尺寸在分配之前就拒绝() {
        // IHDR 改成 40000×40000:像素数据根本没有,能走到解码器才会报 Corrupt
        let mut png = solid(1, 1, [0, 0, 0, 255]);
        png[16..20].copy_from_slice(&40_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&40_000u32.to_be_bytes());
        assert_eq!(
            decode_png(&png, |size| size).unwrap_err(),
            DecodeError::TooLarge
        );
        // 恰好在上限内的不拒绝(同样没有数据,报的是解不开,不是超限)
        let side = (MAX_DECODED_BYTES / 4).isqrt() as u32;
        png[16..20].copy_from_slice(&side.to_be_bytes());
        png[20..24].copy_from_slice(&side.to_be_bytes());
        assert_eq!(
            decode_png(&png, |size| size).unwrap_err(),
            DecodeError::Corrupt
        );
    }

    #[test]
    fn 坏数据不崩() {
        let png = solid(32, 32, [9, 9, 9, 255]);
        assert_eq!(
            decode_png(&png[..png.len() / 2], |size| size).unwrap_err(),
            DecodeError::Corrupt,
            "截断"
        );
        assert_eq!(
            decode_png(b"garbage", |size| size).unwrap_err(),
            DecodeError::NotPng
        );
        assert_eq!(
            decode_png(&[], |size| size).unwrap_err(),
            DecodeError::NotPng
        );
    }

    #[test]
    fn 每张图的_uid_都不同() {
        let png = solid(1, 1, [0, 0, 0, 255]);
        let a = decode_png(&png, |size| size).expect("解码");
        let b = decode_png(&png, |size| size).expect("解码");
        assert_ne!(a.uid(), b.uid());
    }
}
