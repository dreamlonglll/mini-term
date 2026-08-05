//! Win11「贴靠布局」(Snap Layouts) 支持 —— 无边框窗口下让系统重新认得最大化按钮。
//!
//! 去掉原生标题栏后，最大化按钮只是一个普通 DOM 元素，系统无从知道它在哪，
//! 悬停不再弹出贴靠布局菜单。让系统认回它的唯一途径，是在 `WM_NCHITTEST` 中
//! 对该矩形返回 `HTMAXBUTTON`。
//!
//! 代价是这块区域随即变成「非客户区」：鼠标消息不再送进 WebView，CSS `:hover`
//! 和 React `onClick` 双双失效。所以悬停态与点击都得在这里补回去 ——
//! 悬停经 `titlebar-max-hover` 事件回传前端绘制，点击直接投 `WM_SYSCOMMAND`
//! 切最大化，不绕前端一圈。
//!
//! 前端未上报按钮矩形（或本模块未安装成功）时，命中测试完全不介入，
//! 按钮退回普通 DOM 元素由 React `onClick` 处理 —— 非 Windows 平台走的正是这条路径。

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::OnceLock;
    use tauri::{Emitter, Manager};

    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_NCHITTEST: u32 = 0x0084;
    const WM_NCMOUSEMOVE: u32 = 0x00A0;
    const WM_NCMOUSELEAVE: u32 = 0x02A2;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const WM_NCLBUTTONUP: u32 = 0x00A2;
    const WM_SYSCOMMAND: u32 = 0x0112;

    const HTCLIENT: isize = 1;
    const HTMAXBUTTON: isize = 9;

    const SC_MAXIMIZE: usize = 0xF030;
    const SC_RESTORE: usize = 0xF120;

    const TME_LEAVE: u32 = 0x0000_0002;
    const TME_NONCLIENT: u32 = 0x0000_0010;

    /// 子类化 ID：同一窗口只装一次，重复安装会叠加同一个 proc。
    const SUBCLASS_ID: usize = 0x4D54_5342; // "MTSB"

    const HOVER_EVENT: &str = "titlebar-max-hover";

    /// 前端上报的最大化按钮矩形（CSS 像素，相对 WebView 左上角）。
    /// 未上报时 `RECT_SET` 为 false，命中测试直接放行。
    static RECT_SET: AtomicBool = AtomicBool::new(false);
    static RECT_LEFT: AtomicI32 = AtomicI32::new(0);
    static RECT_TOP: AtomicI32 = AtomicI32::new(0);
    static RECT_RIGHT: AtomicI32 = AtomicI32::new(0);
    static RECT_BOTTOM: AtomicI32 = AtomicI32::new(0);

    static HOVERING: AtomicBool = AtomicBool::new(false);
    static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct TrackMouseEvent {
        cb_size: u32,
        flags: u32,
        track: *mut std::ffi::c_void,
        hover_time: u32,
    }

    type SubclassProc = unsafe extern "system" fn(
        hwnd: *mut std::ffi::c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
        id: usize,
        ref_data: usize,
    ) -> isize;

    #[link(name = "comctl32")]
    extern "system" {
        fn SetWindowSubclass(
            hwnd: *mut std::ffi::c_void,
            proc: SubclassProc,
            id: usize,
            ref_data: usize,
        ) -> i32;
        fn DefSubclassProc(
            hwnd: *mut std::ffi::c_void,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> isize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn ScreenToClient(hwnd: *mut std::ffi::c_void, point: *mut Point) -> i32;
        fn GetDpiForWindow(hwnd: *mut std::ffi::c_void) -> u32;
        fn IsZoomed(hwnd: *mut std::ffi::c_void) -> i32;
        fn PostMessageW(
            hwnd: *mut std::ffi::c_void,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> i32;
        fn TrackMouseEvent(event: *mut TrackMouseEvent) -> i32;
    }

    /// 安装子类化窗口过程。窗口创建后调用一次即可。
    pub fn install(window: &tauri::WebviewWindow) {
        let hwnd = match window.hwnd() {
            Ok(handle) => handle.0 as *mut std::ffi::c_void,
            Err(e) => {
                eprintln!("[window-snap] hwnd 不可用，贴靠布局未启用: {e}");
                return;
            }
        };
        let _ = APP.set(window.app_handle().clone());

        let installed = unsafe { SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0) } != 0;
        if !installed {
            eprintln!("[window-snap] SetWindowSubclass 失败，贴靠布局未启用");
        }
    }

    /// 前端上报最大化按钮矩形（CSS 像素）。宽高非正 = 撤销上报（按钮不可见时）。
    pub fn set_max_button_rect(x: f64, y: f64, width: f64, height: f64) {
        if width <= 0.0 || height <= 0.0 {
            RECT_SET.store(false, Ordering::Release);
            set_hovering(false);
            return;
        }
        // 先写四条边再置位，保证命中测试读到的永远是完整矩形而非半旧半新
        RECT_LEFT.store(x.floor() as i32, Ordering::Relaxed);
        RECT_TOP.store(y.floor() as i32, Ordering::Relaxed);
        RECT_RIGHT.store((x + width).ceil() as i32, Ordering::Relaxed);
        RECT_BOTTOM.store((y + height).ceil() as i32, Ordering::Relaxed);
        RECT_SET.store(true, Ordering::Release);
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: *mut std::ffi::c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
        _id: usize,
        _ref_data: usize,
    ) -> isize {
        match msg {
            WM_NCHITTEST => {
                // 先问原 proc：resize 边框优先于按钮，否则贴着窗口上沿的
                // 那几像素会被按钮吃掉，纵向 resize 从最大化按钮上方就拉不动了
                let hit = DefSubclassProc(hwnd, msg, wparam, lparam);
                if hit == HTCLIENT && point_in_max_button(hwnd, lparam) {
                    return HTMAXBUTTON;
                }
                hit
            }
            WM_NCMOUSEMOVE => {
                let over = wparam as isize == HTMAXBUTTON;
                set_hovering(over);
                if over {
                    // 非客户区的 leave 通知要显式订阅，否则鼠标移开后悬停态不灭
                    track_nc_mouse_leave(hwnd);
                    return 0;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCMOUSELEAVE => {
                set_hovering(false);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            // 鼠标进了真正的客户区：非客户区的 leave 偶尔不到（快速划过），补一刀
            WM_MOUSEMOVE => {
                set_hovering(false);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            // 按下不交给默认处理：默认会进系统菜单的模态循环，把 WebView 的输入卡住
            WM_NCLBUTTONDOWN if wparam as isize == HTMAXBUTTON => 0,
            WM_NCLBUTTONUP if wparam as isize == HTMAXBUTTON => {
                set_hovering(false);
                let command = if IsZoomed(hwnd) != 0 {
                    SC_RESTORE
                } else {
                    SC_MAXIMIZE
                };
                PostMessageW(hwnd, WM_SYSCOMMAND, command, 0);
                0
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }

    /// lparam 里是屏幕坐标（多显示器下可能为负，必须按有符号取）。
    unsafe fn point_in_max_button(hwnd: *mut std::ffi::c_void, lparam: isize) -> bool {
        if !RECT_SET.load(Ordering::Acquire) {
            return false;
        }

        let mut point = Point {
            x: signed_low_word(lparam),
            y: signed_high_word(lparam),
        };
        if ScreenToClient(hwnd, &mut point) == 0 {
            return false;
        }

        // 上报的是 CSS 像素，命中测试拿到的是物理像素，按窗口当前 DPI 换算
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 };
        let scaled = |v: i32| (v as f64 * scale).round() as i32;

        point.x >= scaled(RECT_LEFT.load(Ordering::Relaxed))
            && point.x < scaled(RECT_RIGHT.load(Ordering::Relaxed))
            && point.y >= scaled(RECT_TOP.load(Ordering::Relaxed))
            && point.y < scaled(RECT_BOTTOM.load(Ordering::Relaxed))
    }

    unsafe fn track_nc_mouse_leave(hwnd: *mut std::ffi::c_void) {
        let mut event = TrackMouseEvent {
            cb_size: std::mem::size_of::<TrackMouseEvent>() as u32,
            flags: TME_LEAVE | TME_NONCLIENT,
            track: hwnd,
            hover_time: 0,
        };
        TrackMouseEvent(&mut event);
    }

    /// 悬停态变化才发事件 —— 命中测试每次鼠标移动都跑，不去重会淹了前端。
    fn set_hovering(hovering: bool) {
        if HOVERING.swap(hovering, Ordering::AcqRel) == hovering {
            return;
        }
        if let Some(app) = APP.get() {
            let _ = app.emit(HOVER_EVENT, hovering);
        }
    }

    fn signed_low_word(value: isize) -> i32 {
        (value & 0xFFFF) as u16 as i16 as i32
    }

    fn signed_high_word(value: isize) -> i32 {
        ((value >> 16) & 0xFFFF) as u16 as i16 as i32
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn negative_screen_coordinates_survive_word_extraction() {
            // 副屏在主屏左侧时鼠标坐标为负，按无符号取会变成 65000 这类巨值
            let lparam = ((-13i32 as u16 as isize) & 0xFFFF) | (((-7i32 as u16 as isize) & 0xFFFF) << 16);
            assert_eq!(signed_low_word(lparam), -13);
            assert_eq!(signed_high_word(lparam), -7);
        }

        #[test]
        fn zero_sized_rect_disables_hit_testing() {
            set_max_button_rect(10.0, 0.0, 46.0, 32.0);
            assert!(RECT_SET.load(Ordering::Acquire));
            set_max_button_rect(10.0, 0.0, 0.0, 32.0);
            assert!(!RECT_SET.load(Ordering::Acquire));
        }
    }
}

#[cfg(windows)]
pub fn install(window: &tauri::WebviewWindow) {
    imp::install(window);
}

#[cfg(not(windows))]
pub fn install(_window: &tauri::WebviewWindow) {}

/// 前端上报最大化按钮位置，供 `WM_NCHITTEST` 把该矩形认作系统最大化按钮。
/// 非 Windows 平台是空实现——那里按钮就是普通 DOM 元素，走 React onClick。
#[tauri::command]
pub fn set_max_button_rect(x: f64, y: f64, width: f64, height: f64) {
    #[cfg(windows)]
    imp::set_max_button_rect(x, y, width, height);
    #[cfg(not(windows))]
    let _ = (x, y, width, height);
}
