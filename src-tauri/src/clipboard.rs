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

        let header_size = header.biSize as usize;
        let pixel_offset = if bit_count <= 8 {
            let palette_entries = if header.biClrUsed > 0 {
                header.biClrUsed as usize
            } else {
                1usize << bit_count
            };
            header_size + palette_entries * 4
        } else {
            header_size
        };

        if pixel_offset >= size {
            return Err("像素数据偏移超出范围".into());
        }

        let pixels = ptr.add(pixel_offset);
        let stride = ((width * bit_count as u32 + 31) / 32 * 4) as usize;
        let bottom_up = header.biHeight > 0;

        let mut img = RgbaImage::new(width, height);

        for y in 0..height {
            let src_y = if bottom_up { height - 1 - y } else { y };
            let row = pixels.add(src_y as usize * stride);

            for x in 0..width {
                let (r, g, b, a) = match bit_count {
                    32 => {
                        let off = (x * 4) as usize;
                        (
                            *row.add(off + 2),
                            *row.add(off + 1),
                            *row.add(off),
                            *row.add(off + 3),
                        )
                    }
                    24 => {
                        let off = (x * 3) as usize;
                        (*row.add(off + 2), *row.add(off + 1), *row.add(off), 255)
                    }
                    _ => return Err(format!("不支持的位深: {bit_count}")),
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
