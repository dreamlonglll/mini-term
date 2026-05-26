#[tauri::command]
pub fn set_window_dark_mode(window: tauri::Window, dark: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::{BOOL, HWND};
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

        let hwnd = match window.hwnd() {
            Ok(h) => HWND(h.0 as *mut _),
            Err(e) => return Err(format!("hwnd unavailable: {e}")),
        };
        let value: BOOL = if dark { BOOL(1) } else { BOOL(0) };
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &value as *const _ as *const _,
                std::mem::size_of::<BOOL>() as u32,
            )
        };
        if let Err(e) = result {
            eprintln!(
                "[window_theme] DwmSetWindowAttribute failed: hresult={:?} dark={}",
                e, dark
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, dark);
    }
    Ok(())
}
