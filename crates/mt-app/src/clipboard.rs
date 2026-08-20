//! 终端粘贴的落盘转换:剪贴板图片(截图)与长文本(audit #30 的主体)。
//!
//! 对应 `src/utils/terminalCache.ts:670-774` 的阈值判定与粘贴主流程、
//! `src/utils/pastePath.ts` 的三类 pane 路径映射,以及
//! `src-tauri/src/clipboard.rs` 的 Win32 图片读取、临时文件落盘与 24h 清理。
//!
//! # 为什么落在 mt-app 而不是 mt-ui
//!
//! 阈值判定要读 [`AppConfig`](mt_config::AppConfig)、路径映射要知道项目是不是
//! WSL / 远程 —— 那都是壳的东西。`mt_ui::TerminalView` 只留一个
//! [`on_paste`](mt_ui::TerminalView::on_paste) 钩子,内建的 `paste()` 依旧纯粹。
//!
//! # 两条粘贴路线
//!
//! | 剪贴板内容 | 处理 |
//! |---|---|
//! | 图片 | 落盘临时文件 → 粘带引号的路径([`read_clipboard_image`]);读不出则退 `Alt+V` |
//! | 长文本(过阈值) | 落盘 `.txt` → 粘带引号的路径 |
//! | 其余文本 | 原样粘 |
//!
//! ~~SSH 远程分支不做~~ ✅ BB-a 批补上:见 [`spawn_remote_paste`]。
//!
//! ~~剪贴板图片不做~~ ✅ 补上:见 [`read_clipboard_image`]。GPUI 迁移期这条整块
//! 缺失,表现是**按 Ctrl+V 毫无反应** —— gpui 的剪贴板只认 `PNG`/`JFIF`/`GIF`/
//! `image/svg+xml` 四个注册格式(`platform/windows/clipboard.rs`),而截图工具
//! 放进剪贴板的是 `CF_DIB`,于是 `read_from_clipboard()` 连 `None` 都不给,
//! `resolve_paste` 直接静默返回。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::notify::ToastKind;
use crate::store::AppStore;
use crate::toast;
use crate::tr;

/// 临时文件目录名。图片(`clip-*.png`)与长文本(`paste-*.txt`)同处一处,
/// 且与装机版**共用同一个目录名** —— 两边的 24h 清理因此互相覆盖得到。
const TEMP_DIR: &str = "mini-term-clipboard";

/// 清理阈值:24 小时。
const CLEANUP_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// 判定剪贴板文本是否要转存为临时文件。
///
/// 逐字照抄 `terminalCache.ts:671-678`:**任一阈值命中即转存**,
/// 阈值为 0 表示该维度不判;比较是 `>=` 而不是 `>`。
///
/// ⚠️ 字符数取 **UTF-16 码元数**而不是 `chars().count()` —— JS 的 `String.length`
/// 就是码元数,中文按 1、emoji 按 2。用 `chars()` 会让同一段文本在两个版本里
/// 判定不同(emoji 多的文本尤其明显)。
pub fn is_long_text(text: &str, line_threshold: u32, char_threshold: u32) -> bool {
    if char_threshold > 0 && text.encode_utf16().count() >= char_threshold as usize {
        return true;
    }
    if line_threshold > 0 {
        // CRLF 先归一再按 \n 切,与 `text.replace(/\r\n/g,'\n').split('\n').length` 同
        let lines = text.replace("\r\n", "\n").split('\n').count();
        if lines >= line_threshold as usize {
            return true;
        }
    }
    false
}

/// 临时文件目录(`std::env::temp_dir()/mini-term-clipboard`)。
fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(TEMP_DIR)
}

/// 把长文本写进临时 `.txt`,返回绝对路径。
///
/// 文件名 `paste-{unix_millis}.txt`,与装机版 `save_clipboard_text` 一字不差
/// (`src-tauri/src/clipboard.rs:321-327`)—— 两个版本轮流跑时清理逻辑仍然通用。
/// 错误文案同样照抄(装机版这两句就是硬编码中文,不走 i18n)。
pub fn save_clipboard_text(text: &str) -> Result<PathBuf, String> {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join(format!("paste-{}.txt", unix_millis()));
    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("写入临时文件失败: {e}"))?;
    Ok(path)
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// 删掉临时目录里 mtime 超过 24h 的**全部**文件。启动时调一次。
///
/// 与装机版 `cleanup_old_clipboard_images` 同语义:粘进终端的路径用完就没人管了,
/// 不清的话这个目录会随使用无界增长。目录不存在直接返回(还没粘过)。
pub fn cleanup_old_files() {
    let Ok(entries) = std::fs::read_dir(temp_dir()) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(CLEANUP_AGE) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// 剪贴板图片(截图粘贴)
// ---------------------------------------------------------------------------

/// AI CLI 自取剪贴板图片的转义序列(`Alt+V`)。
///
/// 剪贴板里**确实有图但本进程读不出来**时发这个,让终端里跑着的 AI 工具自己去
/// 读系统剪贴板(装机版 `pasteToTerminalInner` 的同名兜底)。注意它对 SSH 远程
/// pane 无效 —— 那头的 agent 读的是**远端**剪贴板。
pub const ALT_V: &str = "\x1bv";

/// 读剪贴板图片的三态结果。
///
/// 「没有图」与「有图但读不出」必须分开:前者继续走文本分支,后者要退 `Alt+V`。
/// 合并成 `Option` 的话,BI_BITFIELDS 压缩的 DIB(本模块只解 BI_RGB)会被当成
/// 「剪贴板是空的」,又回到按 Ctrl+V 毫无反应。
pub enum ClipboardImage {
    /// 已落盘,值是本机绝对路径。
    Saved(PathBuf),
    /// 剪贴板有图,但解不出 / 存不下 —— 调用方退 [`ALT_V`]。
    Unreadable,
    /// 剪贴板里没有图。
    None,
}

/// 读剪贴板里的图片并落盘,返回本机路径。
///
/// `item` 是调用方**已经取好**的 gpui 剪贴板快照 —— 传进来而不是在这里现读:
/// 判图与随后取文本必须看同一份快照,否则用户在两次读之间换了剪贴板内容,
/// 会出现「判定是图,粘出来是文本」。
///
/// # 探测顺序
///
/// 1. **Windows 的 Win32 直读**(`CF_DIB` → `CF_BITMAP`):截图工具(Win+Shift+S、
///    微信/QQ 截图、PinPix 等)放的就是这两个格式,gpui 一概不认;
/// 2. **gpui 的图片 entry**:认 `PNG`/`JFIF`/`GIF`/`image/svg+xml` 四个注册格式,
///    拿到的是**已编码的原始字节**(浏览器复制图片走这条),原样写盘不重编码。
///
/// 非 Windows 只有第 2 条 —— 装机版这个功能本就只有 Windows,这里顺带让
/// mac/Linux 有个基本能力。
///
/// # 已知边界(与装机版同款,刻意保持)
///
/// 剪贴板同时有图和文本时(Excel/Word 复制单元格、网页图文混排)**按图片处理**。
/// 判据是「有没有图」而不是「有没有文本」,与装机版 `clipboardHasImage()` 优先
/// 的口径一致。
pub fn read_clipboard_image(item: Option<&gpui::ClipboardItem>) -> ClipboardImage {
    #[cfg(windows)]
    match win::read_clipboard_to_png() {
        Ok(path) => return ClipboardImage::Saved(path),
        // 剪贴板里确实没有位图 —— 还可能有 gpui 认得的注册格式,继续往下问
        Err(win::ReadError::NoImage) => {}
        Err(win::ReadError::Failed(detail)) => {
            eprintln!("[clipboard] Win32 读剪贴板图片失败: {detail}");
            // 有图却读不出:gpui 那条路还有机会(某些应用只放 PNG 注册格式),
            // 都不行才认输退 Alt+V。
            return match gpui_image(item) {
                Some(Ok(path)) => ClipboardImage::Saved(path),
                _ => ClipboardImage::Unreadable,
            };
        }
    }

    match gpui_image(item) {
        Some(Ok(path)) => ClipboardImage::Saved(path),
        // 有图但写盘失败:同样别退回文本分支(那会静默),退 Alt+V 至少还有一条路
        Some(Err(detail)) => {
            eprintln!("[clipboard] 剪贴板图片落盘失败: {detail}");
            ClipboardImage::Unreadable
        }
        None => ClipboardImage::None,
    }
}

/// 从 gpui 的剪贴板快照里找图片并落盘。
///
/// 外层 `None` = 里面没有图片 entry;`Some(Err)` = 有图但落盘失败。
fn gpui_image(item: Option<&gpui::ClipboardItem>) -> Option<Result<PathBuf, String>> {
    let image = item?.entries().iter().find_map(|entry| match entry {
        gpui::ClipboardEntry::Image(image) => Some(image),
        gpui::ClipboardEntry::String(_) => None,
    })?;
    Some(save_clipboard_image(&image.bytes, image_ext(image.format)))
}

/// gpui 图片格式 → 落盘扩展名。
///
/// 穷尽匹配(不写 `_`):gpui 哪天加了格式,这里要编译报错提醒补一笔,
/// 而不是默默存成错扩展名。
fn image_ext(format: gpui::ImageFormat) -> &'static str {
    use gpui::ImageFormat as F;
    match format {
        F::Png => "png",
        F::Jpeg => "jpg",
        F::Webp => "webp",
        F::Gif => "gif",
        F::Svg => "svg",
        F::Bmp => "bmp",
        F::Tiff => "tiff",
    }
}

/// 把剪贴板图片的字节写进临时目录,返回绝对路径。
///
/// 文件名 `clip-{unix_millis}.{ext}` —— 前缀与装机版 `read_clipboard_image`
/// (`src-tauri/src/clipboard.rs:196-207`)一字不差,两个版本轮流跑时
/// [`cleanup_old_files`] 的 24h 清理仍然通用。
pub fn save_clipboard_image(bytes: &[u8], ext: &str) -> Result<PathBuf, String> {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join(format!("clip-{}.{ext}", unix_millis()));
    std::fs::write(&path, bytes).map_err(|e| format!("写入临时文件失败: {e}"))?;
    Ok(path)
}

/// Win32 剪贴板直读(`CF_DIB` / `CF_BITMAP` → PNG)。
///
/// 整体搬自装机版 `src-tauri/src/clipboard.rs` 的 `win` 模块,含 `1fcf1bc`
/// 那轮对 [`win::parse_dib`] 的越界读 / 整数溢出加固与三个安全回归测试;
/// `read_bitmap` 的缓冲区尺寸这次一并按同样口径加固(装机版漏了那一处)。
#[cfg(windows)]
pub mod win {
    use std::path::PathBuf;

    use image::{ImageBuffer, RgbaImage};
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, GetDIBits,
        GetObjectW, HBITMAP, SelectObject,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    /// 这两个常量在 `windows` crate 里散落在 Ole 模块且类型不一,直接写字面量
    /// (装机版同款)—— 它们是 Win32 的固化取值,三十年没变过。
    const CF_BITMAP: u32 = 2;
    const CF_DIB: u32 = 8;

    /// 任何一边超过这个像素数就拒绝。65536 远超任何真实截图,拦的是
    /// 剪贴板写入方声称的巨大维度(见 [`parse_dib`] 的加固说明)。
    const MAX_DIM: u32 = 1 << 16;

    /// 读失败的两种含义 —— 调用方据此决定「继续走文本」还是「退 Alt+V」。
    pub enum ReadError {
        /// 剪贴板里没有 `CF_DIB` / `CF_BITMAP`。
        NoImage,
        /// 有图,但拿不到 / 解不出 / 存不下。
        Failed(String),
    }

    /// 尝试从剪贴板读取图片(`CF_DIB` → `CF_BITMAP`),保存为 PNG 到临时目录。
    pub fn read_clipboard_to_png() -> Result<PathBuf, ReadError> {
        unsafe {
            if OpenClipboard(None).is_err() {
                // 剪贴板被别的进程占着(剪贴板管理器监听时很常见)。这里**必须**
                // 报 NoImage 而不是 Failed:此刻根本不知道里面是不是图,报 Failed
                // 会让普通文本粘贴退成 Alt+V —— 那在 bash/readline 里是个真动作,
                // 比「少一次兜底」糟得多。
                return Err(ReadError::NoImage);
            }
            let result = read_inner();
            let _ = CloseClipboard();
            result
        }
    }

    unsafe fn read_inner() -> Result<PathBuf, ReadError> {
        // 两个格式都不在场 = 剪贴板里没图,让调用方去走文本分支。
        let has_dib = unsafe { IsClipboardFormatAvailable(CF_DIB) }.is_ok();
        let has_bitmap = unsafe { IsClipboardFormatAvailable(CF_BITMAP) }.is_ok();
        if !has_dib && !has_bitmap {
            return Err(ReadError::NoImage);
        }

        let mut last = String::new();
        if has_dib {
            match unsafe { read_dib() }.and_then(|img| save_png(&img)) {
                Ok(path) => return Ok(path),
                // BI_BITFIELDS 压缩、异形位深等落在这里 —— 还有 CF_BITMAP 可试
                Err(detail) => last = detail,
            }
        }
        if has_bitmap {
            match unsafe { read_bitmap() }.and_then(|img| save_png(&img)) {
                Ok(path) => return Ok(path),
                Err(detail) => last = detail,
            }
        }
        Err(ReadError::Failed(last))
    }

    unsafe fn read_dib() -> Result<RgbaImage, String> {
        let handle = unsafe { GetClipboardData(CF_DIB) }
            .map_err(|e| format!("GetClipboardData(CF_DIB): {e}"))?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
        if ptr.is_null() {
            return Err("GlobalLock 失败".into());
        }
        let size = unsafe { GlobalSize(hglobal) };
        let result = unsafe { parse_dib(ptr, size) };
        let _ = unsafe { GlobalUnlock(hglobal) };
        result
    }

    /// # Safety
    ///
    /// `ptr` 必须指向至少 `size` 字节的可读内存。函数**只信 `size`**:头部声称的
    /// 维度一律当作敌意输入校验(剪贴板内容由任意进程写入)。
    pub(crate) unsafe fn parse_dib(ptr: *const u8, size: usize) -> Result<RgbaImage, String> {
        if size < std::mem::size_of::<BITMAPINFOHEADER>() {
            return Err("DIB 数据太短".into());
        }

        let header = unsafe { &*(ptr as *const BITMAPINFOHEADER) };
        let width = header.biWidth as u32;
        let height = header.biHeight.unsigned_abs();
        let bit_count = header.biBitCount;
        let compression = header.biCompression;

        if compression != BI_RGB.0 {
            return Err(format!("不支持的 DIB 压缩格式: {compression}"));
        }

        // 只支持 24/32 位真彩(调色板位深从未真正被下方循环支持),提前拒绝可避免
        // palette 偏移歧义,也让后续偏移计算只有一种分支。
        if bit_count != 24 && bit_count != 32 {
            return Err(format!("不支持的位深: {bit_count}"));
        }

        // 尺寸来自剪贴板写入方完全可控的 BITMAPINFOHEADER。biWidth 负值经 `as u32` 会
        // 回绕成巨值,必须用原始 i32 判;并对维度设上限防止 RgbaImage::new 巨额分配。
        if header.biWidth <= 0 || header.biHeight == 0 {
            return Err("DIB 尺寸非法".into());
        }
        if width > MAX_DIM || height > MAX_DIM {
            return Err("DIB 尺寸超出上限".into());
        }

        let pixel_offset = header.biSize as usize; // 24/32 位无调色板,像素紧跟头部
        if pixel_offset >= size {
            return Err("像素数据偏移超出范围".into());
        }

        // 关键加固:全程 usize + checked 运算,并校验整块像素数据落在缓冲区内,
        // 杜绝声称维度远大于实际分配时的越界读 / 整数溢出。
        let stride = ((width as usize) * (bit_count as usize)).div_ceil(32) * 4;
        let pixel_bytes = (height as usize)
            .checked_mul(stride)
            .ok_or("DIB 像素数据长度溢出")?;
        let required = pixel_offset
            .checked_add(pixel_bytes)
            .ok_or("DIB 像素数据长度溢出")?;
        if required > size {
            return Err("DIB 像素数据超出缓冲区范围".into());
        }

        let pixels = unsafe { ptr.add(pixel_offset) };
        let bottom_up = header.biHeight > 0;

        let mut img = RgbaImage::new(width, height);

        for y in 0..height {
            let src_y = if bottom_up { height - 1 - y } else { y };
            let row = unsafe { pixels.add(src_y as usize * stride) };

            for x in 0..width {
                let (r, g, b, a) = if bit_count == 32 {
                    let off = (x as usize) * 4;
                    unsafe {
                        (
                            *row.add(off + 2),
                            *row.add(off + 1),
                            *row.add(off),
                            *row.add(off + 3),
                        )
                    }
                } else {
                    // 24 位
                    let off = (x as usize) * 3;
                    unsafe { (*row.add(off + 2), *row.add(off + 1), *row.add(off), 255) }
                };
                img.put_pixel(x, y, image::Rgba([r, g, b, a]));
            }
        }

        Ok(img)
    }

    unsafe fn read_bitmap() -> Result<RgbaImage, String> {
        let handle = unsafe { GetClipboardData(CF_BITMAP) }
            .map_err(|e| format!("GetClipboardData(CF_BITMAP): {e}"))?;
        let hbitmap = HBITMAP(handle.0);

        let mut bmp = BITMAP::default();
        let ret = unsafe {
            GetObjectW(
                hbitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut _ as *mut _),
            )
        };
        if ret == 0 {
            return Err("GetObjectW 失败".into());
        }

        // 维度同样是外来数据(位图由别的进程创建),照 parse_dib 的口径校验:
        // 负值 / 超大值都拒绝,并用 checked 运算算缓冲区大小 —— 装机版这一处
        // 是裸 `(width * height * 4) as usize`,理论上能整数溢出成小缓冲。
        if bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            return Err("位图尺寸非法".into());
        }
        let width = bmp.bmWidth as u32;
        let height = bmp.bmHeight as u32;
        if width > MAX_DIM || height > MAX_DIM {
            return Err("位图尺寸超出上限".into());
        }
        let buf_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or("位图缓冲区长度溢出")?;

        let hdc = unsafe { CreateCompatibleDC(None) };
        let old = unsafe { SelectObject(hdc, hbitmap.into()) };

        let mut bi = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut buf = vec![0u8; buf_len];

        let ret = unsafe {
            GetDIBits(
                hdc,
                hbitmap,
                0,
                height,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bi as *mut _ as *mut _,
                DIB_RGB_COLORS,
            )
        };

        unsafe { SelectObject(hdc, old) };
        let _ = unsafe { DeleteDC(hdc) };

        if ret == 0 {
            return Err("GetDIBits 失败".into());
        }

        // BGRA → RGBA
        for chunk in buf.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }

        ImageBuffer::from_raw(width, height, buf).ok_or_else(|| "构建图像缓冲区失败".into())
    }

    /// 编成 PNG 落盘。走 [`super::save_clipboard_image`] 以复用同一套文件名与
    /// 24h 清理口径 —— 唯一的区别是这里要先把裸像素编码。
    fn save_png(img: &RgbaImage) -> Result<PathBuf, String> {
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| format!("PNG 编码失败: {e}"))?;
        super::save_clipboard_image(png.get_ref(), "png")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const HDR: usize = std::mem::size_of::<BITMAPINFOHEADER>();

        fn make_header(width: i32, height: i32, bit_count: u16) -> BITMAPINFOHEADER {
            let mut h = BITMAPINFOHEADER::default();
            h.biSize = HDR as u32;
            h.biWidth = width;
            h.biHeight = height;
            h.biPlanes = 1;
            h.biBitCount = bit_count;
            h.biCompression = BI_RGB.0;
            h
        }

        fn buf_with_header(header: &BITMAPINFOHEADER, total: usize) -> Vec<u8> {
            let mut buf = vec![0u8; total.max(HDR)];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    header as *const _ as *const u8,
                    buf.as_mut_ptr(),
                    HDR,
                );
            }
            buf
        }

        // 声称 1000x1000 却只给极小缓冲:必须返回 Err 而不是越界读/panic。
        #[test]
        fn parse_dib_rejects_truncated_pixel_buffer() {
            let header = make_header(1000, 1000, 32);
            let buf = buf_with_header(&header, HDR + 64);
            unsafe {
                assert!(parse_dib(buf.as_ptr(), buf.len()).is_err());
            }
        }

        // 负宽度(as u32 会回绕)与超大维度都必须被拒绝。
        #[test]
        fn parse_dib_rejects_negative_or_oversized_dims() {
            let neg = make_header(-5, 10, 32);
            let big = make_header(1 << 20, 10, 32);
            let buf_neg = buf_with_header(&neg, HDR + 16);
            let buf_big = buf_with_header(&big, HDR + 16);
            unsafe {
                assert!(parse_dib(buf_neg.as_ptr(), buf_neg.len()).is_err());
                assert!(parse_dib(buf_big.as_ptr(), buf_big.len()).is_err());
            }
        }

        // 回归:合法的小图仍能正常解析,确保加固没误伤正常路径。
        #[test]
        fn parse_dib_accepts_valid_small_bitmap() {
            let (w, h) = (2i32, 2i32);
            let header = make_header(w, h, 32);
            let stride = ((w as usize) * 32).div_ceil(32) * 4;
            let buf = buf_with_header(&header, HDR + stride * (h as usize));
            unsafe {
                let img = parse_dib(buf.as_ptr(), buf.len()).expect("合法小图应解析成功");
                assert_eq!((img.width(), img.height()), (2, 2));
            }
        }

        /// 真机手测:先截个图(Win+Shift+S)让剪贴板里有位图,再跑
        ///
        /// ```text
        /// cargo test -p mt-app -- --ignored --nocapture 剪贴板真机读图
        /// ```
        ///
        /// 默认 `#[ignore]` —— 它读的是**真实系统剪贴板**,普通 `cargo test` 里
        /// 那东西是什么完全不可控。Win32 直读这条路没有别的自动化验证手段
        /// (要真的有个 HBITMAP 躺在系统剪贴板里),所以留这个入口。
        #[test]
        #[ignore = "需要先往系统剪贴板里放一张图"]
        fn 剪贴板真机读图() {
            match read_clipboard_to_png() {
                Ok(path) => {
                    let len = std::fs::metadata(&path).expect("落盘文件在").len();
                    println!("读到图片 → {} ({len} 字节)", path.display());
                    assert!(len > 0, "落盘文件不能是空的");
                    // 故意不删:留给人肉眼看一眼图对不对。24h 清理会收走。
                }
                Err(ReadError::NoImage) => panic!("剪贴板里没有位图 —— 先截个图再跑"),
                Err(ReadError::Failed(detail)) => panic!("有图但读不出: {detail}"),
            }
        }

        // BI_BITFIELDS 之类的压缩格式解不出 —— 这条正是 `Alt+V` 兜底存在的理由。
        #[test]
        fn parse_dib_rejects_unsupported_compression() {
            let mut header = make_header(2, 2, 32);
            header.biCompression = 3; // BI_BITFIELDS
            let buf = buf_with_header(&header, HDR + 64);
            unsafe {
                assert!(parse_dib(buf.as_ptr(), buf.len()).is_err());
            }
        }
    }
}

/// Windows 盘符路径 → WSL 内可读路径(`C:\a\b.txt` → `/mnt/c/a/b.txt`)。
///
/// 照抄 `src/utils/wslPath.ts::windowsPathToWsl`。只处理盘符路径(含 `\\?\`
/// verbatim 前缀);UNC / 已是 POSIX 形式的返回 `None`,调用方按原样粘贴。
///
/// 已知边界(原版同款):`/mnt` 是 automount 的默认挂载点,用户在
/// `/etc/wsl.conf` 里改过 `[automount] root=` 时不成立 —— 表现是「文件不存在」,
/// 不会误写。
pub fn windows_path_to_wsl(path: &str) -> Option<String> {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut chars = stripped.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    let sep = chars.next()?;
    if sep != '\\' && sep != '/' {
        return None;
    }
    let rest: String = chars.collect::<String>().replace('\\', "/");
    Some(format!("/mnt/{}/{rest}", drive.to_ascii_lowercase()))
}

/// 这个 pane 落盘的文件该以什么路径粘进去。
///
/// 判定口径刻意与后端起 PTY 的分支一致(`pastePath.ts:13-18` 原注释):
/// 判错的代价不对称 —— 漏判只是回到改动前的行为,误判会把路径指向另一台机器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteTarget {
    /// 本地 shell:原样粘 Windows 路径。
    Local,
    /// WSL:转 `/mnt/<盘符>/...`(文件本身经 automount 就能读到,只差路径形式)。
    Wsl,
    /// SSH 远程项目:转存成本机临时文件后 **SFTP 传到远端**,粘远端绝对路径
    /// (`ssh_remote_upload_paste`)。这一支是异步的,走 [`spawn_remote_paste`],
    /// 不经 [`map_pasted_path`]。
    Ssh,
}

/// pane 用的 shell 是不是 `wsl.exe`(本地项目里手工配了 WSL shell 的情况)。
///
/// 取命令的 **basename** 再比对:既不漏判 `C:\Windows\System32\wsl.exe`,
/// 也不误判 `wslconfig.exe` 这类同前缀命令(`pastePath.ts:46-49` 的同一条注释)。
pub fn command_is_wsl(command: &str) -> bool {
    let base = command
        .trim()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    base == "wsl" || base == "wsl.exe"
}

/// 判断某个 pty 所在 pane 的粘贴目标(`pastePath.ts::resolvePasteTarget`)。
/// 定位不到 pane / 项目时退回 [`PasteTarget::Local`](原版同款兜底)。
pub fn resolve_paste_target(store: &AppStore, pty_id: u32) -> PasteTarget {
    let Some((project_id, pane_id)) = store.pane_of_pty(pty_id) else {
        return PasteTarget::Local;
    };
    let Some(project) = store.project(&project_id) else {
        return PasteTarget::Local;
    };
    if project.ssh_connection_id.is_some() {
        return PasteTarget::Ssh;
    }
    // 项目根是 WSL UNC → 后端起 PTY 时就已经改用 wsl.exe 了(decide_wsl_override)
    if mt_pty::decide_wsl_override(&project.path).is_some() {
        return PasteTarget::Wsl;
    }
    // 本地项目但 pane 自己配了 wsl.exe 当 shell
    let shell_name = store
        .project_state(&project_id)
        .and_then(|s| s.layout.as_ref())
        .and_then(|l| l.pane(&pane_id))
        .map(|p| p.shell_name.clone());
    let runs_wsl = shell_name
        .and_then(|name| {
            store
                .config()
                .available_shells
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.command.clone())
        })
        .is_some_and(|cmd| command_is_wsl(&cmd));
    if runs_wsl {
        PasteTarget::Wsl
    } else {
        PasteTarget::Local
    }
}

/// 把本机临时文件路径映射成「该终端里真正可读的路径」。
///
/// [`PasteTarget::Ssh`] 走不到这里 —— 那一支是异步上传,见 [`spawn_remote_paste`]。
pub fn map_pasted_path(local: &Path, target: PasteTarget) -> String {
    let local = local.to_string_lossy().into_owned();
    match target {
        PasteTarget::Local | PasteTarget::Ssh => local,
        // 转不了(UNC 等非盘符路径)就原样返回,行为退回改动前
        PasteTarget::Wsl => windows_path_to_wsl(&local).unwrap_or(local),
    }
}

// ---------------------------------------------------------------------------
// SSH 远程 pane 的粘贴(异步:转存 → SFTP 上传 → 写远端路径)
// ---------------------------------------------------------------------------

/// 正在处理粘贴的 pty(`terminalCache.ts:680` 的 `pasteInFlight`)。
///
/// 远程上传要几百毫秒到几秒,这期间用户连按 Ctrl+V 会让多条路径以完成顺序
/// **乱序**插进命令行;直接丢弃重入的那次,语义上等同「还没粘完」。
static PASTE_IN_FLIGHT: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn in_flight_begin(pty_id: u32) -> bool {
    let mut guard = PASTE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashSet::new).insert(pty_id)
}

fn in_flight_end(pty_id: u32) {
    let mut guard = PASTE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = guard.as_mut() {
        set.remove(&pty_id);
    }
}

/// 要传到远端的粘贴素材。
pub enum RemotePaste {
    /// 长文本:后台先转存成 `.txt` 再上传。上传失败 → 提示后**粘原文兜底**。
    Text(String),
    /// 已落盘的本机文件(剪贴板图片):直接上传。上传失败 → **只提示,什么都不粘**
    /// —— 图片没有「原文」可退,[`ALT_V`] 也没用(远端 agent 读的是远端剪贴板,
    /// 装机版 `pasteToTerminalInner` 图片分支的同一条结论)。
    File(PathBuf),
}

/// 远程 pane 的粘贴:后台转存 + SFTP 上传,完成后把**远端绝对路径**写进该 pane。
///
/// # 为什么必须异步
///
/// `TerminalView` 的粘贴钩子是**同步返回值制**([`mt_ui::PasteAction`] 的注释:
/// 宿主在钩子里回头 update 视图 = 同一实体嵌套 update,gpui 当场 panic),
/// 而上传要跨网络。于是钩子当场返回 `PasteAction::None`(语义就是「宿主已接管」),
/// 真正的写入由这条任务在完成后经 `AppStore::write_to_pane` 补上。
///
/// # 失败面(与原版逐条对齐)
///
/// - 上传失败 / 转存失败 → 弹一条 `paste-error` toast,长文本**再把原文粘进去**
///   (`notifyPasteFailure` 之后继续走 `enqueuePtyWrite(text)` 的那一支:
///   就是长了点,比什么都没有强);图片没有原文可退,只提示(见 [`RemotePaste`]);
/// - pane 在上传期间被关掉 → `write_to_pane` 返回 false,什么都不发生。
pub fn spawn_remote_paste(
    pty_id: u32,
    payload: RemotePaste,
    connection: mt_config::SshConnection,
    project_path: String,
    project_id: String,
    project_name: String,
    dest_dir: String,
    cx: &mut gpui::App,
) {
    if !in_flight_begin(pty_id) {
        return; // 上一次还没粘完,丢弃这次(原版同款)
    }
    let store = AppStore::global(cx);
    // 上传失败时的兜底原文 —— 只有长文本有
    let fallback = match &payload {
        RemotePaste::Text(text) => Some(text.clone()),
        RemotePaste::File(_) => None,
    };
    cx.spawn(async move |cx| {
        let uploaded = cx
            .background_executor()
            .spawn(async move {
                let local = match payload {
                    RemotePaste::Text(text) => save_clipboard_text(&text)?,
                    RemotePaste::File(path) => path,
                };
                let local = local.to_string_lossy().into_owned();
                crate::remote_ssh::upload_paste(&connection, &project_path, &local, &dest_dir)
            })
            .await;
        in_flight_end(pty_id);

        let _ = store.update(cx, |store, cx| {
            let Some((project, pane)) = store.pane_of_pty(pty_id) else {
                return; // pane 在上传期间被关掉了
            };
            match uploaded {
                Ok(remote_path) => {
                    store.write_to_pane(&project, &pane, &quote_path(&remote_path), cx);
                }
                Err(detail) => {
                    eprintln!("[pane {pty_id}] 粘贴内容上传到远端失败: {detail}");
                    toast::push_message(
                        ToastKind::PasteError,
                        project_id,
                        project_name,
                        tr!("terminal", "pasteUploadFailed", detail = detail),
                        cx,
                    );
                    // 提示完继续粘原文 —— 与原版一致。图片没有原文,到此为止。
                    if let Some(text) = fallback {
                        store.write_to_pane(&project, &pane, &text, cx);
                    }
                }
            }
        });
    })
    .detach();
}

/// 粘进终端的那一串:**带英文双引号**(兼容含空格的路径),
/// 不追加空格、不追加回车(`terminalCache.ts:757`)。
pub fn quote_path(path: &str) -> String {
    format!("\"{path}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字符阈值:`>=` 命中,0 = 该维度不判。
    #[test]
    fn 字符阈值按码元数且是闭区间() {
        assert!(is_long_text(&"a".repeat(2000), 0, 2000));
        assert!(!is_long_text(&"a".repeat(1999), 0, 2000));
        assert!(!is_long_text(&"a".repeat(99999), 0, 0), "0 = 不按字符判");
    }

    /// 行阈值:CRLF 归一后按 `\n` 切,行数 = 分段数(末尾无换行也算一行)。
    #[test]
    fn 行阈值归一_crlf_后计数() {
        // 9 个 \n → 10 行
        let ten = "x\r\n".repeat(9) + "x";
        assert!(is_long_text(&ten, 10, 0));
        let nine = "x\r\n".repeat(8) + "x";
        assert!(!is_long_text(&nine, 10, 0));
        // \r\n 不许被数成两次换行
        assert!(!is_long_text("a\r\nb", 3, 0));
        assert!(is_long_text("a\r\nb", 2, 0));
    }

    /// 任一阈值命中即转存(不是「都要满足」)。
    #[test]
    fn 任一阈值命中即为长文本() {
        // 只超字符数
        assert!(is_long_text(&"a".repeat(2000), 10, 2000));
        // 只超行数
        assert!(is_long_text(&"x\n".repeat(20), 10, 2000));
        // 都不超
        assert!(!is_long_text("hello", 10, 2000));
    }

    /// 两个阈值都是 0 = 整个功能哑掉(用户手动关掉两个维度)。
    #[test]
    fn 两个阈值都为零时永不命中() {
        assert!(!is_long_text(&"x\n".repeat(9999), 0, 0));
    }

    /// 字符数按 UTF-16 码元:emoji 算 2、中文算 1 —— 与 JS `String.length` 对齐。
    #[test]
    fn 字符数用_utf16_码元而非_char() {
        // 5 个 emoji = 10 个码元(chars().count() 只有 5)
        let emoji = "😀".repeat(5);
        assert_eq!(emoji.chars().count(), 5);
        assert!(is_long_text(&emoji, 0, 10));
        assert!(!is_long_text(&emoji, 0, 11));
        // 中文是 1 个码元
        assert!(is_long_text(&"中".repeat(10), 0, 10));
        assert!(!is_long_text(&"中".repeat(9), 0, 10));
    }

    /// 盘符路径 → `/mnt/<小写盘符>/...`,反斜杠一律转正斜杠。
    #[test]
    fn windows_路径转_wsl() {
        assert_eq!(
            windows_path_to_wsl(r"C:\Users\me\paste-1.txt").as_deref(),
            Some("/mnt/c/Users/me/paste-1.txt")
        );
        // verbatim 前缀要先剥掉
        assert_eq!(
            windows_path_to_wsl(r"\\?\D:\tmp\a.txt").as_deref(),
            Some("/mnt/d/tmp/a.txt")
        );
        // 正斜杠分隔同样认
        assert_eq!(
            windows_path_to_wsl("E:/tmp/a.txt").as_deref(),
            Some("/mnt/e/tmp/a.txt")
        );
    }

    /// 非盘符路径转不了 —— 返回 None,调用方原样粘。
    #[test]
    fn 非盘符路径不转换() {
        assert_eq!(windows_path_to_wsl(r"\\wsl$\Ubuntu\home\me\a.txt"), None);
        assert_eq!(windows_path_to_wsl("/home/me/a.txt"), None);
        assert_eq!(windows_path_to_wsl("relative/a.txt"), None);
        assert_eq!(windows_path_to_wsl("C:"), None, "缺分隔符");
        assert_eq!(windows_path_to_wsl(""), None);
    }

    /// shell 命令是不是 wsl:按 basename 比,不漏判全路径、不误判同前缀命令。
    #[test]
    fn wsl_shell_按_basename_判定() {
        assert!(command_is_wsl("wsl"));
        assert!(command_is_wsl("wsl.exe"));
        assert!(command_is_wsl(r"C:\Windows\System32\wsl.exe"));
        assert!(command_is_wsl(" WSL.EXE "), "大小写与空白都要吃掉");
        assert!(!command_is_wsl("wslconfig.exe"));
        assert!(!command_is_wsl("powershell.exe"));
        assert!(!command_is_wsl(""));
    }

    /// 路径映射:local 原样、wsl 转 /mnt、ssh 本批不转(走不到)。
    #[test]
    fn 路径按目标映射() {
        let p = Path::new(r"C:\tmp\paste-1.txt");
        assert_eq!(
            map_pasted_path(p, PasteTarget::Local),
            r"C:\tmp\paste-1.txt"
        );
        assert_eq!(
            map_pasted_path(p, PasteTarget::Wsl),
            "/mnt/c/tmp/paste-1.txt"
        );
        // 转不了的路径在 wsl 目标下原样返回
        let unc = Path::new(r"\\server\share\a.txt");
        assert_eq!(
            map_pasted_path(unc, PasteTarget::Wsl),
            r"\\server\share\a.txt"
        );
    }

    /// 粘进终端的是带双引号的路径,前后不加空格也不加回车。
    #[test]
    fn 粘贴串带双引号且不带回车() {
        assert_eq!(quote_path(r"C:\a b\c.txt"), "\"C:\\a b\\c.txt\"");
        assert!(!quote_path("x").contains('\r'));
        assert!(!quote_path("x").ends_with(' '));
    }

    /// 图片落盘的文件名同样与装机版一字不差:`clip-{毫秒}.{ext}`,与文本的
    /// `paste-*.txt` 同处一个目录 —— 24h 清理是「删目录里全部超时文件」,
    /// 两种前缀都盖得到。
    #[test]
    fn 图片临时文件名与装机版同格式() {
        let path = save_clipboard_image(b"\x89PNG\r\n\x1a\n", "png").expect("写临时文件");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("clip-"), "{name}");
        assert!(name.ends_with(".png"), "{name}");
        assert!(
            name["clip-".len()..name.len() - 4]
                .chars()
                .all(|c| c.is_ascii_digit()),
            "中间必须是纯毫秒数:{name}"
        );
        assert_eq!(path.parent(), Some(temp_dir().as_path()), "与文本同目录");
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&path);
    }

    /// gpui 那条路拿到的是**已编码的字节**,原样写盘不重编码 —— 重编码会白掉
    /// 一次解码/编码,还可能把无损 PNG 转劣。
    ///
    /// 测的是 [`gpui_image`] 而不是 [`read_clipboard_image`]:后者在 Windows 上会
    /// 先去读**真实的系统剪贴板**,测试机上有没有截图不可控。
    #[test]
    fn gpui_图片_entry_原样落盘() {
        let bytes = b"\xff\xd8\xff-not-really-a-jpeg".to_vec();
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes.clone());
        let item = gpui::ClipboardItem::new_image(&image);
        let path = gpui_image(Some(&item)).expect("有图片 entry").expect("落盘");
        assert_eq!(path.extension().unwrap(), "jpg", "按 entry 的格式定扩展名");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "字节原样,不重编码");
        let _ = std::fs::remove_file(&path);
    }

    /// 纯文本 / 空剪贴板都不算图片 —— 这两条决定了普通粘贴不会误走图片分支。
    #[test]
    fn 纯文本与空剪贴板都不算图片() {
        let text = gpui::ClipboardItem::new_string("hello".into());
        assert!(gpui_image(Some(&text)).is_none());
        assert!(gpui_image(None).is_none());
    }

    /// gpui 的每个图片格式都要有个像样的扩展名(拿它当文件名后缀)。
    #[test]
    fn gpui_图片格式都映射到扩展名() {
        use gpui::ImageFormat as F;
        for (format, ext) in [
            (F::Png, "png"),
            (F::Jpeg, "jpg"),
            (F::Webp, "webp"),
            (F::Gif, "gif"),
            (F::Svg, "svg"),
            (F::Bmp, "bmp"),
            (F::Tiff, "tiff"),
        ] {
            assert_eq!(image_ext(format), ext);
        }
    }

    /// 转存的文件名格式与装机版一字不差(两个版本共用同一个目录与清理逻辑)。
    #[test]
    fn 临时文件名与装机版同格式() {
        let dir = temp_dir();
        assert!(dir.ends_with(TEMP_DIR));
        let path = save_clipboard_text("hello").expect("写临时文件");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("paste-"), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
        assert!(
            name["paste-".len()..name.len() - 4]
                .chars()
                .all(|c| c.is_ascii_digit()),
            "中间必须是纯毫秒数:{name}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let _ = std::fs::remove_file(&path);
    }
}
