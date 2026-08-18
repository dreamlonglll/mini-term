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

/// toast 的两种口径,与旧版 `Notification.kind` 同集。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// AI 任务完成。
    Completion,
    /// AI 停下来等你批权限 / 填表单 / 这轮因 API 错误结束。
    Attention,
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

// ─── 平台提醒 ────────────────────────────────────────────────

/// 提示音。自定义路径只认 `.wav`(`PlaySoundW` 的能力边界),其余一律回落系统音。
///
/// 旧版走浏览器 `Audio`,mp3/ogg 都能放;这里是 Win32 直调,格式支持窄一档,
/// 已记入交付说明的偏差清单。
#[cfg(windows)]
pub fn play_sound(custom_path: Option<&str>) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
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
        // 放不出来(文件没了 / 格式不认)时退回系统音,而不是静默什么都不响
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
