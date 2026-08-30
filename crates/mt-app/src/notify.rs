//! AI 完成 / 待确认通知的状态机与平台提醒。
//!
//! 对照 `src/store.ts` 的 `updatePaneStatusByPty` 第 3~4 段与
//! `src/utils/aiCompletion.ts`,把三件事搬过来:
//!
//! | TS 侧 | 这里 |
//! |---|---|
//! | `isAiCompletion` | [`is_completion`] |
//! | `isAttentionRise` | [`is_attention_rise`] |
//! | `unreadDonePaneIds` / `aiDoneOrder` | [`DoneTracker`] |
//! | `pickAttentionTarget`(attentionTarget.ts) | [`pick_attention_target`] |
//! | `playNotificationSound` / `requestUserAttention` | [`play_sound`] / [`flash_taskbar`] |
//!
//! **托盘不做**(交付范围里明确排除),于是 `syncTrayStatus` / `collectAiProjects`
//! 没有搬。`unreadDonePaneIds` 的消费方因此从「托盘绿灯」改成了壳内的未读计数
//! 与「跳到下一个待办」,判据(看窗口焦点)原样保留 —— 托盘补上时不必再改这里。

use std::collections::{HashMap, HashSet};

use crate::tree::PaneStatus;

/// hook 事件名里唯一表示「这一轮任务真的做完了」的成因。
///
/// `StopFailure` / `PermissionRequest` / `Notification` / `Elicitation` /
/// `Interrupt` / `Stall` 同样落 ai-idle,但它们是「又要你来处理一下」而不是完成,
/// 播报即误报(判据与 `src/utils/aiCompletion.ts` 逐字同源)。
const COMPLETION_CAUSE: &str = "Stop";

/// 这次状态变化是否构成「AI 任务完成」。
///
/// `cause == None` 表示这次变化来自无 hook 的降级路径(WSL / SSH / hook 关闭),
/// 那条路径压根收不到事件名,下降沿是它唯一的完成信号,必须放行 —— 否则这些
/// pane 会彻底收不到完成通知。
pub fn is_completion(old: PaneStatus, new: PaneStatus, cause: Option<&str>) -> bool {
    if old != PaneStatus::AiWorking || new != PaneStatus::AiIdle {
        return false;
    }
    match cause {
        None => true,
        Some(c) => c == COMPLETION_CAUSE,
    }
}

/// 「AI 转入待确认」的**上升沿** —— 待确认提醒的唯一判据。
///
/// 不能只看 `is_attention_cause`:后端 `StatusEmitter` 把 attention 类事件显式
/// 排除在去重之外,同一次待确认会连推多条,按 cause 判会一次待确认响好几声。
pub fn is_attention_rise(prev_attention: bool, cause: Option<&str>) -> bool {
    cause.map(mt_ai::is_attention_cause).unwrap_or(false) && !prev_attention
}

/// 一次状态变化的全部输入(纯数据,方便单测)。
pub struct StatusTransition<'a> {
    pub pane_id: &'a str,
    pub old_status: PaneStatus,
    pub new_status: PaneStatus,
    /// 变化**前**该 pane 的 attention 标记(黄灯是否已亮)。
    pub old_attention: bool,
    /// hook 事件名;无 hook 的降级路径为 `None`。
    pub cause: Option<&'a str>,
    /// 主窗口是否聚焦 —— 只影响「未读完成」的计入,不影响提示音/闪烁。
    pub window_focused: bool,
    /// 该 pane 所属项目是否就是当前激活项目(决定要不要弹 toast)。
    pub project_active: bool,
}

/// 通知开关(取自 `AppConfig`,原样透传)。
#[derive(Clone, Copy, Debug)]
pub struct NotifyPrefs {
    pub sound: bool,
    pub flash: bool,
    pub popup: bool,
    /// 待确认提醒开关独立:它的触发频率远高于完成,想只留完成通知的用户得能单独关。
    pub attention_notify: bool,
}

/// toast 的五种口径,与旧版 `AiCompletionNotification['kind']` 同集
/// (旧版 `kind` 缺省即 [`Completion`](Self::Completion),`store.ts:1035` 的判据
/// 显式把 `undefined` 算进完成态)。
///
/// [`AlertPlan`] 只会产出前两种 —— 后三种由别处直接推(见 [`crate::toast`])。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// AI 任务完成。
    Completion,
    /// AI 停下来等你批权限 / 填表单 / 这轮因 API 错误结束。
    Attention,
    /// 信息提示：WSL 启动器替换、远程项目暂不支持搜索等非错误状态。
    WslInfo,
    /// 移动端发起了一个新会话(`mobileStartSession.ts`)。
    MobileSession,
    /// 长文本转存 / 远程上传失败(`terminalCache.ts:690-695`)。
    PasteError,
}

impl ToastKind {
    /// 圆形图标里那个字符。**原版就是文本字符**(`ToastContainer.tsx:53`),
    /// 不是 svg —— 照抄反而与原版一字不差,也绕开本仓没注册 `AssetSource` 的坑。
    pub fn icon_char(self) -> &'static str {
        match self {
            Self::Completion => "✓",
            Self::Attention | Self::PasteError => "!",
            Self::WslInfo | Self::MobileSession => "i",
        }
    }

    /// 点这条 toast 要不要顺带切到那个项目。
    ///
    /// `wsl-info` 只陈述当前状态、`paste-error` 的项目就在眼前 —— 两者点击
    /// **仅关闭**(`ToastContainer.tsx:35-38`)。WSL 启动提示仍使用占位项目 id，
    /// 其它信息提示可以携带真实项目 id，但同样不跳转。
    pub fn jumps_to_project(self) -> bool {
        matches!(
            self,
            Self::Completion | Self::Attention | Self::MobileSession
        )
    }

    /// 正文取自 `message` 字段吗(`false` = 按 kind 取固定文案)。
    /// 判定链照抄 `ToastContainer.tsx:57-63`。
    pub fn uses_message(self) -> bool {
        matches!(self, Self::WslInfo | Self::MobileSession | Self::PasteError)
    }
}

/// 这次变化要执行的提醒动作。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlertPlan {
    pub sound: bool,
    pub flash: bool,
    pub toast: Option<ToastKind>,
    /// 项目行上的绿色「完成」标(只有完成才置,待确认不置 —— 语义对不上)。
    pub mark_needs_attention: bool,
}

impl AlertPlan {
    pub fn is_empty(&self) -> bool {
        *self == AlertPlan::default()
    }
}

/// 完成队列:未读集合 + 完成序号。
///
/// 两份口径**故意不同**(旧版同一注释):
/// - `unread`:看窗口焦点。窗口聚焦时完成的任务用户正看着,不算未读。
/// - `order`:不看窗口焦点(点状态灯时窗口必然聚焦),用于「先完成的先跳」。
#[derive(Default)]
pub struct DoneTracker {
    unread: HashSet<String>,
    order: HashMap<String, u64>,
    /// 单调发号器。取序号而不是时间戳:同一批完成事件常落在同一毫秒里。
    seq: u64,
}

impl DoneTracker {
    /// 吃进一次状态变化,更新两份队列并给出提醒动作。
    pub fn apply(&mut self, t: &StatusTransition<'_>, prefs: &NotifyPrefs) -> AlertPlan {
        let attention = t.cause.map(mt_ai::is_attention_cause).unwrap_or(false);
        let completion = is_completion(t.old_status, t.new_status, t.cause);
        // hook 的 Stop 是权威信号:ai-idle(待确认)→ 批准 → Stop 这类不经过
        // ai-working 的路径靠它补上完成记账(无下降沿,不播报)。
        let done = t.cause == Some(COMPLETION_CAUSE) || completion;

        // 一个 pane 任一时刻只贡献一种灯:转入待确认/异常时旧的「完成未读」作废,
        // 否则同一个 pane 黄绿双计。
        if attention || t.new_status == PaneStatus::Error {
            self.unread.remove(t.pane_id);
        }
        if done && !attention && !t.window_focused {
            self.unread.insert(t.pane_id.to_string());
        }

        // 已在队列里的不重新发号:同一次任务的多个 Stop 不该把它挤到队尾。
        let should_queue = done && !attention && t.new_status != PaneStatus::AiWorking;
        if should_queue {
            if !self.order.contains_key(t.pane_id) {
                self.seq += 1;
                self.order.insert(t.pane_id.to_string(), self.seq);
            }
        } else {
            self.order.remove(t.pane_id);
        }

        let mut plan = AlertPlan::default();
        if completion {
            // 提示音与任务栏闪烁不区分激活项目
            plan.sound = prefs.sound;
            plan.flash = prefs.flash;
            if !t.project_active {
                plan.mark_needs_attention = true;
                if prefs.popup {
                    plan.toast = Some(ToastKind::Completion);
                }
            }
        } else if prefs.attention_notify && is_attention_rise(t.old_attention, t.cause) {
            plan.sound = prefs.sound;
            plan.flash = prefs.flash;
            // 不设 needsAttention:那是项目行上绿色的「完成」标,语义对不上
            if !t.project_active && prefs.popup {
                plan.toast = Some(ToastKind::Attention);
            }
        }
        plan
    }

    /// pane 关掉后撤出两份队列 —— 否则计数会往一个已经不存在的 pane 上跳,
    /// 两张表也会随开关终端无界增长(旧版 `setProjectLayout` 的同一段)。
    pub fn retain_panes(&mut self, live: &HashSet<String>) {
        self.unread.retain(|id| live.contains(id));
        self.order.retain(|id, _| live.contains(id));
    }

    pub fn unread_count(&self) -> usize {
        self.unread.len()
    }

    pub fn is_unread(&self, pane_id: &str) -> bool {
        self.unread.contains(pane_id)
    }

    pub fn clear_unread(&mut self) {
        self.unread.clear();
    }

    pub fn order(&self) -> &HashMap<String, u64> {
        &self.order
    }
}

/// 挑目标用的一条 pane 快照。
///
/// `Copy` 是给「一份 `Vec<PaneRef>` 喂给多个聚合器」用的(见
/// [`AppStore::title_bar_snapshot`](crate::store::AppStore::title_bar_snapshot)):
/// 字段全是 `&str` 与 Copy 标量,复制一条就是几个字长。
#[derive(Clone, Copy)]
pub struct PaneRef<'a> {
    pub project_id: &'a str,
    pub pane_id: &'a str,
    pub status: PaneStatus,
    pub attention: bool,
}

/// 「下一件该我做的事」在哪个 pane(`src/utils/attentionTarget.ts` 的搬运)。
///
/// 优先级:待确认/异常 > 已完成(最先完成的排最前)> 处理中。
pub fn pick_attention_target<'a>(
    panes: impl IntoIterator<Item = PaneRef<'a>>,
    order: &HashMap<String, u64>,
) -> Option<(String, String)> {
    let mut attention: Option<(String, String)> = None;
    let mut done: Option<(String, String, u64)> = None;
    let mut working: Option<(String, String)> = None;

    for p in panes {
        if p.status == PaneStatus::Error || p.attention {
            attention.get_or_insert_with(|| (p.project_id.to_string(), p.pane_id.to_string()));
            continue;
        }
        match order.get(p.pane_id) {
            Some(&seq) => {
                if done.as_ref().map(|d| seq < d.2).unwrap_or(true) {
                    done = Some((p.project_id.to_string(), p.pane_id.to_string(), seq));
                }
            }
            None if p.status == PaneStatus::AiWorking => {
                working.get_or_insert_with(|| (p.project_id.to_string(), p.pane_id.to_string()));
            }
            None => {}
        }
    }

    attention
        .or_else(|| done.map(|(a, b, _)| (a, b)))
        .or(working)
}

// ─── 提示音的双音合成 ────────────────────────────────────────

/// 采样率 / 位深 / 声道:44.1kHz、16-bit、单声道 —— `PlaySoundW` 最保守的兼容组合。
const SAMPLE_RATE: u32 = 44_100;
/// 峰值增益(线性),与 `gain.setValueAtTime(0.3, ...)` 同值。
const PEAK_GAIN: f32 = 0.3;
/// 指数衰减的落点,与 `exponentialRampToValueAtTime(0.01, ...)` 同值(≈ -30dB)。
const FLOOR_GAIN: f32 = 0.01;
/// 两段正弦:`(频率 Hz, 起点秒, 终点秒)`。逐字照抄
/// `src/utils/notificationSound.ts:14-33` —— 880Hz(A5)→ 660Hz(E5) 的下行纯四度,
/// 中间 10ms 静默,总长 280ms。
const TONES: [(f32, f32, f32); 2] = [(880.0, 0.0, 0.12), (660.0, 0.13, 0.28)];
/// 整段波形的时长(秒)。
const WAVE_SECONDS: f32 = 0.28;
/// WAV 头固定 44 字节(RIFF + fmt(16) + data)。
const WAV_HEADER_LEN: usize = 44;

/// 内置双音的 WAV 字节(合成一次、进程内长活)。
///
/// `SND_ASYNC` 下 `PlaySoundW` 立刻返回、后台继续读那块内存,所以缓冲区**必须
/// 在播放期间存活**。波形是常量,`OnceLock` 既解决存活问题又省掉每次合成的开销。
static NOTIFICATION_WAVE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

fn notification_wave() -> &'static [u8] {
    NOTIFICATION_WAVE.get_or_init(build_notification_wave)
}

/// 合成 880Hz→660Hz 双音的完整 RIFF/WAVE 字节。
///
/// 包络 `env(t) = PEAK * (FLOOR/PEAK)^(t/dur)` 与 Web Audio 的
/// `exponentialRampToValueAtTime` **同形**(指数插值,端点分别是 0.3 与 0.01),
/// 因此听感与旧版一致 —— 这正是不用 `Beep()` 的理由:方波、无包络、还阻塞线程。
fn build_notification_wave() -> Vec<u8> {
    let total = (WAVE_SECONDS * SAMPLE_RATE as f32).round() as usize;
    let mut samples = vec![0i16; total];
    for (freq, start, end) in TONES {
        let dur = end - start;
        let from = (start * SAMPLE_RATE as f32).round() as usize;
        let to = ((end * SAMPLE_RATE as f32).round() as usize).min(total);
        for i in from..to {
            // 段内时间从 0 重新起算(两段各自完整衰减一次)
            let t = (i - from) as f32 / SAMPLE_RATE as f32;
            let env = PEAK_GAIN * (FLOOR_GAIN / PEAK_GAIN).powf(t / dur);
            let s = (std::f32::consts::TAU * freq * t).sin() * env;
            samples[i] = (s * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }
    encode_wav_mono16(&samples, SAMPLE_RATE)
}

/// 16-bit 单声道 PCM → 完整 RIFF/WAVE 字节(小端)。
fn encode_wav_mono16(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(WAV_HEADER_LEN + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk 长度
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate = rate × block_align
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ─── 平台提醒 ────────────────────────────────────────────────

/// 提示音。三级回落:
///
/// 1. 自定义 `.wav` → `PlaySoundW(SND_FILENAME)`,放得出来就到此为止;
/// 2. **内置 880→660 双音**(内存合成的 WAV,`SND_MEMORY`)—— 与旧版 Web Audio
///    那两段正弦的频率 / 时长 / 间隔 / 指数包络一一对应;
/// 3. `MessageBeep(MB_OK)` 兜底(前两条都失败时才轮到它)。
///
/// **不用 `Beep(freq, ms)`**:它同步阻塞调用线程 280ms —— GPUI 是单线程 UI,
/// 那是肉眼可见的 17 帧卡顿;而且它只有方波、没有音量与包络。
///
/// 已知偏差:自定义音只认 `.wav`(旧版走浏览器 `Audio`,mp3/ogg 都能放)。
#[cfg(windows)]
pub fn play_sound(custom_path: Option<&str>) {
    use windows::Win32::Media::Audio::{
        PlaySoundW, SND_ASYNC, SND_FILENAME, SND_MEMORY, SND_NODEFAULT,
    };
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_OK;
    use windows::core::HSTRING;

    if let Some(path) = custom_path.filter(|p| p.to_ascii_lowercase().ends_with(".wav")) {
        let wide = HSTRING::from(path);
        // SAFETY: 只传一个以 NUL 结尾的宽字符串,不涉及跨线程共享
        let ok = unsafe {
            PlaySoundW(
                windows::core::PCWSTR(wide.as_ptr()),
                None,
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            )
        };
        if ok.as_bool() {
            return;
        }
        // 放不出来(文件没了 / 格式不认)时继续往下走内置双音,而不是静默
    }

    let wave = notification_wave();
    // SAFETY: `SND_MEMORY` 下第一个参数是**内存镜像指针**而非字符串;缓冲区来自
    // `OnceLock`,进程内永久存活,满足 `SND_ASYNC` 播放期间不得释放的要求。
    let ok = unsafe {
        PlaySoundW(
            windows::core::PCWSTR(wave.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if ok.as_bool() {
        return;
    }

    // SAFETY: 无参数系统调用
    unsafe {
        let _ = MessageBeep(MB_OK);
    }
}

#[cfg(not(windows))]
pub fn play_sound(_custom_path: Option<&str>) {}

/// 任务栏闪烁。等价于旧版的 `requestUserAttention(Informational)`。
///
/// `FLASHW_TIMERNOFG` = 一直闪到窗口被切到前台为止;窗口已经在前台时这一调用
/// 自然什么都不做,不必自己再判一次焦点。
#[cfg(windows)]
pub fn flash_taskbar(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
    };

    // gpui 的 `Window` 上有一个同名的固有方法(返回 AnyWindowHandle),
    // 必须显式走 trait 才能拿到平台句柄。
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: info 是栈上完整初始化的结构,hwnd 来自当前进程的活窗口
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

#[cfg(not(windows))]
pub fn flash_taskbar(_window: &gpui::Window) {}

#[cfg(test)]
mod wave_tests {
    use super::*;

    fn samples(wave: &[u8]) -> Vec<i16> {
        wave[WAV_HEADER_LEN..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// 44 字节 RIFF 头逐字段对账 —— 头写错的表现是「一声都不响」,且没有任何日志。
    #[test]
    fn wav_头是合法的单声道_16bit_44k() {
        let wave = build_notification_wave();
        let data_len = (wave.len() - WAV_HEADER_LEN) as u32;
        assert_eq!(&wave[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(wave[4..8].try_into().unwrap()),
            36 + data_len
        );
        assert_eq!(&wave[8..12], b"WAVE");
        assert_eq!(&wave[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wave[16..20].try_into().unwrap()), 16);
        assert_eq!(
            u16::from_le_bytes(wave[20..22].try_into().unwrap()),
            1,
            "PCM"
        );
        assert_eq!(
            u16::from_le_bytes(wave[22..24].try_into().unwrap()),
            1,
            "单声道"
        );
        assert_eq!(u32::from_le_bytes(wave[24..28].try_into().unwrap()), 44_100);
        assert_eq!(
            u32::from_le_bytes(wave[28..32].try_into().unwrap()),
            88_200,
            "byte rate = 44100 × 2"
        );
        assert_eq!(
            u16::from_le_bytes(wave[32..34].try_into().unwrap()),
            2,
            "block align"
        );
        assert_eq!(
            u16::from_le_bytes(wave[34..36].try_into().unwrap()),
            16,
            "位深"
        );
        assert_eq!(&wave[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wave[40..44].try_into().unwrap()),
            data_len
        );
    }

    /// 总长 280ms = 12348 采样(0.28 × 44100),一个采样两字节。
    #[test]
    fn 采样数对应_280ms() {
        let wave = build_notification_wave();
        assert_eq!(samples(&wave).len(), 12_348);
        assert_eq!(wave.len(), WAV_HEADER_LEN + 12_348 * 2);
    }

    /// 两段之间那 10ms(0.12→0.13)必须是**真静默** —— 少了它两个音会连成一个滑音。
    #[test]
    fn 两段之间有_10ms_静默() {
        let s = samples(&build_notification_wave());
        let from = (0.12 * SAMPLE_RATE as f32) as usize;
        let to = (0.13 * SAMPLE_RATE as f32) as usize;
        assert_eq!(to - from, 441, "10ms @44.1kHz = 441 采样");
        assert!(s[from..to].iter().all(|&v| v == 0), "段间不是静默");
    }

    /// 包络:每段起头接近峰值 0.3,段尾衰减到 0.01 附近(指数衰减,不是硬切)。
    #[test]
    fn 每段包络从峰值指数衰减() {
        let s = samples(&build_notification_wave());
        let peak = (PEAK_GAIN * i16::MAX as f32) as i32; // ≈ 9830
        for (label, start, end) in [("段1", 0.0f32, 0.12f32), ("段2", 0.13, 0.28)] {
            let from = (start * SAMPLE_RATE as f32) as usize;
            let to = (end * SAMPLE_RATE as f32) as usize;
            // 起头 10ms 内必然扫过一整个正弦周期(880/660Hz 周期都 < 1.6ms),
            // 所以峰值取得到
            let head = s[from..from + 441]
                .iter()
                .map(|v| (*v as i32).abs())
                .max()
                .unwrap();
            assert!(
                (head - peak).abs() < peak / 10,
                "{label} 起头应接近峰值 {peak},实测 {head}"
            );
            // 段尾 5ms:包络已落到 ~0.012,幅值不该超过峰值的 5%
            let tail = s[to - 220..to]
                .iter()
                .map(|v| (*v as i32).abs())
                .max()
                .unwrap();
            assert!(tail < peak / 20, "{label} 段尾应已衰减,实测 {tail}");
        }
    }

    /// 频率对账:数过零次数反推基频 —— 880Hz 与 660Hz 差一个纯四度,
    /// 写反了听感完全不同,而这是唯一能在无声环境里验的判据。
    #[test]
    fn 两段基频分别是_880_与_660() {
        let s = samples(&build_notification_wave());
        // 只数每段起头 60ms:再往后包络压到量化噪声里,过零会数不准
        for (freq, start) in [(880.0f32, 0.0f32), (660.0, 0.13)] {
            let from = (start * SAMPLE_RATE as f32) as usize;
            let window = (0.06 * SAMPLE_RATE as f32) as usize;
            let seg = &s[from..from + window];
            let crossings = seg
                .windows(2)
                .filter(|w| (w[0] >= 0) != (w[1] >= 0))
                .count();
            let expected = (freq * 0.06 * 2.0).round() as usize;
            assert!(
                crossings.abs_diff(expected) <= 2,
                "{freq}Hz 段过零应约 {expected} 次,实测 {crossings}"
            );
        }
    }

    /// 合成结果进程内只算一次并长活(`SND_ASYNC` 要求缓冲区在播放期间不被释放)。
    #[test]
    fn 波形缓冲是同一块内存() {
        let a = notification_wave();
        let b = notification_wave();
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a, build_notification_wave().as_slice());
    }
}

#[cfg(test)]
mod toast_kind_tests {
    use super::*;

    /// 图标字符 / 跳转语义 / 正文来源三张表,逐 kind 对着
    /// `ToastContainer.tsx:42-63` 钉死。
    #[test]
    fn 五种_kind_的图标与点击语义() {
        use ToastKind::*;
        assert_eq!(Completion.icon_char(), "✓");
        assert_eq!(Attention.icon_char(), "!");
        assert_eq!(PasteError.icon_char(), "!");
        assert_eq!(WslInfo.icon_char(), "i");
        assert_eq!(MobileSession.icon_char(), "i");

        assert!(Completion.jumps_to_project());
        assert!(Attention.jumps_to_project());
        assert!(MobileSession.jumps_to_project());
        assert!(
            !WslInfo.jumps_to_project(),
            "wsl-info 的 projectId 是占位串"
        );
        assert!(!PasteError.jumps_to_project(), "粘贴失败的项目就在眼前");

        assert!(!Completion.uses_message());
        assert!(!Attention.uses_message());
        assert!(WslInfo.uses_message());
        assert!(MobileSession.uses_message());
        assert!(PasteError.uses_message());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> NotifyPrefs {
        NotifyPrefs {
            sound: true,
            flash: true,
            popup: true,
            attention_notify: true,
        }
    }

    fn transition<'a>(
        old: PaneStatus,
        new: PaneStatus,
        cause: Option<&'a str>,
    ) -> StatusTransition<'a> {
        StatusTransition {
            pane_id: "pane-1",
            old_status: old,
            new_status: new,
            old_attention: false,
            cause,
            window_focused: false,
            project_active: false,
        }
    }

    /// 完成判据:只有下降沿 + (无成因 | Stop) 才算完成。
    #[test]
    fn 完成判据只认下降沿与_stop() {
        use PaneStatus::*;
        assert!(is_completion(AiWorking, AiIdle, None), "无 hook 的降级路径必须放行");
        assert!(is_completion(AiWorking, AiIdle, Some("Stop")));
        assert!(!is_completion(AiWorking, AiIdle, Some("StopFailure")));
        assert!(!is_completion(AiWorking, AiIdle, Some("PermissionRequest")));
        assert!(!is_completion(AiWorking, AiIdle, Some("Stall")), "停摆兜底不是完成");
        assert!(!is_completion(AiWorking, AiIdle, Some("Interrupt")), "用户打断不是完成");
        assert!(!is_completion(AiIdle, AiIdle, Some("Stop")), "没有下降沿不算");
        assert!(!is_completion(AiWorking, Idle, None));
    }

    /// 待确认提醒只认上升沿:黄灯已亮时再来同类事件不再响。
    #[test]
    fn 待确认提醒只响上升沿() {
        assert!(is_attention_rise(false, Some("PermissionRequest")));
        assert!(!is_attention_rise(true, Some("PermissionRequest")));
        assert!(!is_attention_rise(false, Some("Stop")));
        assert!(!is_attention_rise(false, None));
    }

    #[test]
    fn 完成时按开关给出三通道提醒() {
        let mut tracker = DoneTracker::default();
        let plan = tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        assert!(plan.sound && plan.flash);
        assert_eq!(plan.toast, Some(ToastKind::Completion));
        assert!(plan.mark_needs_attention);
        assert_eq!(tracker.unread_count(), 1, "窗口没聚焦 → 计未读");
        assert!(tracker.order().contains_key("pane-1"));
    }

    /// 激活项目里的完成不弹 toast(就在眼前),但提示音照响。
    #[test]
    fn 激活项目的完成不弹_toast() {
        let mut tracker = DoneTracker::default();
        let mut t = transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop"));
        t.project_active = true;
        let plan = tracker.apply(&t, &prefs());
        assert!(plan.sound);
        assert_eq!(plan.toast, None);
        assert!(!plan.mark_needs_attention);
    }

    /// 窗口聚焦时完成不计未读,但完成序号照记(两份口径不同)。
    #[test]
    fn 窗口聚焦时完成不计未读但照样排队() {
        let mut tracker = DoneTracker::default();
        let mut t = transition(PaneStatus::AiWorking, PaneStatus::AiIdle, None);
        t.window_focused = true;
        tracker.apply(&t, &prefs());
        assert_eq!(tracker.unread_count(), 0);
        assert!(tracker.order().contains_key("pane-1"));
    }

    /// 转入待确认时旧的「完成未读」作废 —— 同一 pane 不许黄绿双计。
    #[test]
    fn 转入待确认撤销完成未读() {
        let mut tracker = DoneTracker::default();
        tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        assert_eq!(tracker.unread_count(), 1);

        let plan = tracker.apply(
            &transition(PaneStatus::AiIdle, PaneStatus::AiIdle, Some("PermissionRequest")),
            &prefs(),
        );
        assert_eq!(tracker.unread_count(), 0);
        assert!(tracker.order().is_empty(), "待确认同样撤出完成排队");
        assert_eq!(plan.toast, Some(ToastKind::Attention));
        assert!(!plan.mark_needs_attention, "待确认不点项目行的完成标");
    }

    /// 又开始干活 → 撤出完成排队(否则状态灯会往一个正在跑的 pane 上跳)。
    #[test]
    fn 重新开工撤出完成排队() {
        let mut tracker = DoneTracker::default();
        tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        tracker.apply(
            &transition(PaneStatus::AiIdle, PaneStatus::AiWorking, Some("UserPromptSubmit")),
            &prefs(),
        );
        assert!(tracker.order().is_empty());
    }

    /// 同一次任务的多个 Stop 不重新发号。
    #[test]
    fn 重复_stop_不改完成序号() {
        let mut tracker = DoneTracker::default();
        let t = transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop"));
        tracker.apply(&t, &prefs());
        let first = tracker.order()["pane-1"];
        // 第二条 Stop:已经是 ai-idle 了,没有下降沿,但 cause 仍是权威完成信号
        tracker.apply(
            &transition(PaneStatus::AiIdle, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        assert_eq!(tracker.order()["pane-1"], first);
    }

    #[test]
    fn 开关关掉后不发提醒() {
        let mut tracker = DoneTracker::default();
        let prefs = NotifyPrefs {
            sound: false,
            flash: false,
            popup: false,
            attention_notify: false,
        };
        let plan = tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs,
        );
        assert!(!plan.sound && !plan.flash && plan.toast.is_none());
        assert!(plan.mark_needs_attention, "项目行的完成标不受通知开关管辖");
        assert_eq!(tracker.unread_count(), 1, "记账与提醒是两件事");
    }

    /// 窗口聚焦时「未读」清空,但完成排队**不动** —— 两份口径本来就不同,
    /// 顺手把 order 也清了的话「跳到下一件待办」会一下子没了目标。
    #[test]
    fn 聚焦清未读不动完成排队() {
        let mut tracker = DoneTracker::default();
        tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        assert_eq!(tracker.unread_count(), 1);
        tracker.clear_unread();
        assert_eq!(tracker.unread_count(), 0);
        assert!(!tracker.is_unread("pane-1"));
        assert!(tracker.order().contains_key("pane-1"));
    }

    #[test]
    fn 关掉的_pane_撤出两份队列() {
        let mut tracker = DoneTracker::default();
        tracker.apply(
            &transition(PaneStatus::AiWorking, PaneStatus::AiIdle, Some("Stop")),
            &prefs(),
        );
        tracker.retain_panes(&HashSet::new());
        assert_eq!(tracker.unread_count(), 0);
        assert!(tracker.order().is_empty());
    }

    #[test]
    fn 挑目标按待确认_完成_处理中排序() {
        let mut order = HashMap::new();
        order.insert("p-done-late".to_string(), 9u64);
        order.insert("p-done-early".to_string(), 2u64);

        let panes = || {
            vec![
                PaneRef {
                    project_id: "proj-a",
                    pane_id: "p-working",
                    status: PaneStatus::AiWorking,
                    attention: false,
                },
                PaneRef {
                    project_id: "proj-a",
                    pane_id: "p-done-late",
                    status: PaneStatus::AiIdle,
                    attention: false,
                },
                PaneRef {
                    project_id: "proj-b",
                    pane_id: "p-done-early",
                    status: PaneStatus::AiIdle,
                    attention: false,
                },
            ]
        };

        // 没有待确认 → 取最先完成的那个
        assert_eq!(
            pick_attention_target(panes(), &order),
            Some(("proj-b".into(), "p-done-early".into()))
        );

        // 有待确认 → 待确认优先
        let mut with_attention = panes();
        with_attention.push(PaneRef {
            project_id: "proj-c",
            pane_id: "p-attention",
            status: PaneStatus::AiIdle,
            attention: true,
        });
        assert_eq!(
            pick_attention_target(with_attention, &order),
            Some(("proj-c".into(), "p-attention".into()))
        );

        // 只剩处理中 → 回落处理中
        assert_eq!(
            pick_attention_target(
                vec![PaneRef {
                    project_id: "proj-a",
                    pane_id: "p-working",
                    status: PaneStatus::AiWorking,
                    attention: false,
                }],
                &HashMap::new()
            ),
            Some(("proj-a".into(), "p-working".into()))
        );

        // 全空闲 → 没有目标
        assert_eq!(
            pick_attention_target(
                vec![PaneRef {
                    project_id: "proj-a",
                    pane_id: "p-idle",
                    status: PaneStatus::Idle,
                    attention: false,
                }],
                &HashMap::new()
            ),
            None
        );
    }
}
