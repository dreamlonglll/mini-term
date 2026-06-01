/// 通过 Win32 剪贴板 API 读取非标准格式的图片数据，保存为 temp PNG 文件。
/// 用于兜底 Tauri 插件 readImage 无法识别的截图工具（如 PinPix）。

#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, GetDIBits, GetObjectW, SelectObject, BITMAP,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    use image::{ImageBuffer, RgbaImage};

    const CF_BITMAP: u32 = 2;
    const CF_DIB: u32 = 8;

    /// 尝试从剪贴板读取图片（CF_DIB → CF_BITMAP），保存为 PNG 到 temp 目录。
    pub fn read_clipboard_to_png() -> Result<PathBuf, String> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return Err("无法打开剪贴板".into());
            }
            let result = read_inner();
            let _ = CloseClipboard();
            result
        }
    }

    unsafe fn read_inner() -> Result<PathBuf, String> {
        if IsClipboardFormatAvailable(CF_DIB).is_ok() {
            if let Ok(img) = read_dib() {
                return save_png(&img);
            }
        }
        if IsClipboardFormatAvailable(CF_BITMAP).is_ok() {
            if let Ok(img) = read_bitmap() {
                return save_png(&img);
            }
        }
        Err("剪贴板中没有可识别的图片数据".into())
    }

    unsafe fn read_dib() -> Result<RgbaImage, String> {
        let handle =
            GetClipboardData(CF_DIB).map_err(|e| format!("GetClipboardData(CF_DIB): {e}"))?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u8;
        if ptr.is_null() {
            return Err("GlobalLock 失败".into());
        }
        let size = GlobalSize(hglobal);
        let result = parse_dib(ptr, size);
        let _ = GlobalUnlock(hglobal);
        result
    }

    unsafe fn parse_dib(ptr: *const u8, size: usize) -> Result<RgbaImage, String> {
        if size < std::mem::size_of::<BITMAPINFOHEADER>() {
            return Err("DIB 数据太短".into());
        }

        let header = &*(ptr as *const BITMAPINFOHEADER);
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
        const MAX_DIM: u32 = 1 << 16; // 65536,远超任何真实截图
        if width > MAX_DIM || height > MAX_DIM {
            return Err("DIB 尺寸超出上限".into());
        }

        let pixel_offset = header.biSize as usize; // 24/32 位无调色板,像素紧跟头部
        if pixel_offset >= size {
            return Err("像素数据偏移超出范围".into());
        }

        // 关键加固:全程 usize + checked 运算,并校验整块像素数据落在缓冲区内,
        // 杜绝声称维度远大于实际分配时的越界读 / 整数溢出。
        let stride = (((width as usize) * (bit_count as usize) + 31) / 32) * 4;
        let pixel_bytes = (height as usize)
            .checked_mul(stride)
            .ok_or("DIB 像素数据长度溢出")?;
        let required = pixel_offset
            .checked_add(pixel_bytes)
            .ok_or("DIB 像素数据长度溢出")?;
        if required > size {
            return Err("DIB 像素数据超出缓冲区范围".into());
        }

        let pixels = ptr.add(pixel_offset);
        let bottom_up = header.biHeight > 0;

        let mut img = RgbaImage::new(width, height);

        for y in 0..height {
            let src_y = if bottom_up { height - 1 - y } else { y };
            let row = pixels.add(src_y as usize * stride);

            for x in 0..width {
                let (r, g, b, a) = if bit_count == 32 {
                    let off = (x as usize) * 4;
                    (
                        *row.add(off + 2),
                        *row.add(off + 1),
                        *row.add(off),
                        *row.add(off + 3),
                    )
                } else {
                    // 24 位
                    let off = (x as usize) * 3;
                    (*row.add(off + 2), *row.add(off + 1), *row.add(off), 255)
                };
                img.put_pixel(x, y, image::Rgba([r, g, b, a]));
            }
        }

        Ok(img)
    }

    unsafe fn read_bitmap() -> Result<RgbaImage, String> {
        let handle =
            GetClipboardData(CF_BITMAP).map_err(|e| format!("GetClipboardData(CF_BITMAP): {e}"))?;
        let hbitmap = HBITMAP(handle.0);

        let mut bmp = BITMAP::default();
        let ret = GetObjectW(
            hbitmap,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        if ret == 0 {
            return Err("GetObjectW 失败".into());
        }

        let width = bmp.bmWidth as u32;
        let height = bmp.bmHeight as u32;

        let hdc = CreateCompatibleDC(None);
        let old = SelectObject(hdc, hbitmap);

        let mut bi = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..std::mem::zeroed()
        };

        let mut buf = vec![0u8; (width * height * 4) as usize];

        let ret = GetDIBits(
            hdc,
            hbitmap,
            0,
            height,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi as *mut _ as *mut _,
            DIB_RGB_COLORS,
        );

        SelectObject(hdc, old);
        let _ = DeleteDC(hdc);

        if ret == 0 {
            return Err("GetDIBits 失败".into());
        }

        // BGRA → RGBA
        for chunk in buf.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }

        ImageBuffer::from_raw(width, height, buf).ok_or_else(|| "构建图像缓冲区失败".into())
    }

    fn save_png(img: &RgbaImage) -> Result<PathBuf, String> {
        let dir = std::env::temp_dir().join("mini-term-clipboard");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;

        let path = dir.join(format!(
            "clip-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));

        img.save(&path).map_err(|e| format!("保存 PNG 失败: {e}"))?;
        Ok(path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const HDR: usize = std::mem::size_of::<BITMAPINFOHEADER>();

        fn make_header(width: i32, height: i32, bit_count: u16) -> BITMAPINFOHEADER {
            let mut h: BITMAPINFOHEADER = unsafe { std::mem::zeroed() };
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
            let stride = (((w as usize) * 32 + 31) / 32) * 4;
            let buf = buf_with_header(&header, HDR + stride * (h as usize));
            unsafe {
                let img = parse_dib(buf.as_ptr(), buf.len()).expect("合法小图应解析成功");
                assert_eq!((img.width(), img.height()), (2, 2));
            }
        }
    }
}

/// 清理 temp 目录中超过 24 小时的剪贴板截图文件，启动时调用一次。
pub fn cleanup_old_clipboard_images() {
    let dir = std::env::temp_dir().join("mini-term-clipboard");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
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

#[tauri::command]
pub fn read_clipboard_image() -> Result<String, String> {
    #[cfg(windows)]
    {
        let path = win::read_clipboard_to_png()?;
        Ok(path.to_string_lossy().into_owned())
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows 平台".into())
    }
}

/// 将长文本剪贴板内容保存为 temp 目录下的 .txt 文件，返回绝对路径。
/// 与图片粘贴共用 `mini-term-clipboard` 目录，清理逻辑自动覆盖。
#[tauri::command]
pub fn save_clipboard_text(text: String) -> Result<String, String> {
    let dir = std::env::temp_dir().join("mini-term-clipboard");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;

    let path = dir.join(format!(
        "paste-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));

    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("写入临时文件失败: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}
