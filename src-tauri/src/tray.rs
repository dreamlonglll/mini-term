//! 菜单栏状态灯:**单个灯位**,按当前状态着色,多状态时同一灯位交替变色。
//!
//! 颜色语义(与主窗口 StatusDot 按语义对齐,非逐色复刻——主窗口的处理中
//! 是随皮肤走的主题色旋转弧,托盘没有形状/动画,必须用固定颜色区分):
//!   黄 = 有 pane 需要用户确认(授权/输入请求,含 error 异常)
//!   蓝 = 有 pane 处理中(ai-working,含 API 重试)
//!   绿 = 有已完成且未读的回答(激活主窗口即清除)
//!   灰 = 全部安静(静止不闪)
//! 静止时停在最高优先级色(黄>蓝>绿);tray-icon 在 macOS 把图标等比
//! 缩放到高 18pt,这里按 2x(36px)绘制保证 retina 清晰。
//!
//! 闪烁:后台线程每 BLINK_MS 走一帧,经 run_on_main_thread 重绘
//! (NSStatusItem 只能在主线程操作)。策略:聚焦不闪;失焦多状态 =
//! 同一灯位颜色轮转;失焦单状态 = 状态变化后短促亮暗呼吸几轮再定格全亮。
//!
//! 交互:左键唤起主窗口并跳最高优先级项目;右键菜单列出所有进入
//! AI agent 的项目及状态(含 ai-idle 空闲待命的,前端聚合时生成,
//! 与灯的数据源一致),点击项目跳转。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

/// 单个圆点的画布边长(2x,显示为 18pt)
const DOT: u32 = 36;
const TRAY_ID: &str = "status-light";
/// 闪烁帧间隔
const BLINK_MS: u64 = 600;
/// 单状态时状态变化后的短促闪烁帧数(约 3.6s),之后定格全亮——
/// 持续呼吸闪太抢注意力(用户反馈),只在「有新变化」时闪一阵提醒
const BURST_FRAMES: usize = 6;
/// 暗帧的 alpha 系数(255 * 0.35)
const DIM: f32 = 0.35;

// Apple 系统色板
const GRAY: [u8; 3] = [0x8E, 0x8E, 0x93];
const BLUE: [u8; 3] = [0x0A, 0x84, 0xFF];
const YELLOW: [u8; 3] = [0xFF, 0xCC, 0x00];
const GREEN: [u8; 3] = [0x34, 0xC7, 0x59];

/// 托盘灯的共享状态:`set_tray_status` 写入,闪烁线程读取。
/// 所有 UI 操作(图标/tooltip/菜单)统一在主线程从这里读最新值绘制,
/// 写入者(command 线程池、闪烁线程)乱序到达也不会画出过期帧。
#[derive(Default)]
pub struct TrayLightState {
    attention: bool,
    working: bool,
    done: bool,
    enabled: bool,
    /// 主窗口是否聚焦:聚焦时不闪(用户正看着,无需吸引注意)
    focused: bool,
    /// 闪烁帧计数,灯色/焦点变化时归零(让新状态从「全亮」帧开始)
    frame: usize,
    /// 单状态短促闪烁结束后已定格全亮,不再重绘
    settled: bool,
    /// 前端推送的单调序号:command 在线程池上可能乱序执行,
    /// 序号小于已应用值的推送直接丢弃,防止旧状态覆盖新状态
    seq: u64,
    tooltip: String,
    /// 右键菜单项 (project_id, label)
    projects: Vec<(String, String)>,
}

pub type SharedTrayState = Arc<Mutex<TrayLightState>>;

/// 当前活跃的颜色集合(顺序固定 黄→蓝→绿)
fn active_colors(s: &TrayLightState) -> Vec<[u8; 3]> {
    let mut colors = Vec::new();
    if s.attention {
        colors.push(YELLOW);
    }
    if s.working {
        colors.push(BLUE);
    }
    if s.done {
        colors.push(GREEN);
    }
    colors
}

/// 画单个圆点(托盘永远只占一个灯位;多状态靠交替变色表达)。
/// `dim` = 暗帧(alpha 压到 DIM,用于单状态呼吸闪烁)。
fn compose_frame(color: [u8; 3], dim: bool) -> Image<'static> {
    let w = DOT;
    let h = DOT;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let radius = DOT as f32 / 2.0 - 5.0;
    let cx = (DOT / 2) as f32 - 0.5;
    let cy = h as f32 / 2.0 - 0.5;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            // 1px 软边抗锯齿
            let mut alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if dim {
                alpha *= DIM;
            }
            let alpha = (alpha * 255.0) as u8;
            if alpha > 0 {
                let idx = ((y * w + x) * 4) as usize;
                rgba[idx] = color[0];
                rgba[idx + 1] = color[1];
                rgba[idx + 2] = color[2];
                rgba[idx + 3] = alpha;
            }
        }
    }
    Image::new_owned(rgba, w, h)
}

/// 本帧该显示的颜色与明暗。
/// 静止(聚焦/已定格/安静) = 最高优先级色全亮;
/// 失焦单状态 = 亮暗呼吸;失焦多状态 = 颜色轮转(全亮)。
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

/// 按共享状态的当前帧重绘图标(必须在主线程调用)
fn redraw(app: &AppHandle, state: &SharedTrayState) {
    let (colors, frame, blinking) = {
        let s = state.lock().unwrap();
        if !s.enabled {
            return;
        }
        let blinking = !s.focused && !s.settled;
        (active_colors(&s), s.frame, blinking)
    };
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let (color, dim) = frame_color(&colors, frame, blinking);
    let _ = tray.set_icon(Some(compose_frame(color, dim)));
}

/// 状态推送后的全量应用:可见性 + 图标 + tooltip + 菜单(必须在主线程调用)。
/// 执行时从共享状态读最新值,多个推送的闭包乱序/排队执行结果幂等。
fn apply_full(app: &AppHandle, state: &SharedTrayState) {
    let (enabled, tooltip, projects) = {
        let s = state.lock().unwrap();
        (s.enabled, s.tooltip.clone(), s.projects.clone())
    };
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let _ = tray.set_visible(enabled);
    if !enabled {
        return;
    }
    redraw(app, state);
    let _ = tray.set_tooltip(if tooltip.is_empty() {
        None
    } else {
        Some(tooltip.as_str())
    });

    // 重建右键菜单:进入 AI agent 的项目列表(数量小,整个重建成本可忽略)
    let mut builder = MenuBuilder::new(app);
    for (id, label) in &projects {
        if let Ok(item) = MenuItemBuilder::with_id(format!("proj:{}", id), label).build(app) {
            builder = builder.item(&item);
        }
    }
    if let Ok(menu) = builder.build() {
        let _ = tray.set_menu(if projects.is_empty() { None } else { Some(menu) });
    }
}

/// setup 里调用:创建托盘 + 启动闪烁线程
pub fn init_tray(app: &AppHandle) -> tauri::Result<SharedTrayState> {
    let state: SharedTrayState = Arc::new(Mutex::new(TrayLightState {
        enabled: true,
        // 启动即认为聚焦(主窗口马上显示并获焦),避免开局按失焦语义闪烁;
        // 首次 set_tray_status 会带来真实焦点值
        focused: true,
        ..Default::default()
    }));

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(compose_frame(GRAY, false))
        // 彩色语义图标,不能标 template(template 会被 macOS 强制单色化)
        .icon_as_template(false)
        // 左键保持「唤起+跳转」,菜单只挂右键
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    let app = tray.app_handle();
                    focus_main_window(app);
                    let _ = app.emit("tray-clicked", ());
                }
            }
        })
        .on_menu_event(|app, event| {
            if let Some(project_id) = event.id().as_ref().strip_prefix("proj:") {
                focus_main_window(app);
                let _ = app.emit("tray-project-clicked", project_id.to_string());
            }
        })
        .build(app)?;

    // 闪烁线程:只在「开关开启 + 失焦 + 有活跃状态」时走帧重绘。
    // 聚焦时不闪(用户正看着);多状态失焦持续交替;单状态失焦只在
    // 状态变化后短促闪 BURST_FRAMES 帧,之后定格全亮不再打扰
    let thread_state = state.clone();
    let thread_app = app.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(BLINK_MS));
        {
            let mut s = thread_state.lock().unwrap();
            let n = active_colors(&s).len();
            if !s.enabled || n == 0 || s.focused || s.settled {
                continue;
            }
            if n == 1 && s.frame >= BURST_FRAMES {
                // 短促闪烁结束:补一帧全亮定格,之后跳过
                s.settled = true;
            } else {
                s.frame = s.frame.wrapping_add(1);
            }
        }
        let app = thread_app.clone();
        let st = thread_state.clone();
        let _ = thread_app.run_on_main_thread(move || redraw(&app, &st));
    });

    Ok(state)
}

fn focus_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 托盘菜单里的一个进入 AI agent 的项目(label 由前端拼好,含 emoji 灯色与 i18n 状态文案)
#[derive(Deserialize)]
pub struct TrayProjectEntry {
    pub id: String,
    pub label: String,
}

/// 前端聚合后推送托盘状态;tooltip/projects 由前端拼好(含 i18n)。
/// enabled=false 时隐藏托盘图标(设置里的「菜单栏项目状态」开关);
/// focused = 主窗口聚焦中(不闪烁);seq = 前端单调递增序号(乱序裁决)。
/// 本函数只写共享状态,一切 UI 操作经 run_on_main_thread 走 apply_full
/// (NSStatusItem 只能在主线程操作,且保证落屏的永远是最新状态)。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn set_tray_status(
    app: AppHandle,
    state: tauri::State<'_, SharedTrayState>,
    seq: u64,
    attention: bool,
    working: bool,
    done: bool,
    tooltip: String,
    projects: Vec<TrayProjectEntry>,
    enabled: bool,
    focused: bool,
) {
    {
        let mut s = state.lock().unwrap();
        if seq <= s.seq {
            return; // 线程池乱序:更新的推送已应用过,丢弃旧的
        }
        s.seq = seq;
        // 只有灯色/焦点真的变化才重置闪烁相位——tooltip/菜单等无关变化
        // 不打断多状态轮转,也不重启单状态的短促闪烁
        let lamps_changed = s.attention != attention
            || s.working != working
            || s.done != done
            || s.focused != focused;
        s.attention = attention;
        s.working = working;
        s.done = done;
        s.enabled = enabled;
        s.focused = focused;
        s.tooltip = tooltip;
        s.projects = projects.into_iter().map(|p| (p.id, p.label)).collect();
        if lamps_changed {
            s.frame = 0; // 新状态从「全亮」帧开始
            s.settled = false; // 重新允许短促闪烁
        }
    }
    eprintln!(
        "[tray] set_tray_status seq={} attention={} working={} done={} focused={}",
        seq, attention, working, done, focused
    );
    let inner = state.inner().clone();
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || apply_full(&app2, &inner));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(attention: bool, working: bool, done: bool) -> TrayLightState {
        TrayLightState {
            attention,
            working,
            done,
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn active_colors_priority_order() {
        assert_eq!(active_colors(&state(false, false, false)).len(), 0);
        assert_eq!(active_colors(&state(true, false, false)), vec![YELLOW]);
        // 黄→蓝→绿 固定优先级
        assert_eq!(active_colors(&state(true, true, true)), vec![YELLOW, BLUE, GREEN]);
    }

    #[test]
    fn frame_color_single_dot_semantics() {
        // 安静 → 灰,不闪
        assert_eq!(frame_color(&[], 3, true), (GRAY, false));
        // 单状态闪烁:偶帧亮奇帧暗;静止时恒亮
        assert_eq!(frame_color(&[YELLOW], 0, true), (YELLOW, false));
        assert_eq!(frame_color(&[YELLOW], 1, true), (YELLOW, true));
        assert_eq!(frame_color(&[YELLOW], 1, false), (YELLOW, false));
        // 多状态闪烁:同一灯位颜色轮转,全亮
        let colors = vec![YELLOW, BLUE, GREEN];
        assert_eq!(frame_color(&colors, 0, true), (YELLOW, false));
        assert_eq!(frame_color(&colors, 1, true), (BLUE, false));
        assert_eq!(frame_color(&colors, 2, true), (GREEN, false));
        assert_eq!(frame_color(&colors, 3, true), (YELLOW, false));
        // 多状态静止:停在最高优先级色
        assert_eq!(frame_color(&colors, 2, false), (YELLOW, false));
    }

    #[test]
    fn canvas_is_always_single_dot() {
        let img = compose_frame(GREEN, false);
        assert_eq!(img.width(), DOT);
        let w = img.width() as usize;
        let center = ((DOT as usize / 2) * w + DOT as usize / 2) * 4;
        assert_eq!(&img.rgba()[center..center + 3], &GREEN);
    }

    #[test]
    fn dim_frame_reduces_alpha() {
        let bright = compose_frame(YELLOW, false);
        let dim = compose_frame(YELLOW, true);
        let w = DOT as usize;
        let center_a = ((w / 2) * w + w / 2) * 4 + 3;
        assert_eq!(bright.rgba()[center_a], 255);
        let a = dim.rgba()[center_a];
        assert!(a < 100 && a > 0);
    }
}
