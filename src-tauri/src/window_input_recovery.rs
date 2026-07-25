#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
const VK_LBUTTON: i32 = 0x01;
#[cfg(windows)]
const WM_CANCELMODE: u32 = 0x001f;
#[cfg(windows)]
const GUI_MODAL_FLAGS: u32 = 0x02 | 0x04 | 0x08 | 0x10;
#[cfg(windows)]
const RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(windows)]
const GUI_SETTLE_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(windows)]
const WINDOW_IDENTITY_PROPERTY: [u16; 29] = [
    77, 105, 110, 105, 84, 101, 114, 109, 46, 87, 105, 110, 100, 111, 119, 73, 110, 112, 117, 116,
    82, 101, 99, 111, 118, 101, 114, 121, 0,
];

#[cfg(windows)]
static DEFERRED_RECOVERY_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowHandle(isize);

#[cfg(windows)]
impl WindowHandle {
    fn as_ptr(self) -> *mut std::ffi::c_void {
        self.0 as *mut std::ffi::c_void
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowIdentityMarker(isize);

#[cfg(windows)]
impl WindowIdentityMarker {
    fn as_ptr(self) -> *mut std::ffi::c_void {
        self.0 as *mut std::ffi::c_void
    }
}

#[cfg(windows)]
struct WindowIdentity {
    handle: WindowHandle,
    marker: Box<u8>,
}

#[cfg(windows)]
impl WindowIdentity {
    fn marker(&self) -> WindowIdentityMarker {
        WindowIdentityMarker(self.marker.as_ref() as *const u8 as isize)
    }
}

#[cfg(windows)]
#[derive(Default)]
struct GuiInputState {
    flags: u32,
    has_capture: bool,
    has_menu_owner: bool,
    has_move_size: bool,
}

#[cfg(windows)]
impl GuiInputState {
    fn needs_cancel(&self) -> bool {
        self.flags & GUI_MODAL_FLAGS != 0
            || self.has_capture
            || self.has_menu_owner
            || self.has_move_size
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct GuiThreadInfo {
    size: u32,
    flags: u32,
    active: *mut std::ffi::c_void,
    focus: *mut std::ffi::c_void,
    capture: *mut std::ffi::c_void,
    menu_owner: *mut std::ffi::c_void,
    move_size: *mut std::ffi::c_void,
    caret: *mut std::ffi::c_void,
    caret_rect: Rect,
}

#[cfg(windows)]
extern "system" {
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetGUIThreadInfo(thread_id: u32, info: *mut GuiThreadInfo) -> i32;
    fn GetWindowThreadProcessId(window: *mut std::ffi::c_void, process_id: *mut u32) -> u32;
    fn GetPropW(window: *mut std::ffi::c_void, property: *const u16) -> *mut std::ffi::c_void;
    fn PostMessageW(
        window: *mut std::ffi::c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> i32;
    fn ReleaseCapture() -> i32;
    fn RemovePropW(window: *mut std::ffi::c_void, property: *const u16) -> *mut std::ffi::c_void;
    fn SetPropW(
        window: *mut std::ffi::c_void,
        property: *const u16,
        data: *mut std::ffi::c_void,
    ) -> i32;
}

/// 窗口失焦后清理 Win32 的鼠标捕获和系统菜单模态状态。
///
/// 左键未按下时可立即清理。若失焦发生在拖动/缩放期间，立即清理会中断正常
/// 操作，因此改为后台等待左键松开，再检查 GUI 线程是否仍有模态输入状态。
/// 只有状态确实残留时才向顶层窗口投递 `WM_CANCELMODE`。
#[cfg(windows)]
pub fn recover_after_focus_loss(window: &tauri::Window) {
    let window_handle = match window.hwnd() {
        Ok(handle) => WindowHandle(handle.0 as isize),
        Err(_) => return,
    };

    if is_left_button_down() {
        schedule_recovery_after_button_release(window_handle);
    } else {
        post_cancel_if_modal_state_remains(window_handle);
        unsafe {
            // 当前回调运行在窗口事件线程；保留原有的即时释放兜底。
            ReleaseCapture();
        }
    }
}

#[cfg(not(windows))]
pub fn recover_after_focus_loss(_window: &tauri::Window) {}

#[cfg(windows)]
fn schedule_recovery_after_button_release(window_handle: WindowHandle) {
    if DEFERRED_RECOVERY_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let Some(window_identity) = register_window_identity(window_handle) else {
        DEFERRED_RECOVERY_PENDING.store(false, Ordering::Release);
        eprintln!("[window-input] SetPropW(window identity) failed");
        return;
    };

    std::thread::spawn(move || {
        wait_for_left_button_release(is_left_button_down, || {
            std::thread::sleep(RELEASE_POLL_INTERVAL)
        });

        // 让窗口线程先处理已经排队的 WM_LBUTTONUP；正常拖动/缩放应在此期间
        // 自然退出。若用户已经开始下一次操作，则不取消新的模态状态。
        std::thread::sleep(GUI_SETTLE_INTERVAL);
        if !is_left_button_down()
            && is_registered_window(&window_identity)
            && post_cancel_if_modal_state_remains(window_identity.handle)
        {
            // 验证第一次消息已生效；仍残留时再投递一次，避免单次消息竞争。
            std::thread::sleep(GUI_SETTLE_INTERVAL);
            if !is_left_button_down() && is_registered_window(&window_identity) {
                post_cancel_if_modal_state_remains(window_identity.handle);
            }
        }

        unregister_window_identity(&window_identity);
        DEFERRED_RECOVERY_PENDING.store(false, Ordering::Release);
    });
}

#[cfg(windows)]
fn is_left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000) != 0 }
}

#[cfg(windows)]
fn register_window_identity(window_handle: WindowHandle) -> Option<WindowIdentity> {
    let identity = WindowIdentity {
        handle: window_handle,
        marker: Box::new(0),
    };
    let registered = unsafe {
        SetPropW(
            window_handle.as_ptr(),
            WINDOW_IDENTITY_PROPERTY.as_ptr(),
            identity.marker().as_ptr(),
        )
    } != 0;
    registered.then_some(identity)
}

#[cfg(windows)]
fn is_registered_window(identity: &WindowIdentity) -> bool {
    let marker = unsafe { GetPropW(identity.handle.as_ptr(), WINDOW_IDENTITY_PROPERTY.as_ptr()) };
    let marker = WindowIdentityMarker(marker as isize);
    marker_matches(identity.marker(), marker)
}

#[cfg(windows)]
fn unregister_window_identity(identity: &WindowIdentity) {
    if is_registered_window(identity) {
        unsafe {
            RemovePropW(identity.handle.as_ptr(), WINDOW_IDENTITY_PROPERTY.as_ptr());
        }
    }
}

#[cfg(windows)]
fn marker_matches(expected: WindowIdentityMarker, actual: WindowIdentityMarker) -> bool {
    expected.0 != 0 && actual == expected
}

#[cfg(windows)]
fn post_cancel_if_modal_state_remains(window_handle: WindowHandle) -> bool {
    let Some(thread_id) = window_thread_id(window_handle) else {
        return false;
    };

    let needs_cancel = query_gui_input_state(thread_id)
        .map(|state| state.needs_cancel())
        .unwrap_or(true);
    if !needs_cancel {
        return false;
    }

    let posted = unsafe { PostMessageW(window_handle.as_ptr(), WM_CANCELMODE, 0, 0) } != 0;
    if !posted {
        eprintln!("[window-input] PostMessageW(WM_CANCELMODE) failed");
    }
    posted
}

#[cfg(windows)]
fn window_thread_id(window_handle: WindowHandle) -> Option<u32> {
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(window_handle.as_ptr(), &mut process_id) };

    // 延迟期间窗口可能已经销毁且句柄被复用；只操作仍属于当前进程的窗口。
    (thread_id != 0 && process_id == std::process::id()).then_some(thread_id)
}

#[cfg(windows)]
fn query_gui_input_state(thread_id: u32) -> Option<GuiInputState> {
    let mut info = GuiThreadInfo {
        size: std::mem::size_of::<GuiThreadInfo>() as u32,
        ..GuiThreadInfo::default()
    };
    if unsafe { GetGUIThreadInfo(thread_id, &mut info) } == 0 {
        return None;
    }

    Some(GuiInputState {
        flags: info.flags,
        has_capture: !info.capture.is_null(),
        has_menu_owner: !info.menu_owner.is_null(),
        has_move_size: !info.move_size.is_null(),
    })
}

#[cfg(windows)]
fn wait_for_left_button_release(
    mut is_left_button_down: impl FnMut() -> bool,
    mut wait_once: impl FnMut(),
) {
    while is_left_button_down() {
        wait_once();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{
        marker_matches, wait_for_left_button_release, GuiInputState, WindowIdentityMarker,
    };
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[test]
    fn focus_loss_while_left_button_is_down_waits_until_release() {
        let button_states = RefCell::new(VecDeque::from([true, true, false]));
        let events = RefCell::new(Vec::new());

        wait_for_left_button_release(
            || button_states.borrow_mut().pop_front().unwrap_or(false),
            || events.borrow_mut().push("wait"),
        );

        assert_eq!(*events.borrow(), ["wait", "wait"]);
    }

    #[test]
    fn focus_loss_after_left_button_release_does_not_wait() {
        let events = RefCell::new(Vec::new());

        wait_for_left_button_release(|| false, || events.borrow_mut().push("wait"));

        assert!(events.borrow().is_empty());
    }

    #[test]
    fn live_system_menu_lock_state_requires_cancel() {
        let state = GuiInputState {
            flags: 0x04 | 0x08,
            has_capture: false,
            has_menu_owner: false,
            has_move_size: false,
        };

        assert!(state.needs_cancel());
    }

    #[test]
    fn captured_input_without_modal_flags_requires_cancel() {
        let state = GuiInputState {
            has_capture: true,
            ..GuiInputState::default()
        };

        assert!(state.needs_cancel());
    }

    #[test]
    fn reused_window_handle_with_a_different_marker_is_rejected() {
        assert!(marker_matches(
            WindowIdentityMarker(10),
            WindowIdentityMarker(10)
        ));
        assert!(!marker_matches(
            WindowIdentityMarker(10),
            WindowIdentityMarker(11)
        ));
        assert!(!marker_matches(
            WindowIdentityMarker(10),
            WindowIdentityMarker(0)
        ));
    }

    #[test]
    fn clear_gui_input_state_does_not_require_cancel() {
        assert!(!GuiInputState::default().needs_cancel());
    }
}
