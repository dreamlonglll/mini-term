//! 弹窗毛玻璃背板(快照式)。
//!
//! # 为什么是快照
//!
//! 原版所有 Modal 的遮罩是 `bg-black/50 backdrop-blur-sm`(`Modal.tsx:171`),
//! 而 gpui 0.2 **没有元素级 backdrop blur**:模糊只存在于阴影图元
//! (`scene.rs` 的 `Shadow::blur_radius`)与「窗口级毛玻璃」(透出的是桌面,
//! 不是窗口内容),也没有渲染到纹理的通道可以自己实现实时模糊。
//!
//! 代偿方案:弹窗**打开的第一帧**里调 `PrintWindow(PW_RENDERFULLCONTENT)` 抓
//! DWM 上一帧已合成画面 —— 那一帧还没有弹窗 —— 1/4 降采样(HALFTONE 均值)
//! + 一遍小半径盒模糊,升采样交给 GPU 双线性,观感等价 4~8px 高斯,与原版
//! `blur-sm`(4px)同档。弹窗期间背后内容基本静止,快照的「静态」几乎不可
//! 感知;终端仍在滚动输出时背板不跟着动,属已知取舍。
//!
//! # 时序为什么成立
//!
//! `PW_RENDERFULLCONTENT` 取的是 DWM **最后一次呈现**的表面。弹窗打开会
//! notify → 下一帧重绘,我们在那次 render 里捕获 —— 此时"最后呈现帧"还是
//! 没有弹窗的画面,天然不含弹窗自身。之后的帧沿用缓存(`Workspace::frost`),
//! 关窗清空;若在弹窗开着时再抓一次就会把弹窗抓进去,所以**只在 0→1 转换抓**。
//!
//! # 消费方
//!
//! - Dialog 族(`Root::render_dialog_layer` 之前垫一层,压暗仍由
//!   `cx.theme().overlay` 的 black/50 承担,见 `theme.rs::apply`);
//! - 用量统计浮层(自绘 Modal,`main.rs` 的 usage_layer):频道相同,
//!   频道内自己叠 black/50。
//!
//! 捕获失败(非 Windows / GDI 出错)返回 `None`,退回纯 black/50 遮罩。

#[cfg(windows)]
pub fn capture(window: &gpui::Window) -> Option<std::sync::Arc<gpui::RenderImage>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, HALFTONE, HBITMAP, HDC, SRCCOPY,
        SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
    };
    // PrintWindow 在 windows crate 里归档在打印/XPS 模块,不在 WindowsAndMessaging
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

    // gpui 的 `Window` 有同名固有方法(返回 AnyWindowHandle),必须显式走 trait
    // 才能拿到平台句柄(与 `notify::flash_taskbar` 同一句注释)。
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return None;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut c_void);

    /// 顶朝下(负高)的 32bpp DIB;返回位图句柄与像素区指针(BGRA)。
    unsafe fn make_dib(dc: HDC, w: i32, h: i32) -> Option<(HBITMAP, *mut u8)> {
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp =
            unsafe { CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(bmp.into());
            }
            return None;
        }
        Some((bmp, bits as *mut u8))
    }

    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return None;
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        // PrintWindow 给的是**整窗** rect(含 DWM 不可见边框),而消费方铺的是
        // 客户区(inset_0)—— 不裁的话边框那几像素被拉进画面,顶边糊出一条白线。
        // 这里算出客户区在窗口内的偏移,降采样时只取客户区那一块。
        let mut crect = RECT::default();
        if GetClientRect(hwnd, &mut crect).is_err() {
            return None;
        }
        let (cw, ch) = (crect.right - crect.left, crect.bottom - crect.top);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        let mut origin = POINT::default();
        let _ = ClientToScreen(hwnd, &mut origin);
        let (off_x, off_y) = (origin.x - rect.left, origin.y - rect.top);

        // 全尺寸抓帧
        let full_dc = CreateCompatibleDC(None);
        let Some((full_bmp, _full_bits)) = make_dib(full_dc, w, h) else {
            let _ = DeleteDC(full_dc);
            return None;
        };
        let old_full = SelectObject(full_dc, full_bmp.into());
        // PW_RENDERFULLCONTENT(=2):windows crate 0.61 没铺这个常量,数值来自
        // WinUser.h;没有它 DirectComposition 呈现的窗口抓出来是黑的。
        let ok = PrintWindow(hwnd, full_dc, PRINT_WINDOW_FLAGS(2)).as_bool();

        // 降采样到 1/4,**两步级联**(全尺寸→1/2→1/4,只取客户区那一块):
        // 单步 4 倍 HALFTONE 对终端字符列这种高频栅格会打出摩尔纹 ——
        // 模糊完还留着一排排竖条(用户实测);每步 2 倍是干净的 2×2 均值,
        // 级联等于规整的 4×4 盒滤波,竖纹在源头就被抹平。
        let (hw, hh) = ((cw / 2).max(1), (ch / 2).max(1));
        let (sw, sh) = ((cw / 4).max(1), (ch / 4).max(1));
        let half_dc = CreateCompatibleDC(None);
        let small_dc = CreateCompatibleDC(None);
        let half = make_dib(half_dc, hw, hh);
        let small = make_dib(small_dc, sw, sh);
        let mut out: Option<Vec<u8>> = None;
        if ok && let (Some((half_bmp, _)), Some((small_bmp, small_bits))) = (half, small) {
            let old_half = SelectObject(half_dc, half_bmp.into());
            let old_small = SelectObject(small_dc, small_bmp.into());
            SetStretchBltMode(half_dc, HALFTONE);
            let _ = SetBrushOrgEx(half_dc, 0, 0, None);
            SetStretchBltMode(small_dc, HALFTONE);
            let _ = SetBrushOrgEx(small_dc, 0, 0, None);
            let step1 = StretchBlt(
                half_dc, 0, 0, hw, hh, Some(full_dc), off_x, off_y, cw, ch, SRCCOPY,
            )
            .as_bool();
            let step2 = step1
                && StretchBlt(small_dc, 0, 0, sw, sh, Some(half_dc), 0, 0, hw, hh, SRCCOPY)
                    .as_bool();
            if step2 {
                let _ = GdiFlush();
                let len = (sw * sh * 4) as usize;
                out = Some(std::slice::from_raw_parts(small_bits, len).to_vec());
            }
            SelectObject(half_dc, old_half);
            SelectObject(small_dc, old_small);
            let _ = DeleteObject(half_bmp.into());
            let _ = DeleteObject(small_bmp.into());
        } else {
            if let Some((half_bmp, _)) = half {
                let _ = DeleteObject(half_bmp.into());
            }
            if let Some((small_bmp, _)) = small {
                let _ = DeleteObject(small_bmp.into());
            }
        }
        SelectObject(full_dc, old_full);
        let _ = DeleteObject(full_bmp.into());
        let _ = DeleteDC(full_dc);
        let _ = DeleteDC(half_dc);
        let _ = DeleteDC(small_dc);

        let mut buf = out?;
        // 半径 3 跑两遍(两遍盒 ≈ 高斯,在 1/4 图上等效全尺寸 ~12px)——
        // 残余的字符列纹理周期只有 2~4px,一遍小半径压不干净。
        box_blur_bgra(&mut buf, sw as usize, sh as usize, 3, 2);
        // GDI 不维护 alpha 通道(常见留 0),RenderImage 是预乘语义,置 255。
        for px in buf.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }

        // RenderImage 的帧就是 BGRA 字节序(assets.rs 文档明示),GDI 给的
        // 恰好是 BGRA —— `RgbaImage` 在这里只是字节容器,不做通道解释。
        let img = image::RgbaImage::from_raw(sw as u32, sh as u32, buf)?;
        let frame = image::Frame::new(img);
        Some(std::sync::Arc::new(gpui::RenderImage::new(vec![frame])))
    }
}

#[cfg(not(windows))]
pub fn capture(_window: &gpui::Window) -> Option<std::sync::Arc<gpui::RenderImage>> {
    None
}

/// 就地盒模糊(BGRA,横竖各一遍为一趟,可多趟 —— 两趟以上近似高斯)。
/// 滑窗 O(n):窗口越界按**边缘复制**补齐(除数恒为 2r+1,边上不发暗);
/// alpha 通道不参与(调用方随后统一置 255)。
#[cfg(windows)]
fn box_blur_bgra(buf: &mut [u8], w: usize, h: usize, radius: usize, passes: usize) {
    if w == 0 || h == 0 || radius == 0 || passes == 0 {
        return;
    }
    let mut tmp = vec![0u8; buf.len()];
    for _ in 0..passes {
        // 横向 buf → tmp:行内步长 1
        blur_line(buf, &mut tmp, w, h, radius, |x, y| (y * w + x) * 4);
        // 纵向 tmp → buf:把 (主轴, 副轴) 映射转置即可
        blur_line(&tmp, buf, h, w, radius, |y, x| (y * w + x) * 4);
    }
}

/// 对每条「长度 len 的主轴线 × lanes 条」做一维滑窗均值。`index(主轴, 副轴)`
/// 给出字节偏移,横竖两个方向共用同一份实现。
#[cfg(windows)]
fn blur_line(
    src: &[u8],
    dst: &mut [u8],
    len: usize,
    lanes: usize,
    radius: usize,
    index: impl Fn(usize, usize) -> usize,
) {
    let count = (2 * radius + 1) as u32;
    let clamp = |i: isize| i.clamp(0, len as isize - 1) as usize;
    for lane in 0..lanes {
        let mut acc = [0u32; 3];
        for i in -(radius as isize)..=(radius as isize) {
            let p = index(clamp(i), lane);
            acc[0] += src[p] as u32;
            acc[1] += src[p + 1] as u32;
            acc[2] += src[p + 2] as u32;
        }
        for x in 0..len {
            let q = index(x, lane);
            dst[q] = (acc[0] / count) as u8;
            dst[q + 1] = (acc[1] / count) as u8;
            dst[q + 2] = (acc[2] / count) as u8;
            let add = index(clamp(x as isize + radius as isize + 1), lane);
            let sub = index(clamp(x as isize - radius as isize), lane);
            for c in 0..3 {
                acc[c] += src[add + c] as u32;
                acc[c] -= src[sub + c] as u32;
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn 盒模糊_纯色不变_边缘复制均值正确() {
        // 2x2 纯色:边缘复制下除数恒为 2r+1,纯色是均值的不动点(边上不发暗)
        let mut buf = vec![10, 20, 30, 0, 10, 20, 30, 0, 10, 20, 30, 0, 10, 20, 30, 0];
        box_blur_bgra(&mut buf, 2, 2, 1, 2);
        for px in buf.chunks_exact(4) {
            assert_eq!(&px[..3], &[10, 20, 30]);
        }

        // 2x1 黑白,半径 1 一趟:窗口按边缘复制补齐 ——
        // x=0 取 [黑,黑,白] = 85,x=1 取 [黑,白,白] = 170
        let mut buf = vec![0, 0, 0, 0, 255, 255, 255, 0];
        box_blur_bgra(&mut buf, 2, 1, 1, 1);
        assert_eq!(&buf[..3], &[85, 85, 85]);
        assert_eq!(&buf[4..7], &[170, 170, 170]);
    }

    #[test]
    fn 盒模糊_空图与零半径零趟不崩() {
        let mut empty: Vec<u8> = vec![];
        box_blur_bgra(&mut empty, 0, 0, 2, 2);
        let mut one = vec![1, 2, 3, 4];
        box_blur_bgra(&mut one, 1, 1, 0, 2);
        assert_eq!(one, vec![1, 2, 3, 4]);
        let mut one = vec![1, 2, 3, 4];
        box_blur_bgra(&mut one, 1, 1, 2, 0);
        assert_eq!(one, vec![1, 2, 3, 4]);
    }
}
