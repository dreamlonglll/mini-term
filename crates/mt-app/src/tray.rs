//! 系统托盘状态灯(audit #21 / T 批)。对照 `src-tauri/src/tray.rs` + `src/store.ts`
//! 的 `syncTrayStatus` / `collectAiProjects`。
//!
//! # 颜色语义(与主窗口 StatusDot 按**语义**对齐,非逐色复刻)
//!
//! ```text
//! 黄 = 有 pane 需要用户确认(授权/输入请求,含 error 异常)
//! 蓝 = 有 pane 处理中(ai-working,含 API 重试)
//! 绿 = 有已完成且未读的回答(激活主窗口即清除)
//! 灰 = 全部安静(静止不闪)
//! ```
//!
//! 静止时停在最高优先级色(黄>蓝>绿)。闪烁三档:**聚焦不闪**;失焦多状态 =
//! 同一灯位颜色轮转;失焦单状态 = 状态变化后短促亮暗呼吸 [`BURST_FRAMES`] 帧再
//! 定格全亮(原注释:「持续呼吸闪太抢注意力(用户反馈)」)。
//!
//! # 与装机版的分工翻转
//!
//! 装机版的 Rust 侧**零业务逻辑** —— 裁剪/排序/emoji/i18n/tooltip 拼接/去重签名
//! 全在前端 TS 里,`set_tray_status` 只是个搬运工。GPUI 侧没有「前端」,这些整个
//! 搬进本模块([`menu_entries`] / [`tooltip`] / [`build_snapshot`]);装机版那套
//! `seq` 乱序裁决**整个删掉** —— 快照是主线程同步算出来的,不存在乱序覆盖。
//!
//! # 线程模型
//!
//! ```text
//! GPUI 主线程                          托盘线程(mt-tray)
//! ───────────                          ────────────────
//! store 变化 → build_snapshot          隐藏消息窗口 + GetMessage 循环
//!            → Tray::push(去重)  ──┐   ├─ WM_TRAY_SYNC  : 取快照 → 重画图标/tooltip
//!                                  └──►├─ WM_TIMER 600ms: 走一帧闪烁
//!                                      ├─ WM_TRAY_CB    : 左键/右键
//!  Workspace::on_tray_event  ◄─────────┘                  └→ 唤主窗 + 事件回主线程
//!  (futures channel + 前台任务)
//! ```
//!
//! 托盘回调**不碰任何 Entity**:与 `ai.rs` 同一套路数(后台线程只管往 channel
//! 里丢,主线程上的前台任务醒来后再改 store)。唤起主窗口那一步是纯 Win32
//! (`ShowWindow`/`SetForegroundWindow`),在托盘线程里做 —— 点击托盘图标时
//! 前台锁正好允许本进程抢前台,换到主线程就错过这个窗口了。
//!
//! # 为什么不用 `tray-icon` crate
//!
//! 它会拉进 muda + 一套全局事件循环钩子,与 gpui 自己的 Windows 消息循环共存有
//! 风险(装机版是靠 tauri 的集成才避开的)。这里 Win32 直写 `Shell_NotifyIconW`,
//! 代价是 **HICON 生命周期得自己管**(见 [`OwnedIcon`],换一次图标销毁一个旧句柄)。

use futures::channel::mpsc::{self, UnboundedReceiver};

use crate::i18n::{t, tr};
use crate::store::{AiProjectKind, AiProjects};

// ─── 常量(逐条对齐 src-tauri/src/tray.rs:31-45) ──────────────

/// 闪烁帧间隔(ms)。
const BLINK_MS: u32 = 600;
/// 单状态时状态变化后的短促闪烁帧数(约 3.6s),之后定格全亮 ——
/// 持续呼吸闪太抢注意力(用户反馈),只在「有新变化」时闪一阵提醒。
const BURST_FRAMES: usize = 6;
/// 暗帧的 alpha 系数。
const DIM: f32 = 0.35;
/// 圆点半径与画布边长之比。装机版是 36px 画布 / 13px 半径的字面量,
/// Windows 侧画布跟着 `SM_CXSMICON` 走(16px@100%),只能改成比例式。
const RADIUS_RATIO: f32 = 13.0 / 36.0;

// Apple 系统色板(装机版是 macOS 菜单栏优先设计,颜色照搬)
const GRAY: [u8; 3] = [0x8E, 0x8E, 0x93];
const BLUE: [u8; 3] = [0x0A, 0x84, 0xFF];
const YELLOW: [u8; 3] = [0xFF, 0xCC, 0x00];
const GREEN: [u8; 3] = [0x34, 0xC7, 0x59];

/// 托盘菜单里的档位 emoji(`store.ts:330` 的 `KIND_EMOJI`)。
pub fn kind_emoji(kind: AiProjectKind) -> &'static str {
    match kind {
        AiProjectKind::Attention => "🟡",
        AiProjectKind::Working => "🔵",
        AiProjectKind::Done => "🟢",
        AiProjectKind::Idle => "⚪",
    }
}

// ─── 主线程算出来、推给托盘线程的一份全量快照 ─────────────────

/// 托盘右键菜单里的一条(label 含 emoji 灯色与 i18n 状态文案)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrayEntry {
    pub id: String,
    pub label: String,
}

/// 三盏灯的亮灭。`false/false/false` = 灰(安静)。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Lamps {
    pub attention: bool,
    pub working: bool,
    pub done: bool,
}

/// 一次推送的全部内容。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct TraySnapshot {
    /// 设置页「状态栏图标」开关。false = 图标隐藏且全部逻辑早退。
    pub enabled: bool,
    /// 主窗口是否聚焦(聚焦不闪)。
    pub focused: bool,
    pub lamps: Lamps,
    /// 空串 = 不设 tooltip。
    pub tooltip: String,
    pub projects: Vec<TrayEntry>,
    /// 去重签名(`store.ts:336-338`):`enabled|focused|attention|working|done|labels`。
    /// **不含 tooltip** —— 它是三个计数的等价导出,计数没变 tooltip 必然没变。
    pub signature: String,
}

/// 托盘线程送回主线程的交互。
pub enum TrayEvent {
    /// 左键点了图标。窗口**已经被唤起**(不看开关),这里只负责
    /// `trayClickFocus` 门控下的「跳到待办 pane」。
    Clicked,
    /// 右键菜单点了某个项目。**不受 `trayClickFocus` 管辖**。
    ProjectClicked(String),
}

// ─── 纯函数:菜单 / tooltip / 快照 ────────────────────────────

/// 菜单条目:取前 `max` 个,label = `emoji + 空格 + 项目名 + " · " + 状态文案`
/// (`store.ts:331-334` 逐字)。**超出上限的直接不显示,没有省略提示**。
pub fn menu_entries(projects: &AiProjects, max: usize) -> Vec<TrayEntry> {
    projects
        .entries
        .iter()
        .take(max)
        .map(|entry| TrayEntry {
            id: entry.id.clone(),
            label: format!(
                "{} {} · {}",
                kind_emoji(entry.kind),
                entry.name,
                t("app", entry.kind.tray_status_key())
            ),
        })
        .collect()
}

/// tooltip:三个 **pane 级**计数(`ai-idle` 不计入),0 的那档整条不出现,
/// 用 ` · ` 连接(`store.ts:339-348`)。
pub fn tooltip(attention: usize, working: usize, done: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if attention > 0 {
        parts.push(tr!("app", "trayAttention", count = attention));
    }
    if working > 0 {
        parts.push(tr!("app", "trayWorking", count = working));
    }
    if done > 0 {
        parts.push(tr!("app", "trayDone", count = done));
    }
    parts.join(" · ")
}

/// 把一份 [`AiProjects`](crate::store::AiProjects) 聚合结果压成推送快照。
///
/// `max_projects` 是 `config.trayMaxProjects ?? 5`(UI 限幅 1..20,这里不再钳 ——
/// 手改配置成 0 就是「菜单空着」,与 TS 的 `slice(0, 0)` 同结果)。
pub fn build_snapshot(
    enabled: bool,
    focused: bool,
    projects: &AiProjects,
    max_projects: usize,
) -> TraySnapshot {
    let entries = menu_entries(projects, max_projects);
    let signature = format!(
        "{}|{}|{}|{}|{}|{}",
        enabled,
        focused,
        projects.attention,
        projects.working,
        projects.done,
        entries
            .iter()
            .map(|e| e.label.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    TraySnapshot {
        enabled,
        focused,
        lamps: Lamps {
            attention: projects.attention > 0,
            working: projects.working > 0,
            done: projects.done > 0,
        },
        tooltip: tooltip(projects.attention, projects.working, projects.done),
        projects: entries,
        signature,
    }
}

// ─── 纯函数:灯色与画帧 ──────────────────────────────────────

/// 当前活跃的颜色集合(顺序固定 黄→蓝→绿;灰不在集合里,空 = 灰)。
#[cfg_attr(not(windows), allow(dead_code))]
fn active_colors(lamps: Lamps) -> Vec<[u8; 3]> {
    let mut colors = Vec::new();
    if lamps.attention {
        colors.push(YELLOW);
    }
    if lamps.working {
        colors.push(BLUE);
    }
    if lamps.done {
        colors.push(GREEN);
    }
    colors
}

/// 本帧该显示的颜色与明暗。
///
/// 静止(聚焦 / 已定格 / 安静)= 最高优先级色全亮;
/// 失焦单状态 = 亮暗呼吸;失焦多状态 = 颜色轮转(全亮)。
#[cfg_attr(not(windows), allow(dead_code))]
fn frame_color(colors: &[[u8; 3]], frame: usize, blinking: bool) -> ([u8; 3], bool) {
    match colors.len() {
        0 => (GRAY, false),
        1 => (colors[0], blinking && frame % 2 == 1),
        n => {
            if blinking {
                (colors[frame % n], false)
            } else {
                (colors[0], false) // 静止时停在最高优先级色
            }
        }
    }
}

/// 画单个圆点(托盘永远只占一个灯位;多状态靠交替变色表达)。
///
/// 实心圆 + 1px 软边抗锯齿,无边框无描边;返回 **RGBA**(Win32 那一层再转
/// 预乘 BGRA)。`dim` = 暗帧(alpha 压到 [`DIM`],用于单状态呼吸闪烁)。
#[cfg_attr(not(windows), allow(dead_code))]
fn compose_frame_rgba(size: u32, color: [u8; 3], dim: bool) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let radius = size as f32 * RADIUS_RATIO;
    let center = size as f32 / 2.0 - 0.5;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            // 1px 软边抗锯齿
            let mut alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if dim {
                alpha *= DIM;
            }
            let alpha = (alpha * 255.0) as u8;
            if alpha > 0 {
                let idx = ((y * size + x) * 4) as usize;
                rgba[idx] = color[0];
                rgba[idx + 1] = color[1];
                rgba[idx + 2] = color[2];
                rgba[idx + 3] = alpha;
            }
        }
    }
    rgba
}

/// 闪烁相位。装机版把它散在 `TrayLightState` 的两个字段 + 线程循环里,
/// 这里收成一个可单测的小状态机(`tray.rs:224-242` 的同一条判据链)。
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Blink {
    frame: usize,
    /// 单状态短促闪烁结束后已定格全亮,不再重绘。
    settled: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Blink {
    /// 灯色/焦点变化 → 新状态从「全亮」帧开始,并重新允许短促闪烁。
    fn reset(&mut self) {
        self.frame = 0;
        self.settled = false;
    }

    /// 走一帧。返回**是否需要重绘**。
    ///
    /// `colors` = 活跃颜色数;安静 / 聚焦 / 已定格 / 开关关掉都不推帧。
    fn tick(&mut self, colors: usize, enabled: bool, focused: bool) -> bool {
        if !enabled || colors == 0 || focused || self.settled {
            return false;
        }
        if colors == 1 && self.frame >= BURST_FRAMES {
            // 短促闪烁结束:补一帧全亮定格,之后跳过
            self.settled = true;
        } else {
            self.frame = self.frame.wrapping_add(1);
        }
        true
    }

    /// 现在处于「该闪」的状态吗(聚焦或已定格都算静止)。
    fn blinking(&self, focused: bool) -> bool {
        !focused && !self.settled
    }
}

// ─── 对外句柄 ────────────────────────────────────────────────

/// 托盘句柄。**主线程持有**,drop 时把图标摘掉并收掉托盘线程。
pub struct Tray {
    handle: Option<platform::TrayHandle>,
    /// 上一次真正推下去的签名(去重,`store.ts` 的 `lastTraySig`)。
    last_signature: String,
}

impl Tray {
    /// 建托盘。返回句柄 + 交互事件的接收端。
    ///
    /// 建不起来(非 Windows / 取不到 HWND / 注册窗口失败)时句柄是空壳、
    /// 接收端立刻结束 —— 与装机版「初始化失败只 eprintln 不中断启动」同语义。
    pub fn start(window: &gpui::Window) -> (Self, UnboundedReceiver<TrayEvent>) {
        let (tx, rx) = mpsc::unbounded();
        let handle = main_window_handle(window).and_then(|hwnd| platform::start(hwnd, tx));
        if handle.is_none() {
            eprintln!("[tray] 托盘未启用(平台不支持或初始化失败)");
        }
        (
            Self {
                handle,
                last_signature: String::new(),
            },
            rx,
        )
    }

    /// 推一份快照。签名相同直接丢弃 —— store 的每一次 notify 都会走到这里,
    /// 不去重的话光是移动鼠标就会疯狂重建图标。
    pub fn push(&mut self, snapshot: TraySnapshot) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        if snapshot.signature == self.last_signature {
            return;
        }
        self.last_signature = snapshot.signature.clone();
        handle.push(snapshot);
    }
}

/// 主窗口的 HWND(给托盘线程唤窗用)。
///
/// **必须显式走 `HasWindowHandle` trait**:gpui 的 `Window` 上有一个同名的固有
/// 方法(返回 `AnyWindowHandle`),写成 `window.window_handle()` 会静默拿错东西
/// —— 与 `notify::flash_taskbar` 同一个坑,同一条注释。
#[cfg(windows)]
fn main_window_handle(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(win32.hwnd.get())
}

#[cfg(not(windows))]
fn main_window_handle(_window: &gpui::Window) -> Option<isize> {
    None
}

// ─── 平台实现 ────────────────────────────────────────────────

/// 非 Windows:空实现(与 `notify::play_sound` 同款分层)。
/// macOS 的 `NSStatusItem` / Linux 的 StatusNotifierItem 都是另一套 API,
/// 接口留在这里,补的时候只换这个模块。
#[cfg(not(windows))]
mod platform {
    use super::{TrayEvent, TraySnapshot};
    use futures::channel::mpsc::UnboundedSender;

    pub struct TrayHandle;

    pub fn start(_main_hwnd: isize, _events: UnboundedSender<TrayEvent>) -> Option<TrayHandle> {
        None
    }

    impl TrayHandle {
        pub fn push(&self, _snapshot: TraySnapshot) {}
    }
}

#[cfg(windows)]
mod platform {
    //! Win32 直写 `Shell_NotifyIconW`。
    //!
    //! 托盘图标的回调消息只能送到**窗口**,而 gpui 的主窗口 WndProc 归 gpui 管
    //! (子类化进去等于往它的消息处理里插一脚)。这里另起一个线程 + 一个不可见的
    //! 顶层窗口专门收托盘消息:与主线程零共享,只靠 channel + `PostMessage` 通信。
    //!
    //! **为什么不是 message-only 窗口(`HWND_MESSAGE`)**:它收不到广播,而
    //! explorer.exe 重启后重新登记图标靠的正是 `TaskbarCreated` 广播消息。
    //! 于是用一个零尺寸、never-shown、带 `WS_EX_TOOLWINDOW` 的顶层窗口
    //! (不会出现在 Alt+Tab / 任务栏里)。

    use std::ffi::c_void;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use futures::channel::mpsc::UnboundedSender;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject, HBITMAP,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
        GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON, ICONINFO, IsIconic, KillTimer,
        MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
        SM_CXSMICON, SW_RESTORE, SW_SHOW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
        ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
        WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
        WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };
    use windows::core::{PCWSTR, w};

    use super::{
        BLINK_MS, Blink, Lamps, TrayEntry, TrayEvent, TraySnapshot, active_colors,
        compose_frame_rgba, frame_color,
    };

    /// 托盘图标的回调消息(shell → 我们的窗口)。
    const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
    /// 主线程往 channel 里塞了新快照的叫醒信号。
    const WM_TRAY_SYNC: u32 = WM_APP + 2;
    /// `NOTIFYICONDATAW::uID`(同一个 hWnd 下唯一即可)。
    const TRAY_UID: u32 = 1;
    /// 闪烁定时器 id。
    const TIMER_ID: usize = 1;
    /// 菜单项命令 id 的起点(`TrackPopupMenu` 返回 0 表示「没选」,不能从 0 开始)。
    const MENU_ID_BASE: usize = 1000;
    /// 取不到 `SM_CXSMICON` 时的兜底边长。
    const FALLBACK_ICON_SIZE: i32 = 16;

    enum Command {
        Sync(Box<TraySnapshot>),
        Quit,
    }

    /// HICON 的 RAII 包装 —— **换图标必须销毁旧句柄**。
    ///
    /// 装机版靠 tray-icon crate 代管,这里自己写就得自己管:600ms 一帧的闪烁下
    /// 漏一个句柄就是每分钟漏 100 个 GDI 对象,几小时就能顶到进程 10000 上限。
    struct OwnedIcon(HICON);

    impl Drop for OwnedIcon {
        fn drop(&mut self) {
            // SAFETY: 句柄由 CreateIconIndirect 造出,本结构独占,只销毁一次
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }

    /// HBITMAP 的 RAII 包装(造 HICON 的中间产物,`CreateIconIndirect` 会拷贝它们)。
    struct OwnedBitmap(HBITMAP);

    impl Drop for OwnedBitmap {
        fn drop(&mut self) {
            // SAFETY: 句柄由 CreateDIBSection / CreateBitmap 造出,本结构独占
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }

    /// 主线程握着的那一端。
    pub struct TrayHandle {
        tx: Sender<Command>,
        /// 托盘线程那个隐藏窗口的 HWND(`isize` 是为了 `Send`)。
        hwnd: isize,
        thread: Option<JoinHandle<()>>,
    }

    impl TrayHandle {
        pub fn push(&self, snapshot: TraySnapshot) {
            if self.tx.send(Command::Sync(Box::new(snapshot))).is_ok() {
                self.wake();
            }
        }

        fn wake(&self) {
            // SAFETY: PostMessageW 跨线程投递是设计用法;窗口已死时返回错误,忽略
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut c_void)),
                    WM_TRAY_SYNC,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            let _ = self.tx.send(Command::Quit);
            self.wake();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub fn start(main_hwnd: isize, events: UnboundedSender<TrayEvent>) -> Option<TrayHandle> {
        let (tx, rx) = channel::<Command>();
        let (ready_tx, ready_rx) = channel::<isize>();
        let thread = std::thread::Builder::new()
            .name("mt-tray".into())
            .spawn(move || run(main_hwnd, rx, events, ready_tx))
            .ok()?;
        // 建窗口是同步的本地调用,几毫秒就回来;给 5s 只是别在异常环境下吊死
        let hwnd = ready_rx.recv_timeout(Duration::from_secs(5)).ok()?;
        if hwnd == 0 {
            let _ = thread.join();
            return None;
        }
        Some(TrayHandle {
            tx,
            hwnd,
            thread: Some(thread),
        })
    }

    /// 托盘线程主体:建窗口 → 起定时器 → 跑消息循环。
    fn run(
        main_hwnd: isize,
        rx: Receiver<Command>,
        events: UnboundedSender<TrayEvent>,
        ready: Sender<isize>,
    ) {
        let Some(hwnd) = create_window() else {
            let _ = ready.send(0);
            return;
        };

        // SAFETY: taskbar_created 只是注册一个消息号,失败返回 0(下面显式跳过 0)
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        // SAFETY: 取系统度量,无副作用
        let icon_size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let icon_size = if icon_size > 0 {
            icon_size
        } else {
            FALLBACK_ICON_SIZE
        };

        let state = Box::new(TrayThread {
            rx,
            events,
            main_hwnd: HWND(main_hwnd as *mut c_void),
            taskbar_created,
            icon_size,
            icon: None,
            added: false,
            // 装机版的初值:启动即认为聚焦(主窗口马上显示并获焦),避免开局按
            // 失焦语义闪烁;首次推送会带来真实值
            enabled: true,
            focused: true,
            lamps: Lamps::default(),
            blink: Blink::default(),
            tooltip: String::new(),
            projects: Vec::new(),
            menu_open: false,
            quit_pending: false,
        });
        // SAFETY: 指针存进本窗口的 USERDATA,只有本线程的 wndproc 会取用;
        // WM_DESTROY 里取回并 Box::from_raw 释放
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
            let _ = SetTimer(Some(hwnd), TIMER_ID, BLINK_MS, None);
        }

        if ready.send(hwnd.0 as isize).is_err() {
            // 主线程等超时后已经放弃了这个托盘 —— 别留一个没人能叫停的窗口
            // 和定时器在这儿转(DestroyWindow 会同步走一遍 WM_DESTROY 收摊)。
            // SAFETY: 窗口刚由本线程创建
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }

        let mut msg = MSG::default();
        // SAFETY: 标准消息循环;GetMessageW 返回 -1 是错误,`> 0` 一并挡掉
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn create_window() -> Option<HWND> {
        const CLASS_NAME: PCWSTR = w!("MiniTermTrayWindow");
        // SAFETY: 全是标准窗口创建调用;类重复注册返回 0,不视为错误
        unsafe {
            let hinstance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("mini-term tray"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance),
                None,
            )
            .ok()
        }
    }

    /// 托盘线程的全部状态。**只在托盘线程上碰**。
    struct TrayThread {
        rx: Receiver<Command>,
        events: UnboundedSender<TrayEvent>,
        main_hwnd: HWND,
        /// explorer 重启广播的消息号;0 = 注册失败(那就不做重登记)。
        taskbar_created: u32,
        icon_size: i32,
        /// 当前挂在托盘上的图标。换图时**先 NIM_MODIFY 再 drop 旧的**。
        icon: Option<OwnedIcon>,
        /// 图标是否已经登记进 shell。
        added: bool,
        enabled: bool,
        focused: bool,
        lamps: Lamps,
        blink: Blink,
        tooltip: String,
        projects: Vec<TrayEntry>,
        /// `TrackPopupMenu` 的模态循环正在栈上跑。
        menu_open: bool,
        /// 收到过 [`Command::Quit`],还没兑现(菜单开着时要等它收了)。
        quit_pending: bool,
    }

    impl TrayThread {
        /// 取空 channel 里的全部命令。返回 true = **现在**可以拆窗口了。
        fn drain(&mut self, hwnd: HWND) -> bool {
            while let Ok(cmd) = self.rx.try_recv() {
                match cmd {
                    Command::Sync(snapshot) => self.apply(hwnd, *snapshot),
                    Command::Quit => self.quit_pending = true,
                }
            }
            // 菜单模态循环里**不许**拆窗口:`TrackPopupMenu` 还在栈上,拆掉等于
            // 把它脚下这个 TrayThread 抽走(WM_DESTROY 会 free 掉它)。
            // 菜单收了之后 `show_menu` 会自己再发一次同步信号把这一步补上。
            self.quit_pending && !self.menu_open
        }

        /// 应用一份快照:可见性 + 图标 + tooltip + 菜单数据。
        fn apply(&mut self, hwnd: HWND, snapshot: TraySnapshot) {
            // 只有灯色/焦点真的变化才重置闪烁相位 —— tooltip/菜单等无关变化不
            // 打断多状态轮转,也不重启单状态的短促闪烁(装机版 tray.rs:289-303)
            let lamps_changed = self.lamps != snapshot.lamps || self.focused != snapshot.focused;
            self.lamps = snapshot.lamps;
            self.enabled = snapshot.enabled;
            self.focused = snapshot.focused;
            self.tooltip = snapshot.tooltip;
            self.projects = snapshot.projects;
            if lamps_changed {
                self.blink.reset();
            }
            self.refresh(hwnd);
        }

        /// 按当前状态重画图标 + 写 tooltip(开关关掉则摘图标)。
        fn refresh(&mut self, hwnd: HWND) {
            if !self.enabled {
                self.remove_icon(hwnd);
                return;
            }
            let colors = active_colors(self.lamps);
            let blinking = self.blink.blinking(self.focused);
            let (color, dim) = frame_color(&colors, self.blink.frame, blinking);
            let icon = make_icon(self.icon_size, color, dim);

            let mut data = self.base_data(hwnd);
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = WM_TRAY_CALLBACK;
            data.hIcon = icon.as_ref().map(|i| i.0).unwrap_or_default();
            write_tip(&mut data.szTip, &self.tooltip);

            let message = if self.added { NIM_MODIFY } else { NIM_ADD };
            // SAFETY: data 是栈上完整初始化的结构,hIcon 在本次调用期间有效
            let ok = unsafe { Shell_NotifyIconW(message, &data) }.as_bool();
            // 失败(shell 那边已经没有这一项了)就退回未登记,下一次按 NIM_ADD 重来
            self.added = ok;
            // shell 在 NIM_ADD/NIM_MODIFY 里拷贝了图标,这一刻才轮到旧句柄退场。
            // 赋值语句先写入新值、再 drop 被覆盖的旧值 —— 顺序正是我们要的。
            self.icon = icon;
        }

        fn remove_icon(&mut self, hwnd: HWND) {
            if self.added {
                let data = self.base_data(hwnd);
                // SAFETY: 同上
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                }
                self.added = false;
            }
            self.icon = None; // DestroyIcon
        }

        fn base_data(&self, hwnd: HWND) -> NOTIFYICONDATAW {
            NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_UID,
                ..Default::default()
            }
        }

        fn on_timer(&mut self, hwnd: HWND) {
            let colors = active_colors(self.lamps).len();
            if self.blink.tick(colors, self.enabled, self.focused) {
                self.refresh(hwnd);
            }
        }

        /// 托盘图标上的鼠标事件。`lparam` 是原始鼠标消息号(经典回调协议)。
        fn on_callback(&mut self, hwnd: HWND, mouse_message: u32) {
            match mouse_message {
                // 左键:**无条件**唤起主窗口(不看 trayClickFocus 开关),
                // 跳不跳 pane 由主线程按开关决定
                WM_LBUTTONUP => {
                    focus_main_window(self.main_hwnd);
                    let _ = self.events.unbounded_send(TrayEvent::Clicked);
                }
                WM_RBUTTONUP => self.show_menu(hwnd),
                _ => {}
            }
        }

        /// 右键菜单:**只列项目**(无「显示窗口」/「退出」/分隔符),
        /// 项目为空时压根不弹(装机版的 `set_menu(None)`)。
        fn show_menu(&mut self, hwnd: HWND) {
            if self.projects.is_empty() {
                return;
            }
            // `TrackPopupMenu` 是**模态**的:它自带的消息循环会把 WM_TRAY_SYNC
            // 派回 wndproc,期间 `self.projects` 完全可能被换成另一批。菜单是
            // 弹出那一刻的快照,选中项必须按**当时**那份来解读 —— 所以先把要用
            // 的东西全拷成局部量,菜单收了之后一概不再读 self 的这两个字段。
            let labels: Vec<String> = self
                .projects
                .iter()
                .map(|entry| escape_menu_label(&entry.label))
                .collect();
            let ids: Vec<String> = self.projects.iter().map(|entry| entry.id.clone()).collect();
            let events = self.events.clone();
            let main_hwnd = self.main_hwnd;

            let mut pt = POINT::default();
            // SAFETY: 下面整段是 TrackPopupMenu 的标准用法(含 MSDN 记载的
            // SetForegroundWindow 前置与 WM_NULL 后置,少了菜单点外面不消失)
            let command = unsafe {
                if GetCursorPos(&mut pt).is_err() {
                    return;
                }
                let Ok(menu) = CreatePopupMenu() else {
                    return;
                };
                for (index, label) in labels.iter().enumerate() {
                    let text = windows::core::HSTRING::from(label.as_str());
                    let _ =
                        AppendMenuW(menu, MF_STRING, MENU_ID_BASE + index, PCWSTR(text.as_ptr()));
                }
                let _ = SetForegroundWindow(hwnd);
                // 菜单开着期间不许拆窗口(见 [`TrayThread::drain`])
                self.menu_open = true;
                let command = TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                    pt.x,
                    pt.y,
                    None,
                    hwnd,
                    None,
                )
                .0;
                self.menu_open = false;
                let _ = DestroyMenu(menu);
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                if self.quit_pending {
                    // 模态循环里压下来的退出:现在补一记同步信号去兑现它
                    let _ = PostMessageW(Some(hwnd), WM_TRAY_SYNC, WPARAM(0), LPARAM(0));
                }
                command
            };

            if command >= MENU_ID_BASE as i32
                && let Some(id) = ids.get(command as usize - MENU_ID_BASE)
            {
                focus_main_window(main_hwnd);
                let _ = events.unbounded_send(TrayEvent::ProjectClicked(id.clone()));
            }
        }

        /// explorer.exe 重启后图标没了 —— 按 NIM_ADD 重新登记。
        fn readd(&mut self, hwnd: HWND) {
            self.added = false;
            self.refresh(hwnd);
        }

        fn teardown(&mut self, hwnd: HWND) {
            // SAFETY: 定时器由本窗口持有
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
            }
            self.remove_icon(hwnd);
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: USERDATA 里要么是 0(还没装上/已释放),要么是 run() 存进去的
        // 那个 Box::into_raw 指针,且只有本线程会取用。
        //
        // ⚠️ 每个分支**各借各的**,不在函数头上 `let state = &mut *ptr` 一次借到底:
        // WM_TRAY_SYNC 收到 Quit 后要 `DestroyWindow`,而那一调用会**同步**递归回
        // 本函数走 WM_DESTROY 把 state 释放掉 —— 外层若还攥着一个 `&mut`,它就是
        // 悬垂引用。
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayThread;
            if ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }

            match msg {
                WM_TRAY_SYNC => {
                    let quit = (*ptr).drain(hwnd);
                    if quit {
                        // 这一行之后 ptr 已失效(WM_DESTROY 在调用里跑完了)
                        let _ = DestroyWindow(hwnd);
                    }
                    LRESULT(0)
                }
                WM_TIMER if wparam.0 == TIMER_ID => {
                    (*ptr).on_timer(hwnd);
                    LRESULT(0)
                }
                WM_TRAY_CALLBACK => {
                    (*ptr).on_callback(hwnd, lparam.0 as u32);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    (*ptr).teardown(hwnd);
                    // 先摘掉指针再释放:teardown 之后到 free 之间若还有消息进来
                    // (菜单/定时器都可能),它看到的是 null 而不是已释放的内存
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                other if other != 0 && other == (*ptr).taskbar_created => {
                    (*ptr).readd(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// 唤起主窗口(装机版的 show → unminimize → set_focus)。
    ///
    /// 在**托盘线程**里做:点托盘图标的这一刻 shell 给了本进程抢前台的许可,
    /// 绕一圈回主线程再调 `SetForegroundWindow` 就只会闪任务栏图标了。
    fn focus_main_window(main_hwnd: HWND) {
        // SAFETY: main_hwnd 来自 gpui 主窗口,窗口已关时这些调用只是返回 false
        unsafe {
            let _ = ShowWindow(main_hwnd, SW_SHOW);
            if IsIconic(main_hwnd).as_bool() {
                let _ = ShowWindow(main_hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(main_hwnd);
        }
    }

    /// 把 RGBA 圆点变成一个 32bpp 带 alpha 的 HICON。
    ///
    /// shell 走 `AlphaBlend` 画托盘图标,颜色位图必须是**预乘** BGRA;
    /// 掩码位图(1bpp)对 32bpp 图标基本只是形式要求,但不能省、也不能不清零
    /// (`CreateBitmap` 的初始内容是未定义的)。
    fn make_icon(size: i32, color: [u8; 3], dim: bool) -> Option<OwnedIcon> {
        let rgba = compose_frame_rgba(size as u32, color, dim);
        let pixels = (size * size) as usize;

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                // 负高 = top-down,行序与 compose_frame_rgba 一致
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // SAFETY: 下面整段只操作自己刚造出来的位图与它的像素缓冲
        unsafe {
            let mut bits: *mut c_void = std::ptr::null_mut();
            let color_bitmap = OwnedBitmap(
                CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?,
            );
            if bits.is_null() {
                return None;
            }
            let dst = std::slice::from_raw_parts_mut(bits as *mut u8, pixels * 4);
            for i in 0..pixels {
                let (r, g, b, a) = (
                    rgba[i * 4] as u32,
                    rgba[i * 4 + 1] as u32,
                    rgba[i * 4 + 2] as u32,
                    rgba[i * 4 + 3] as u32,
                );
                let premul = |c: u32| ((c * a + 127) / 255) as u8;
                dst[i * 4] = premul(b);
                dst[i * 4 + 1] = premul(g);
                dst[i * 4 + 2] = premul(r);
                dst[i * 4 + 3] = a as u8;
            }

            // 1bpp 掩码:全 0 = 处处「显示颜色位图」,行按 4 字节对齐
            let mask_stride = (((size + 31) / 32) * 4) as usize;
            let mask_bits = vec![0u8; mask_stride * size as usize];
            let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
            if mask.is_invalid() {
                return None;
            }
            let mask_bitmap = OwnedBitmap(mask);

            let info = ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask_bitmap.0,
                hbmColor: color_bitmap.0,
            };
            // CreateIconIndirect 会拷贝两张位图,出了这个作用域它们即可释放
            let icon = CreateIconIndirect(&info).ok()?;
            Some(OwnedIcon(icon))
        }
    }

    /// 写 `szTip`(128 个 UTF-16 码元,含结尾 NUL)。超长按码元截断 ——
    /// tooltip 是三个计数拼的短句,正常永远碰不到这个上限。
    fn write_tip(buf: &mut [u16; 128], text: &str) {
        buf.fill(0);
        for (slot, ch) in buf.iter_mut().take(127).zip(text.encode_utf16()) {
            *slot = ch;
        }
    }

    /// Win32 菜单把 `&` 当助记符前缀(`A&B` 会画成 `A_B`),项目名里真出现
    /// `&` 时要转义成 `&&`。**只在这一层做** —— 上游的 label 必须与装机版逐字
    /// 相同(它进的是签名与单测)。
    fn escape_menu_label(label: &str) -> String {
        label.replace('&', "&&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AiProjectEntry, AiProjectKind, AiProjects};

    fn entry(id: &str, name: &str, kind: AiProjectKind) -> AiProjectEntry {
        AiProjectEntry {
            id: id.into(),
            name: name.into(),
            kind,
        }
    }

    fn projects(entries: Vec<AiProjectEntry>, counts: (usize, usize, usize)) -> AiProjects {
        AiProjects {
            attention: counts.0,
            working: counts.1,
            done: counts.2,
            entries,
        }
    }

    /// 菜单 label = `emoji 空格 项目名 " · " 状态文案`,四个档位的 emoji 不能串。
    #[test]
    fn 菜单条目按_emoji_名字_状态文案拼() {
        let ai = projects(
            vec![
                entry("p1", "alpha", AiProjectKind::Attention),
                entry("p2", "beta", AiProjectKind::Working),
                entry("p3", "gamma", AiProjectKind::Done),
                entry("p4", "delta", AiProjectKind::Idle),
            ],
            (1, 1, 1),
        );
        let out = menu_entries(&ai, 10);
        // 文案随语言走,格式不随 —— 用 t() 现算,免得把测试钉死在中文上
        assert_eq!(
            out[0].label,
            format!("🟡 alpha · {}", t("app", "trayStatus.attention"))
        );
        assert_eq!(
            out[1].label,
            format!("🔵 beta · {}", t("app", "trayStatus.working"))
        );
        assert_eq!(
            out[2].label,
            format!("🟢 gamma · {}", t("app", "trayStatus.done"))
        );
        assert_eq!(
            out[3].label,
            format!("⚪ delta · {}", t("app", "trayStatus.idle"))
        );
        // id 原样带出去(点菜单要用它定位项目)
        assert_eq!(out[1].id, "p2");
    }

    /// `trayMaxProjects` 截断:超出的**直接不显示**,没有「还有 N 个」的省略行。
    #[test]
    fn 超过上限的项目直接截掉() {
        let ai = projects(
            vec![
                entry("p1", "a", AiProjectKind::Attention),
                entry("p2", "b", AiProjectKind::Working),
                entry("p3", "c", AiProjectKind::Done),
            ],
            (1, 1, 1),
        );
        assert_eq!(menu_entries(&ai, 2).len(), 2);
        assert_eq!(menu_entries(&ai, 0).len(), 0, "手改成 0 就是空菜单");
        assert_eq!(menu_entries(&ai, 99).len(), 3, "上限大于条数不补空行");
    }

    /// tooltip:0 的那档整条不出现;三档齐全时用 ` · ` 连接。
    #[test]
    fn tooltip_跳过零计数() {
        assert_eq!(tooltip(0, 0, 0), "");
        assert_eq!(tooltip(2, 0, 0), tr!("app", "trayAttention", count = 2));
        assert_eq!(
            tooltip(2, 1, 3),
            format!(
                "{} · {} · {}",
                tr!("app", "trayAttention", count = 2),
                tr!("app", "trayWorking", count = 1),
                tr!("app", "trayDone", count = 3)
            )
        );
        // 中间那档为 0 时不留空段
        assert_eq!(
            tooltip(2, 0, 3),
            format!(
                "{} · {}",
                tr!("app", "trayAttention", count = 2),
                tr!("app", "trayDone", count = 3)
            )
        );
    }

    /// 三盏灯由 **pane 级计数**点亮(>0 即亮),`ai-idle` 不参与计数所以不点灯。
    #[test]
    fn 灯色由计数点亮而不是由条目点亮() {
        let ai = projects(vec![entry("p1", "a", AiProjectKind::Idle)], (0, 0, 0));
        let snap = build_snapshot(true, false, &ai, 5);
        assert_eq!(snap.lamps, Lamps::default(), "只有 ai-idle 的项目不点灯");
        assert_eq!(snap.projects.len(), 1, "但它照样进菜单");
        assert_eq!(snap.tooltip, "");

        let ai = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 2, 0));
        let snap = build_snapshot(true, false, &ai, 5);
        assert_eq!(
            snap.lamps,
            Lamps {
                attention: false,
                working: true,
                done: false
            }
        );
    }

    /// 去重签名:含开关/焦点/三计数/全部 label,**不含 tooltip**。
    #[test]
    fn 签名覆盖开关焦点计数与标签() {
        let ai = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 1, 0));
        let base = build_snapshot(true, false, &ai, 5);

        // 焦点变了 → 签名必须变(闪烁策略要跟着变)
        assert_ne!(base.signature, build_snapshot(true, true, &ai, 5).signature);
        // 开关变了 → 签名必须变
        assert_ne!(base.signature, build_snapshot(false, false, &ai, 5).signature);
        // 计数变了 → 签名必须变(灯没变但 tooltip 变了)
        let more = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 3, 0));
        assert_ne!(base.signature, build_snapshot(true, false, &more, 5).signature);
        // 项目名变了 → label 变 → 签名变
        let renamed = projects(vec![entry("p1", "b", AiProjectKind::Working)], (0, 1, 0));
        assert_ne!(
            base.signature,
            build_snapshot(true, false, &renamed, 5).signature
        );
        // 什么都没变 → 签名一致(否则去重失效,每次 notify 都重建图标)
        assert_eq!(base.signature, build_snapshot(true, false, &ai, 5).signature);
    }

    /// 上限把某个项目挤出去之后,签名也要跟着变(否则改上限不生效)。
    #[test]
    fn 上限变化会改签名() {
        let ai = projects(
            vec![
                entry("p1", "a", AiProjectKind::Attention),
                entry("p2", "b", AiProjectKind::Working),
            ],
            (1, 1, 0),
        );
        assert_ne!(
            build_snapshot(true, false, &ai, 1).signature,
            build_snapshot(true, false, &ai, 2).signature
        );
    }

    #[test]
    fn 活跃色按黄蓝绿固定顺序() {
        assert_eq!(active_colors(Lamps::default()).len(), 0);
        assert_eq!(
            active_colors(Lamps {
                attention: true,
                working: false,
                done: false
            }),
            vec![YELLOW]
        );
        assert_eq!(
            active_colors(Lamps {
                attention: true,
                working: true,
                done: true
            }),
            vec![YELLOW, BLUE, GREEN]
        );
    }

    #[test]
    fn 单灯位的帧色语义() {
        // 安静 → 灰,不闪
        assert_eq!(frame_color(&[], 3, true), (GRAY, false));
        // 单状态闪烁:偶帧亮奇帧暗;静止时恒亮
        assert_eq!(frame_color(&[YELLOW], 0, true), (YELLOW, false));
        assert_eq!(frame_color(&[YELLOW], 1, true), (YELLOW, true));
        assert_eq!(frame_color(&[YELLOW], 1, false), (YELLOW, false));
        // 多状态闪烁:同一灯位颜色轮转,全亮
        let colors = [YELLOW, BLUE, GREEN];
        assert_eq!(frame_color(&colors, 0, true), (YELLOW, false));
        assert_eq!(frame_color(&colors, 1, true), (BLUE, false));
        assert_eq!(frame_color(&colors, 2, true), (GREEN, false));
        assert_eq!(frame_color(&colors, 3, true), (YELLOW, false));
        // 多状态静止:停在最高优先级色
        assert_eq!(frame_color(&colors, 2, false), (YELLOW, false));
    }

    /// 闪烁三档:聚焦不闪 / 失焦多状态持续轮转 / 失焦单状态爆闪几帧后定格。
    #[test]
    fn 闪烁相位三档() {
        let mut blink = Blink::default();
        assert!(!blink.tick(1, true, true), "聚焦时不推帧");
        assert!(!blink.tick(0, true, false), "安静时不推帧");
        assert!(!blink.tick(1, false, false), "开关关掉不推帧");

        // 单状态:BURST_FRAMES 帧之后定格,再也不推
        let mut blink = Blink::default();
        for i in 0..BURST_FRAMES {
            assert!(blink.tick(1, true, false), "第 {i} 帧应当重绘");
        }
        assert!(blink.tick(1, true, false), "定格那一帧要补一次全亮重绘");
        assert!(blink.settled);
        assert!(!blink.tick(1, true, false), "定格之后不再推帧");
        assert!(!blink.blinking(false), "定格 = 静止");

        // 多状态:永远轮转,不定格
        let mut blink = Blink::default();
        for _ in 0..(BURST_FRAMES * 3) {
            assert!(blink.tick(2, true, false));
        }
        assert!(!blink.settled, "多状态不进定格");

        // 灯色变化 → 相位归零、重新允许爆闪
        blink.settled = true;
        blink.frame = 42;
        blink.reset();
        assert_eq!(blink.frame, 0);
        assert!(!blink.settled);
    }

    /// 画布:圆心是纯色不透明,四角在圆外必须完全透明。
    #[test]
    fn 圆点画在画布正中且四角透明() {
        const SIZE: u32 = 16;
        let rgba = compose_frame_rgba(SIZE, GREEN, false);
        assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
        let center = ((SIZE / 2) * SIZE + SIZE / 2) as usize * 4;
        assert_eq!(&rgba[center..center + 3], &GREEN);
        assert_eq!(rgba[center + 3], 255);
        // 左上角、右下角都在半径之外
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[(SIZE * SIZE * 4 - 1) as usize], 0);
    }

    /// 暗帧只压 alpha,不改颜色。
    #[test]
    fn 暗帧压低_alpha_不改颜色() {
        const SIZE: u32 = 16;
        let bright = compose_frame_rgba(SIZE, YELLOW, false);
        let dim = compose_frame_rgba(SIZE, YELLOW, true);
        let center = ((SIZE / 2) * SIZE + SIZE / 2) as usize * 4;
        assert_eq!(bright[center + 3], 255);
        let a = dim[center + 3];
        assert!(a > 0 && a < 100, "暗帧 alpha 应当落在 (0,100),实际 {a}");
        assert_eq!(&dim[center..center + 3], &YELLOW);
    }

    /// 画布跟着 `SM_CXSMICON` 走,任何尺寸都得画出一个居中的圆。
    #[test]
    fn 不同画布尺寸都居中() {
        for size in [16u32, 20, 24, 32] {
            let rgba = compose_frame_rgba(size, BLUE, false);
            let center = ((size / 2) * size + size / 2) as usize * 4;
            assert_eq!(&rgba[center..center + 3], &BLUE, "size={size}");
            assert_eq!(rgba[center + 3], 255, "size={size}");
            assert_eq!(rgba[3], 0, "size={size} 左上角应透明");
        }
    }
}
