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
//! 与 mt-ui 侧闸的默认值一致。平台支持现状见 AGENTS.md。
//!
//! # 刷新时机
//!
//! 启动探测一次([`install`]);此外**窗口每次重新激活**再探一次
//! ([`refresh`])—— 用户去系统设置里改这个开关,回到本窗口时就生效了,
//! 不必重启。gpui 没有 `WM_SETTINGCHANGE` 的转发口,而这次探测是一次
//! 纯内存的系统调用(微秒级),挂在激活事件上比自建消息窗划算得多。
//!
//! # 还有第二道闸:gpui 自己那个,必须由我们顶住(见 [`gate_values`])
//!
//! gpui 0.2.2 时代没有「减弱动效」这个概念,一次性过渡该不该播**全由本仓
//! 决定**。gpui-pre 0.3.5 给 `App` 加了 [`gpui::App::reduce_motion`],并让
//! `AnimationElement` **无条件**读它:置位时 oneshot 动画直接按终态渲染一帧、
//! 一帧都不再请求(`gpui-pre-0.3.5/src/elements/animation.rs:407-414`)。
//! 而 `gpui_component::init` → `gpui_base::init` → `reduce_motion::init`
//! (`gpui-base-0.6.2/src/reduce_motion.rs:48-50,125-139`)启动时就拿
//! `SPI_GETCLIENTAREAANIMATION` 把系统口径写了进去 —— **与本模块问的是同一个
//! 系统开关**。于是在「动画效果」关着的机器上,全仓所有 `with_animation`
//! (抽屉滑入滑出 / 面板换场 / 抽屉页签指示条 / 切终端的整幅推移)一律瞬间跳变,
//! 与本仓「转场照播、闪烁全停」的口径正相反。
//!
//! 上游给了应用接管这道闸的正式口子:「An application owns the flag once it
//! sets it」(同文件 :24-28)—— `apply_preference` 发现闸里的值不是 Base 上次
//! 写进去的那个就原样退出(:89-101)。所以启动时把它顶成 `false` 即可一劳永逸,
//! 此后系统口径只落在本模块这道闸上,豁免与否逐条由
//! [`mt_ui::motion::TransitionSpec::respects_reduce`] 说了算
//! (走 [`mt_ui::motion::Transition`] 的那些由 spec 兜住;toast 走
//! `with_animation`,它自己在调用点判 [`mt_ui::motion::reduce_motion`],
//! reduce 时连动画元素都不挂 —— 见 `toast.rs` 的注释)。
//!
//! ⚠️ 这不是「无视用户的无障碍设置」:豁免面本来就是原版
//! `styles.css:415-421` 逐条点名的,理由也在那儿 —— 把视觉效果调成
//! 「最佳性能」的人要的是别卡,不是「界面凭空跳变」。

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

/// 一次探测该写进**两道闸**的值:`(mt-ui 那道, gpui 那道)`。
///
/// - mt-ui 那道跟随系统:它是逐条动画的判据,豁免与否由各自的
///   `respects_reduce` 说了算;
/// - gpui 那道**只跟用户的「启用动画」总开关**(`!animations_enabled`),与系统无关:
///   它是个「所有 `with_animation` 一刀切按终态画一帧」的闸
///   (`gpui-pre-0.3.5/src/elements/animation.rs:407-414`),粒度粗到无法表达本仓的
///   豁免面 —— 系统口径进它一次,转场就全没了(理由详见模块注释)。用户亲手关掉
///   「启用动画」时要的恰恰是「一刀切」,所以总开关可以、也只有它可以拧这道闸
///   (2026-11-21 用户拍板「启用动画管全部动画」,组件库内部的动画一并停)。
///
/// 单独拎成纯函数是为了钉住这条口径:日后谁把系统值顺手转发给 gpui 那道闸,
/// 下面那个用例会红。
pub(crate) fn gate_values(system_reduce: bool, animations_enabled: bool) -> (bool, bool) {
    (system_reduce, !animations_enabled)
}

/// 启动时探测一次并写进两道闸(总开关先按缺省「开」,配置读出来后由
/// [`sync_switch`] 校正)。返回系统口径(= mt-ui 那道闸的值)。
///
/// **必须排在 `gpui_component::init` 之后**:接管 gpui 那道闸的前提是
/// Base 已经把系统值写过一遍(`apply_preference` 靠「闸里还是不是我上次写的值」
/// 判断应用有没有接管,反过来会被它随后的 init 覆盖回去)。
pub fn install(cx: &mut gpui::App) -> bool {
    let (reduce, gpui_reduce) = gate_values(probe(), mt_ui::motion::animations_enabled());
    mt_ui::motion::set_reduce_motion(reduce);
    cx.set_reduce_motion(gpui_reduce);
    reduce
}

/// 把设置页「启用动画」(`terminalAnimations`,缺省开)同步进两处:mt-ui 的总开关与
/// gpui 那道闸。**返回是否发生了变化**(变了调用方要刷窗口,已经画出来的那一帧
/// 不会自己更新)。
///
/// 调用点:配置加载后一次(`AppStore` 建好时)、每次 `patch_config` 之后、窗口
/// 重新激活时([`refresh`] 里顺手重按 gpui 那道 —— Linux 上 Base 会跟着门户信号
/// 改写它,值碰巧与 Base 上次写的相同时它认为自己仍持有这道闸)。
pub fn sync_switch(enabled: bool, cx: &mut gpui::App) -> bool {
    let changed = mt_ui::motion::set_animations_enabled(enabled);
    let (_, gpui_reduce) = gate_values(mt_ui::motion::reduce_motion(), enabled);
    if cx.reduce_motion() != gpui_reduce {
        cx.set_reduce_motion(gpui_reduce);
        return true;
    }
    changed
}

/// 重新探测。**返回是否发生了变化** —— 变了的话调用方要刷一遍窗口,
/// 否则已经画出来的那一帧不会自己更新。
///
/// mt-ui 那道跟系统;gpui 那道按当前总开关重按一遍(见 [`sync_switch`])。
pub fn refresh(cx: &mut gpui::App) -> bool {
    let reduce_changed = mt_ui::motion::set_reduce_motion(probe());
    let switch_changed = sync_switch(mt_ui::motion::animations_enabled(), cx);
    reduce_changed || switch_changed
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

    /// 系统口径只许落在 mt-ui 那道闸上;gpui 那道只跟用户的总开关。
    ///
    /// 把系统值转发给 gpui 的后果不是「少播几个动画」,而是**所有**
    /// `with_animation` 一刀切按终态画一帧(抽屉、面板换场、页签指示条、
    /// 切终端的整幅推移全部瞬间跳变)—— 本仓的豁免面在那道闸上无从表达。
    #[test]
    fn gpui_那道闸不跟系统只跟总开关() {
        assert_eq!(
            gate_values(true, true),
            (true, false),
            "系统要求减少时转场仍须照播"
        );
        assert_eq!(gate_values(false, true), (false, false));
        // 用户亲手关掉「启用动画」:gpui 那道一刀切,组件库内部的动画一并停
        assert_eq!(gate_values(false, false), (false, true));
        assert_eq!(gate_values(true, false), (true, true));
    }

    /// 两道闸的语义必须对得上:mt-ui 那道置位时,**豁免类**过渡照跑不误 ——
    /// 这正是 gpui 那道闸不能跟着系统走的原因(它对豁免面一视同仁)。
    #[test]
    fn 减弱动效下豁免类过渡仍在跑() {
        use std::time::Duration;
        with_reduce(true, || {
            let half = Duration::from_millis(100);
            for exempt in [
                mt_ui::motion::OVERLAY_IN,
                mt_ui::motion::TERMINAL_SWAP,
                mt_ui::motion::TAB_INDICATOR,
                mt_ui::motion::PANE_ENTER,
                mt_ui::motion::TOAST_SLIDE_IN,
            ] {
                assert!(
                    exempt.running_at(half, mt_ui::motion::reduce_motion()),
                    "{exempt:?} 在减弱动效下也该继续跑"
                );
                assert!(exempt.progress_at(half, mt_ui::motion::reduce_motion()) < 1.0);
            }
            // 对照组:不在豁免名单里的照旧直达终态
            assert!(!mt_ui::motion::TAG_FADE_IN.running_at(half, mt_ui::motion::reduce_motion()));
        });
    }
}
