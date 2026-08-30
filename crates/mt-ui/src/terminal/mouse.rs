//! 鼠标上报：把点击 / 拖动 / 移动 / 滚轮翻成终端认识的转义序列。
//!
//! xterm.js 白送的又一块。TUI 程序（vim / htop / lazygit / fzf / Claude Code 的
//! 选择菜单）靠这条通道拿鼠标，没有它就只能敲键盘。
//!
//! # 三种「报什么」的模式（DECSET 1000 / 1002 / 1003）
//!
//! | TermMode | DECSET | 报什么 |
//! |---|---|---|
//! | `MOUSE_REPORT_CLICK` | 1000 | 只报按下与松开 |
//! | `MOUSE_DRAG`         | 1002 | 按下松开 + **按住时**的移动 |
//! | `MOUSE_MOTION`       | 1003 | 按下松开 + 一切移动（不按也报） |
//!
//! 三者互斥（alacritty 在 set_private_mode 里先 remove 掉 `MOUSE_MODE` 再置位），
//! 但这里按「位」判定而不假设互斥 —— 判据写成 `intersects` 更耐改。
//!
//! # 两种「怎么编码」（DECSET 1005 / 1006）
//!
//! - **默认（X10 兼容）**：`ESC [ M Cb Cx Cy`，三个字节各加 32。坐标因此封顶在
//!   223 列/行 —— 超出直接**不报**（xterm 同样处理；报个错位的坐标比不报更糟）。
//! - **`UTF8_MOUSE`（1005）**：同上，但 Cx/Cy 用 UTF-8 编码，突破 223。
//!   注意 Cb 也按 UTF-8 走（值不会超过 127，实际就是单字节）。
//! - **`SGR_MOUSE`（1006）**：`ESC [ < Cb ; Cx ; Cy M`（松开用小写 `m`），
//!   十进制无上限，且**松开事件保留真实按键号** —— 前两种编码的松开一律是 3，
//!   程序分不清松的是哪个键。1006 是现在的事实标准，优先级也最高。
//!
//! # 按键码（Cb）的位布局
//!
//! ```text
//! bit 0-1  按键号：0=左 1=中 2=右 3=松开(X10)/无按键(移动)
//! bit 2    Shift  (+4)
//! bit 3    Alt    (+8)
//! bit 4    Ctrl   (+16)
//! bit 5    移动   (+32)
//! bit 6    滚轮   (+64)：64=上 65=下 66=左 67=右
//! bit 7    扩展键 (+128)：按键 8~11
//! ```
//!
//! # 本地选择怎么让位（对照 xterm.js）
//!
//! 上报模式开着的时候，左键拖动属于**程序**，不再是本地框选。唯一的逃生门是
//! **按住 Shift**：xterm / xterm.js / Windows Terminal 一致约定 Shift 强制本地
//! 行为。所以这里的闸门是 `mods.shift ⇒ 一律不上报`，元素侧对应
//! 「不上报 ⇒ 走本地选择」，两边只有这一个判据，不会出现「两边都做」或「都不做」。

use alacritty_terminal::term::TermMode;

use super::input::{Arrow, arrow_bytes};

/// 鼠标按键。`Other` 是 4 号以上的侧键（bit 7 那一族）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseBtn {
    Left,
    Middle,
    Right,
    Other(u8),
}

impl MouseBtn {
    /// 按键号（未加修饰位）。返回 `None` 表示这个键编码不出来。
    fn code(self) -> Option<u8> {
        match self {
            MouseBtn::Left => Some(0),
            MouseBtn::Middle => Some(1),
            MouseBtn::Right => Some(2),
            // 按键 8~11 走 bit 7；再往上没有编码位
            MouseBtn::Other(n) if (8..=11).contains(&n) => Some(128 + (n - 8)),
            MouseBtn::Other(_) => None,
        }
    }
}

/// 滚轮方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelDir {
    Up,
    Down,
    Left,
    Right,
}

impl WheelDir {
    fn code(self) -> u8 {
        64 + match self {
            WheelDir::Up => 0,
            WheelDir::Down => 1,
            WheelDir::Left => 2,
            WheelDir::Right => 3,
        }
    }
}

/// 一个已归一化的鼠标动作。刻意不带 gpui 类型 —— 编码矩阵可以纯逻辑单测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseBtn),
    Release(MouseBtn),
    /// 移动。`Some(btn)` = 按住某键拖动，`None` = 空手移动。
    Motion(Option<MouseBtn>),
    /// 滚轮。终端协议里滚轮只有「按下」没有「松开」。
    Wheel(WheelDir),
}

/// 修饰键。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl MouseMods {
    pub fn new(shift: bool, alt: bool, control: bool) -> Self {
        Self {
            shift,
            alt,
            control,
        }
    }

    fn bits(self) -> u8 {
        // shift 位留着是为了编码完整；实际上 shift 会在闸门处被拦成本地选择，
        // 走不到这里（除非将来加一个「shift 也上报」的配置项）。
        (if self.shift { 4 } else { 0 })
            | (if self.alt { 8 } else { 0 })
            | (if self.control { 16 } else { 0 })
    }
}

/// grid 坐标，**0 基**，行号是可视区行号（不是 alacritty 的 `Line`）。
/// 协议里是 1 基，转换在编码函数内部做，调用方不必记这条。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPos {
    pub col: usize,
    pub row: usize,
}

impl GridPos {
    pub fn new(col: usize, row: usize) -> Self {
        Self { col, row }
    }
}

/// 当前是否有任何一种鼠标上报模式开着。
///
/// **元素侧靠它决定「这次点击是本地选择还是上报」**，所以它必须与
/// [`mouse_report_bytes`] 的闸门口径完全一致 —— 两者共用同一个判据函数。
pub fn mouse_reporting_active(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// 这次动作应该走本地（选择 / 回看滚动）而不是上报吗。
///
/// 唯一的额外规则是 Shift 强制本地，见模块注释。
pub fn prefers_local_handling(mode: TermMode, mods: MouseMods) -> bool {
    !mouse_reporting_active(mode) || mods.shift
}

/// 把一次鼠标动作编成要写进 PTY 的字节。`None` = 这次动作不该上报。
pub fn mouse_report_bytes(
    mode: TermMode,
    action: MouseAction,
    mods: MouseMods,
    pos: GridPos,
) -> Option<Vec<u8>> {
    if prefers_local_handling(mode, mods) {
        return None;
    }

    let (mut code, released) = match action {
        MouseAction::Press(btn) => (btn.code()?, false),
        MouseAction::Release(btn) => (btn.code()?, true),
        MouseAction::Motion(held) => {
            // 1002 只在按住时报，1003 什么都报
            let allowed = match held {
                Some(_) => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
                None => mode.contains(TermMode::MOUSE_MOTION),
            };
            if !allowed {
                return None;
            }
            // 空手移动的按键号是 3（与 X10 的「松开」同码，靠移动位区分）
            let base = match held {
                Some(btn) => btn.code()?,
                None => 3,
            };
            (base | 32, false)
        }
        MouseAction::Wheel(dir) => (dir.code(), false),
    };
    code |= mods.bits();

    // 协议是 1 基坐标
    let col = pos.col + 1;
    let row = pos.row + 1;

    if mode.contains(TermMode::SGR_MOUSE) {
        let final_byte = if released { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{col};{row}{final_byte}").into_bytes());
    }

    // X10 / UTF8：松开一律编成 3，修饰位保留
    let legacy_code = if released { (code & !3) | 3 } else { code };

    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b[M");
    if mode.contains(TermMode::UTF8_MOUSE) {
        push_utf8(&mut out, 32 + legacy_code as u32);
        push_utf8(&mut out, 32 + col as u32);
        push_utf8(&mut out, 32 + row as u32);
    } else {
        // 单字节封顶 255：坐标超过 223 就没法表达，宁可不报也不报错位的
        if col > 223 || row > 223 {
            return None;
        }
        out.push(32 + legacy_code);
        out.push((32 + col) as u8);
        out.push((32 + row) as u8);
    }
    Some(out)
}

/// 把一个码位按 UTF-8 追加进去。127 以内就是原字节（与 X10 兼容）。
fn push_utf8(out: &mut Vec<u8>, value: u32) {
    match char::from_u32(value) {
        Some(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        // 代理区等非法码位：退回单字节截断，绝不丢掉分隔结构
        None => out.push(value as u8),
    }
}

/// alt screen 下滚轮的等价按键序列（没有回看缓冲，滚轮当上下方向键使）。
///
/// 这是 xterm 的老约定，`less` / `vim` / `man` 全都靠它。`app_cursor` 是 DECCKM。
/// 序列本身走 [`super::input::arrow_bytes`] —— 此前这里硬编码着四个字面量，
/// 与 `keystroke_to_bytes` 各写一份，改 DECCKM 判据时波及不到对方。
pub fn alt_screen_scroll_bytes(lines: i32, app_cursor: bool) -> Vec<u8> {
    let dir = if lines > 0 { Arrow::Up } else { Arrow::Down };
    let seq = arrow_bytes(dir, app_cursor);
    let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
    for _ in 0..lines.unsigned_abs() {
        out.extend_from_slice(&seq);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLICK: TermMode = TermMode::MOUSE_REPORT_CLICK;

    fn sgr(mode: TermMode) -> TermMode {
        mode | TermMode::SGR_MOUSE
    }

    #[test]
    fn 关闭上报时一律不报() {
        assert!(!mouse_reporting_active(TermMode::empty()));
        assert_eq!(
            mouse_report_bytes(
                TermMode::empty(),
                MouseAction::Press(MouseBtn::Left),
                MouseMods::default(),
                GridPos::new(0, 0)
            ),
            None
        );
    }

    #[test]
    fn shift_强制本地选择() {
        let mods = MouseMods::new(true, false, false);
        assert!(prefers_local_handling(sgr(CLICK), mods));
        assert_eq!(
            mouse_report_bytes(
                sgr(CLICK),
                MouseAction::Press(MouseBtn::Left),
                mods,
                GridPos::new(3, 4)
            ),
            None
        );
        // 松开 Shift 就该报了
        assert!(mouse_report_bytes(
            sgr(CLICK),
            MouseAction::Press(MouseBtn::Left),
            MouseMods::default(),
            GridPos::new(3, 4)
        )
        .is_some());
    }

    #[test]
    fn sgr_按下与松开保留真实按键号() {
        let pos = GridPos::new(9, 4); // 0 基 → 协议里是 10, 5
        let press = mouse_report_bytes(
            sgr(CLICK),
            MouseAction::Press(MouseBtn::Right),
            MouseMods::default(),
            pos,
        )
        .unwrap();
        assert_eq!(press, b"\x1b[<2;10;5M".to_vec());

        let release = mouse_report_bytes(
            sgr(CLICK),
            MouseAction::Release(MouseBtn::Right),
            MouseMods::default(),
            pos,
        )
        .unwrap();
        // 小写 m 结尾，按键号仍是 2（X10 编码里这里会退化成 3）
        assert_eq!(release, b"\x1b[<2;10;5m".to_vec());
    }

    #[test]
    fn x10_松开退化成按键号3() {
        let pos = GridPos::new(0, 0);
        let press = mouse_report_bytes(
            CLICK,
            MouseAction::Press(MouseBtn::Middle),
            MouseMods::default(),
            pos,
        )
        .unwrap();
        assert_eq!(press, vec![0x1b, b'[', b'M', 32 + 1, 33, 33]);

        let release = mouse_report_bytes(
            CLICK,
            MouseAction::Release(MouseBtn::Middle),
            MouseMods::default(),
            pos,
        )
        .unwrap();
        assert_eq!(release, vec![0x1b, b'[', b'M', 32 + 3, 33, 33]);
    }

    #[test]
    fn 修饰键位_alt加8_ctrl加16() {
        let pos = GridPos::new(0, 0);
        let bytes = mouse_report_bytes(
            sgr(CLICK),
            MouseAction::Press(MouseBtn::Left),
            MouseMods::new(false, true, true),
            pos,
        )
        .unwrap();
        // 0（左键）+ 8（alt）+ 16（ctrl）= 24
        assert_eq!(bytes, b"\x1b[<24;1;1M".to_vec());
    }

    #[test]
    fn 拖动需要_1002_空手移动需要_1003() {
        let pos = GridPos::new(2, 2);
        let mods = MouseMods::default();

        // 1000：拖动与空手移动都不报
        assert_eq!(
            mouse_report_bytes(
                sgr(CLICK),
                MouseAction::Motion(Some(MouseBtn::Left)),
                mods,
                pos
            ),
            None
        );
        assert_eq!(
            mouse_report_bytes(sgr(CLICK), MouseAction::Motion(None), mods, pos),
            None
        );

        // 1002：拖动报，空手不报
        let drag_mode = sgr(TermMode::MOUSE_DRAG);
        assert_eq!(
            mouse_report_bytes(
                drag_mode,
                MouseAction::Motion(Some(MouseBtn::Left)),
                mods,
                pos
            )
            .unwrap(),
            // 0（左键）+ 32（移动位）= 32
            b"\x1b[<32;3;3M".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(drag_mode, MouseAction::Motion(None), mods, pos),
            None
        );

        // 1003：空手也报，按键号用 3
        let motion_mode = sgr(TermMode::MOUSE_MOTION);
        assert_eq!(
            mouse_report_bytes(motion_mode, MouseAction::Motion(None), mods, pos).unwrap(),
            // 3 + 32 = 35
            b"\x1b[<35;3;3M".to_vec()
        );
    }

    #[test]
    fn 滚轮编码_上64下65() {
        let pos = GridPos::new(0, 0);
        let mods = MouseMods::default();
        assert_eq!(
            mouse_report_bytes(sgr(CLICK), MouseAction::Wheel(WheelDir::Up), mods, pos).unwrap(),
            b"\x1b[<64;1;1M".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(sgr(CLICK), MouseAction::Wheel(WheelDir::Down), mods, pos).unwrap(),
            b"\x1b[<65;1;1M".to_vec()
        );
        // X10 编码下滚轮同样是 64/65 再加 32
        assert_eq!(
            mouse_report_bytes(CLICK, MouseAction::Wheel(WheelDir::Up), mods, pos).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 64, 33, 33]
        );
    }

    #[test]
    fn x10_坐标超过223不报_utf8与sgr能报() {
        let pos = GridPos::new(300, 250);
        let mods = MouseMods::default();
        let action = MouseAction::Press(MouseBtn::Left);

        assert_eq!(mouse_report_bytes(CLICK, action, mods, pos), None);

        let utf8 = mouse_report_bytes(CLICK | TermMode::UTF8_MOUSE, action, mods, pos).unwrap();
        assert_eq!(&utf8[..3], b"\x1b[M");
        // 32 + 0 = 32 是单字节；301 与 251 各自成多字节
        let tail: String = String::from_utf8(utf8[3..].to_vec()).unwrap();
        let chars: Vec<char> = tail.chars().collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[0] as u32, 32);
        assert_eq!(chars[1] as u32, 32 + 301);
        assert_eq!(chars[2] as u32, 32 + 251);

        assert_eq!(
            mouse_report_bytes(sgr(CLICK), action, mods, pos).unwrap(),
            b"\x1b[<0;301;251M".to_vec()
        );
    }

    #[test]
    fn sgr_优先于_utf8() {
        // 两个都开的时候按 SGR 走（1006 是后出的、更完整的那个）
        let mode = CLICK | TermMode::SGR_MOUSE | TermMode::UTF8_MOUSE;
        let bytes = mouse_report_bytes(
            mode,
            MouseAction::Press(MouseBtn::Left),
            MouseMods::default(),
            GridPos::new(0, 0),
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<0;1;1M".to_vec());
    }

    #[test]
    fn 编码不出来的侧键直接丢弃() {
        let pos = GridPos::new(0, 0);
        assert_eq!(
            mouse_report_bytes(
                sgr(CLICK),
                MouseAction::Press(MouseBtn::Other(20)),
                MouseMods::default(),
                pos
            ),
            None
        );
        // 8 号键有编码位（128 起）
        assert_eq!(
            mouse_report_bytes(
                sgr(CLICK),
                MouseAction::Press(MouseBtn::Other(8)),
                MouseMods::default(),
                pos
            )
            .unwrap(),
            b"\x1b[<128;1;1M".to_vec()
        );
    }

    #[test]
    fn alt_screen_滚轮翻成方向键() {
        assert_eq!(alt_screen_scroll_bytes(2, false), b"\x1b[A\x1b[A".to_vec());
        assert_eq!(alt_screen_scroll_bytes(-1, false), b"\x1b[B".to_vec());
        assert_eq!(alt_screen_scroll_bytes(1, true), b"\x1bOA".to_vec());
        assert_eq!(alt_screen_scroll_bytes(-2, true), b"\x1bOB\x1bOB".to_vec());
    }
}
