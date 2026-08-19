//! 「减少动画」的系统探测。闸本体在 [`mt_ui::motion`],这里只负责**问系统**
//! 并把结论写进去。
//!
//! # 为什么是 `SPI_GETCLIENTAREAANIMATION`
//!
//! 原版是 WebView2 里的 `@media (prefers-reduced-motion: reduce)`。Chromium 在
//! Windows 上判定这条媒体查询用的就是 `SystemParametersInfoW` 的
//! `SPI_GETCLIENTAREAANIMATION`(设置 → 辅助功能 → 视觉效果 → **动画效果**;
//! 老控制面板里叫「显示 Windows 内的动画」)。所以问同一个开关 = 与装机版
//! 落在同一个分支上,这正是本仓要的:用户机器上那两个版本必须表现一致。
//!
//! 返回值语义**是反的**:`TRUE` = 系统允许动画 → `reduce_motion = false`。
//!
//! # 非 Windows
//!
//! macOS 有 `NSWorkspace.accessibilityDisplayShouldReduceMotion`、Linux 上是
//! `org.gnome.desktop.interface enable-animations` 这类桌面环境私有设置,
//! 两边都还没接 —— [`probe`] 在那些平台恒返回 `false`(= 不减少动画),
//! 与 mt-ui 侧闸的默认值一致。平台支持现状见 CLAUDE.md。
//!
//! # 刷新时机
//!
//! 启动探测一次([`install`]);此外**窗口每次重新激活**再探一次
//! ([`refresh`])—— 用户去系统设置里改这个开关,回到本窗口时就生效了,
//! 不必重启。gpui 没有 `WM_SETTINGCHANGE` 的转发口,而这次探测是一次
//! 纯内存的系统调用(微秒级),挂在激活事件上比自建消息窗划算得多。

/// 探测系统当前是否要求减少动画。
pub fn probe() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };

        // BOOL(i32):TRUE = 客户区动画开着 = 用户**不**要求减少动画
        let mut animations_on: i32 = 1;
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(&mut animations_on as *mut i32 as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // 调用失败按「不减少」处理:宁可多播动画,也别让整个界面无声跳变
        if ok.is_err() {
            return false;
        }
        animations_on == 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 启动时探测一次并写进闸。返回探测结果。
pub fn install() -> bool {
    let reduce = probe();
    mt_ui::motion::set_reduce_motion(reduce);
    reduce
}

/// 重新探测。**返回是否发生了变化** —— 变了的话调用方要刷一遍窗口,
/// 否则已经画出来的那一帧不会自己更新。
pub fn refresh() -> bool {
    mt_ui::motion::set_reduce_motion(probe())
}

/// 闸是进程级全局量,**本 crate 里所有要动闸的用例统一走这个夹具**
/// (同一把锁串行化;各模块自己造一把就白搭了)。
#[cfg(test)]
pub(crate) fn with_reduce<R>(on: bool, f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = mt_ui::motion::reduce_motion();
    mt_ui::motion::set_reduce_motion(on);
    let out = f();
    mt_ui::motion::set_reduce_motion(prev);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测只读不写,重复调用必须稳定(它会挂在窗口激活事件上,一天跑几十次)。
    #[test]
    fn 探测是纯查询且可重入() {
        let first = probe();
        for _ in 0..5 {
            assert_eq!(probe(), first, "同一环境下探测结果不该跳");
        }
        // 探测本身不许改闸 —— 写入是 install/refresh 的事
        let gate = mt_ui::motion::reduce_motion();
        probe();
        assert_eq!(mt_ui::motion::reduce_motion(), gate);
    }

    /// 非 Windows 恒「不减少」,与 mt-ui 侧闸的默认值一致。
    #[test]
    #[cfg(not(windows))]
    fn 非_windows_平台恒不减少() {
        assert!(!probe());
    }
}
