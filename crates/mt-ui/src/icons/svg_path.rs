//! SVG `path` 的 `d` 解析 + 离散化 —— 让「原样搬运官方 logo 的那条 path」成为可能。
//!
//! # 为什么需要它
//!
//! [`super::vector`] 那套形状 DSL(圆/矩形/折线)足够画自制图标,但**画不出**厂商
//! 官方 logo:那些是几百段贝塞尔的自由曲线。而 gpui 的三条 SVG 路子全被堵死
//! (判据见 `vector` 的模块注释),唯一能走的是「自己把 `d` 解析成点列,再喂
//! [`gpui::PathBuilder`]」—— 这样既拿到官方几何,又保住自绘那些好处
//! (无宿主接线、多色、几何是纯数据可单测)。
//!
//! # 支持到什么程度
//!
//! 完整的 SVG 1.1 path 文法:`M m L l H h V v C c S s Q q T t A a Z z`,含
//! 隐式重复(`M` 之后的坐标对按 `L` 处理)、`S`/`T` 的控制点反射、椭圆弧的
//! 端点→中心参数化。曲线**离散成折线**([`CURVE_SEGMENTS`] 段),13~16px 的
//! 图标上看不出折角,且离散结果是纯数据 —— 单测能直接断言。
//!
//! # 坐标口径
//!
//! 输入是 viewBox 里的原始坐标,输出统一归一到 **0..1 单位方框**(与形状 DSL
//! 同一口径)。viewBox 传 `(min_x, min_y, 边长)` —— 品牌 logo 无一例外是正方形
//! 画布,不做非等比拉伸。

use std::cell::RefCell;
use std::collections::HashMap;
use std::f32::consts::TAU;
use std::rc::Rc;

/// 一条子路径:单位方框内的点列 + 是否由 `Z` 闭合。
///
/// 多子路径是**必须**的 —— 官方 logo 的洞(OpenCode 的内框、Copilot 的眼睛)
/// 全靠「外框 + 内框两条子路径 + 填充规则」表达,拆成两个 `Shape` 会把洞画成实心。
pub type SubPath = (Vec<(f32, f32)>, bool);

/// 一段贝塞尔离散成多少折线段。
///
/// 与 [`super::vector::ARC_SEGMENTS`] 的 48 对齐:一个整圆是 4 段三次贝塞尔,
/// 4 × 12 = 48,两边的圆看起来一样圆。
pub const CURVE_SEGMENTS: usize = 12;

/// 解析 + 离散化,结果带缓存。
///
/// `d` 取 `&'static str`(形状表都是 const),缓存 key 直接用「指针 + 长度」——
/// 每帧对一条 2KB 的 path 做内容哈希是白费,而同一枚图标每帧拿到的必然是同一个
/// 字符串常量。解析本身几百段曲线,不缓存的话满屏图标每帧都要重算一遍。
pub fn cached(d: &'static str, view: (f32, f32, f32)) -> Rc<Vec<SubPath>> {
    /// key 是 `&'static str` 的(指针, 长度)。
    type Cache = RefCell<HashMap<(usize, usize), Rc<Vec<SubPath>>>>;
    thread_local! {
        static CACHE: Cache = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        let key = (d.as_ptr() as usize, d.len());
        if let Some(hit) = cache.borrow().get(&key) {
            return hit.clone();
        }
        let parsed = Rc::new(parse(d, view));
        cache.borrow_mut().insert(key, parsed.clone());
        parsed
    })
}

/// 解析 `d` 并归一到单位方框。语法错误就地截断 —— 形状表是 const 数据,
/// 写错了会被单测的「点全在单位方框内」逮住,运行时没有必要 panic。
pub fn parse(d: &str, view: (f32, f32, f32)) -> Vec<SubPath> {
    let mut out: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut closed: Vec<bool> = Vec::new();
    let mut pts: Vec<(f32, f32)> = Vec::new();
    let mut lex = Lexer::new(d);
    let mut cur = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    // `S`/`T` 要反射「上一条同族曲线」的控制点;上一条不是同族就退化成当前点
    let mut last_cubic: Option<(f32, f32)> = None;
    let mut last_quad: Option<(f32, f32)> = None;
    let mut cmd = 0u8;

    loop {
        match lex.take_cmd() {
            Some(c) => cmd = c,
            None => {
                if !lex.has_number() {
                    break;
                }
                // 隐式重复:命令字母只写一次,后面跟几组参数就画几段。
                // `M`/`m` 的重复按 `L`/`l` 算(SVG 1.1 8.3.2)
                cmd = match cmd {
                    b'M' => b'L',
                    b'm' => b'l',
                    0 => break,
                    other => other,
                };
            }
        }

        // 大小写只区分「绝对 / 相对」,语义一律看大写那个字母
        let rel = cmd.is_ascii_lowercase();
        let upper = cmd.to_ascii_uppercase();
        match upper {
            b'M' => {
                let Some(to) = lex.point(cur, rel) else { break };
                flush(&mut out, &mut closed, &mut pts, false);
                cur = to;
                start = to;
                pts.push(to);
                last_cubic = None;
                last_quad = None;
            }
            b'L' => {
                let Some(to) = lex.point(cur, rel) else { break };
                cur = to;
                pts.push(to);
                last_cubic = None;
                last_quad = None;
            }
            b'H' => {
                let Some(x) = lex.number() else { break };
                cur = (if rel { cur.0 + x } else { x }, cur.1);
                pts.push(cur);
                last_cubic = None;
                last_quad = None;
            }
            b'V' => {
                let Some(y) = lex.number() else { break };
                cur = (cur.0, if rel { cur.1 + y } else { y });
                pts.push(cur);
                last_cubic = None;
                last_quad = None;
            }
            b'C' | b'S' => {
                let c1 = if upper == b'C' {
                    match lex.point(cur, rel) {
                        Some(p) => p,
                        None => break,
                    }
                } else {
                    reflect(cur, last_cubic)
                };
                let (Some(c2), Some(to)) = (lex.point(cur, rel), lex.point(cur, rel)) else {
                    break;
                };
                push_cubic(&mut pts, cur, c1, c2, to);
                cur = to;
                last_cubic = Some(c2);
                last_quad = None;
            }
            b'Q' | b'T' => {
                let c = if upper == b'Q' {
                    match lex.point(cur, rel) {
                        Some(p) => p,
                        None => break,
                    }
                } else {
                    reflect(cur, last_quad)
                };
                let Some(to) = lex.point(cur, rel) else { break };
                push_quad(&mut pts, cur, c, to);
                cur = to;
                last_quad = Some(c);
                last_cubic = None;
            }
            b'A' => {
                let (Some(rx), Some(ry), Some(rot)) = (lex.number(), lex.number(), lex.number())
                else {
                    break;
                };
                let (Some(large), Some(sweep)) = (lex.flag(), lex.flag()) else {
                    break;
                };
                let Some(to) = lex.point(cur, rel) else { break };
                push_arc(&mut pts, cur, rx, ry, rot, large, sweep, to);
                cur = to;
                last_cubic = None;
                last_quad = None;
            }
            b'Z' => {
                flush(&mut out, &mut closed, &mut pts, true);
                // `Z` 之后不写 `M` 就接着画,起点回到子路径起点
                cur = start;
                pts.push(cur);
                last_cubic = None;
                last_quad = None;
            }
            _ => break,
        }
    }
    flush(&mut out, &mut closed, &mut pts, false);

    let (min_x, min_y, side) = view;
    let inv = if side.abs() > f32::EPSILON {
        1.0 / side
    } else {
        1.0
    };
    out.into_iter()
        .zip(closed)
        .map(|(pts, closed)| {
            (
                pts.into_iter()
                    .map(|(x, y)| ((x - min_x) * inv, (y - min_y) * inv))
                    .collect(),
                closed,
            )
        })
        .collect()
}

/// 收掉当前子路径。不足两个点的丢掉 —— `Z` 紧跟 `M`、或末尾多一个 `M`
/// 都会留下孤点,喂给 tessellator 只是白跑一趟。
fn flush(
    out: &mut Vec<Vec<(f32, f32)>>,
    closed: &mut Vec<bool>,
    pts: &mut Vec<(f32, f32)>,
    is_closed: bool,
) {
    if pts.len() >= 2 {
        out.push(std::mem::take(pts));
        closed.push(is_closed);
    } else {
        pts.clear();
    }
}

/// `S`/`T` 的控制点反射:上一条同族曲线的第二控制点关于当前点的对称点。
fn reflect(cur: (f32, f32), last: Option<(f32, f32)>) -> (f32, f32) {
    match last {
        Some((x, y)) => (2.0 * cur.0 - x, 2.0 * cur.1 - y),
        // 前一条不是同族曲线 → 控制点与当前点重合(SVG 1.1 8.3.6)
        None => cur,
    }
}

fn push_cubic(
    pts: &mut Vec<(f32, f32)>,
    from: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    to: (f32, f32),
) {
    for i in 1..=CURVE_SEGMENTS {
        let t = i as f32 / CURVE_SEGMENTS as f32;
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        pts.push((
            a * from.0 + b * c1.0 + c * c2.0 + d * to.0,
            a * from.1 + b * c1.1 + c * c2.1 + d * to.1,
        ));
    }
}

fn push_quad(pts: &mut Vec<(f32, f32)>, from: (f32, f32), c: (f32, f32), to: (f32, f32)) {
    for i in 1..=CURVE_SEGMENTS {
        let t = i as f32 / CURVE_SEGMENTS as f32;
        let u = 1.0 - t;
        let (a, b, d) = (u * u, 2.0 * u * t, t * t);
        pts.push((
            a * from.0 + b * c.0 + d * to.0,
            a * from.1 + b * c.1 + d * to.1,
        ));
    }
}

/// 椭圆弧:端点参数化 → 中心参数化 → 均匀采样(SVG 1.1 附录 F.6.5)。
#[allow(clippy::too_many_arguments)]
fn push_arc(
    pts: &mut Vec<(f32, f32)>,
    from: (f32, f32),
    rx: f32,
    ry: f32,
    rot_deg: f32,
    large: bool,
    sweep: bool,
    to: (f32, f32),
) {
    // 退化情形按规范都画直线
    if (from.0 - to.0).abs() < f32::EPSILON && (from.1 - to.1).abs() < f32::EPSILON {
        return;
    }
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx < f32::EPSILON || ry < f32::EPSILON {
        pts.push(to);
        return;
    }
    let phi = rot_deg.to_radians();
    let (sin, cos) = (phi.sin(), phi.cos());
    let (dx2, dy2) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let x1p = cos * dx2 + sin * dy2;
    let y1p = -sin * dx2 + cos * dy2;
    // 半径不够长就等比放大到刚好够(F.6.6)
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = (rx * rx * ry * ry) - (rx * rx * y1p * y1p) - (ry * ry * x1p * x1p);
    let den = (rx * rx * y1p * y1p) + (ry * ry * x1p * x1p);
    let sign = if large == sweep { -1.0 } else { 1.0 };
    let coef = sign * (num / den).max(0.0).sqrt();
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos * cxp - sin * cyp + (from.0 + to.0) / 2.0;
    let cy = sin * cxp + cos * cyp + (from.1 + to.1) / 2.0;

    let theta1 = ((y1p - cyp) / ry).atan2((x1p - cxp) / rx);
    let theta2 = ((-y1p - cyp) / ry).atan2((-x1p - cxp) / rx);
    let mut delta = theta2 - theta1;
    if !sweep && delta > 0.0 {
        delta -= TAU;
    } else if sweep && delta < 0.0 {
        delta += TAU;
    }

    // 段数按扫过的角度分摊,一个 90° 的弧不必用整圈的段数。
    // 取 round 而不是 ceil:整四分之一圈算出来是 12.000001,ceil 会多切一段,
    // 段数随浮点噪声抖动,单测就没法钉住点数了
    let segments = ((super::vector::ARC_SEGMENTS as f32) * (delta.abs() / TAU)).round() as usize;
    let segments = segments.clamp(2, super::vector::ARC_SEGMENTS);
    for i in 1..=segments {
        let theta = theta1 + delta * (i as f32) / (segments as f32);
        let (x, y) = (rx * theta.cos(), ry * theta.sin());
        pts.push((cx + x * cos - y * sin, cy + x * sin + y * cos));
    }
}

// ─── 词法 ─────────────────────────────────────────────────────

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn skip_sep(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b',' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// 吃掉一个命令字母。当前位置不是字母就返回 `None`(= 隐式重复)。
    fn take_cmd(&mut self) -> Option<u8> {
        self.skip_sep();
        let c = *self.b.get(self.i)?;
        if c.is_ascii_alphabetic() {
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }

    fn has_number(&mut self) -> bool {
        self.skip_sep();
        matches!(self.b.get(self.i), Some(b'0'..=b'9' | b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_sep();
        let start = self.i;
        if matches!(self.b.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        self.digits();
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            self.digits();
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'-' | b'+')) {
                self.i += 1;
            }
            if matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.digits();
            } else {
                self.i = save; // `e` 不是指数的一部分,吐回去
            }
        }
        if self.i == start {
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i]).ok()?.parse().ok()
    }

    fn digits(&mut self) {
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }

    /// 一对坐标,`rel` 时叠加到当前点上。
    fn point(&mut self, cur: (f32, f32), rel: bool) -> Option<(f32, f32)> {
        let (x, y) = (self.number()?, self.number()?);
        Some(if rel { (cur.0 + x, cur.1 + y) } else { (x, y) })
    }

    /// 弧命令的 0/1 标志位。SVG 允许它们**连写** —— `a2.97 2.97 0 01-.104-.729`
    /// 里的 `01` 是 large=0、sweep=1 两个标志,走 [`Self::number`] 会被吃成
    /// 一个 `1`,弧当场画歪。所以只能按单字符读。
    fn flag(&mut self) -> Option<bool> {
        self.skip_sep();
        match self.b.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Some(false)
            }
            Some(b'1') => {
                self.i += 1;
                Some(true)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单位方框、边长 1 的 viewBox —— 直接按原坐标断言。
    const UNIT: (f32, f32, f32) = (0.0, 0.0, 1.0);

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.001
    }

    fn assert_pts(got: &[(f32, f32)], want: &[(f32, f32)]) {
        assert_eq!(got.len(), want.len(), "点数不同:{got:?} vs {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!(approx(g.0, w.0) && approx(g.1, w.1), "{got:?} vs {want:?}");
        }
    }

    #[test]
    fn 绝对与相对_直线族() {
        // H/V 只给一个坐标,另一轴保持不变;小写全是增量
        let subs = parse("M1 1 H3 V4 l-1 -1 h-1 v-1 Z", UNIT);
        assert_eq!(subs.len(), 1);
        assert!(subs[0].1, "Z 收尾 = 闭合");
        assert_pts(
            &subs[0].0,
            &[(1.0, 1.0), (3.0, 1.0), (3.0, 4.0), (2.0, 3.0), (1.0, 3.0), (1.0, 2.0)],
        );
    }

    #[test]
    fn m_之后的坐标对按_l_处理() {
        // SVG 1.1 8.3.2:`M0 0 1 1 2 2` = 起点 + 两条直线,而不是三个起点
        let subs = parse("M0 0 1 1 2 2", UNIT);
        assert_eq!(subs.len(), 1, "只有一条子路径");
        assert_pts(&subs[0].0, &[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
    }

    #[test]
    fn 多子路径与_z_之后接着画() {
        // 第一条 Z 闭合;之后没写 M 就接着画,起点回到 (0,0)
        let subs = parse("M0 0 H2 V2 Z H1 V1", UNIT);
        assert_eq!(subs.len(), 2);
        assert!(subs[0].1 && !subs[1].1, "只有 Z 收尾的那条算闭合");
        assert_pts(&subs[1].0, &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]);
    }

    #[test]
    fn 弧的标志位可以连写() {
        // `01` 是 large=0 / sweep=1 两个标志,当成数字读会把弧画反
        // (0,1) → (1,0) 半径 1:两个候选圆心 (0,0) 与 (1,1),large/sweep 决定选哪个
        let on_circle = |d: &str, c: (f32, f32)| {
            let subs = parse(d, UNIT);
            let pts = subs[0].0.clone();
            // 四分之一圈 → 48/4 = 12 段,加上起点
            assert_eq!(pts.len(), super::super::vector::ARC_SEGMENTS / 4 + 1, "{d}");
            let last = pts[pts.len() - 1];
            assert!(approx(last.0, 1.0) && approx(last.1, 0.0), "{d} 终点跑偏 {last:?}");
            for p in &pts {
                let r = ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt();
                assert!(approx(r, 1.0), "{d}:{p:?} 不在以 {c:?} 为心的单位圆上");
            }
        };
        // `01` = large 0 / sweep 1(正角方向的小弧)→ 圆心 (1,1)
        on_circle("M0 1 a1 1 0 01 1 -1", (1.0, 1.0));
        // `00` = large 0 / sweep 0 → 另一侧的圆心 (0,0)。两条只差一个字符,
        // 标志位要是被 number() 吃成一个数,这两条会解析成同一个东西
        on_circle("M0 1 a1 1 0 00 1 -1", (0.0, 0.0));
    }

    #[test]
    fn 半径不足时等比放大() {
        // 两端点相距 2,半径只给 0.5 —— 规范要求放大到刚好够,而不是画不出来
        let subs = parse("M0 0 A0.5 0.5 0 0 1 2 0", UNIT);
        let pts = &subs[0].0;
        assert!(approx(pts[pts.len() - 1].0, 2.0) && approx(pts[pts.len() - 1].1, 0.0));
        // 半圆的顶点应落在 (1, ±1) 附近
        assert!(pts.iter().any(|p| approx(p.0, 1.0) && p.1.abs() > 0.9));
    }

    #[test]
    fn s_与_t_反射上一条同族曲线的控制点() {
        // C 的第二控制点 (1,1) 关于终点 (2,0) 的反射是 (3,-1)
        let reflected = parse("M0 0 C1 -1 1 1 2 0 S3 1 4 0", UNIT);
        let explicit = parse("M0 0 C1 -1 1 1 2 0 C3 -1 3 1 4 0", UNIT);
        assert_pts(&reflected[0].0, &explicit[0].0);
        // 前一条不是同族曲线时,反射出来的控制点退化到当前点 (1,0)
        let lone = parse("M0 0 L1 0 S2 1 3 0", UNIT);
        let same = parse("M0 0 L1 0 C1 0 2 1 3 0", UNIT);
        assert_pts(&lone[0].0, &same[0].0);
    }

    #[test]
    fn 数字紧挨着写也能切开() {
        // `.5.5` 是两个数;`1-2` 是 1 和 -2;指数记法也要认
        let subs = parse("M.5.5L1-2 1e1 2E-1", UNIT);
        assert_pts(&subs[0].0, &[(0.5, 0.5), (1.0, -2.0), (10.0, 0.2)]);
    }

    #[test]
    fn viewbox_归一到单位方框() {
        // pi 的 viewBox:165.29 165.29 469.43 469.43
        let view = (165.29, 165.29, 469.43);
        let subs = parse("M165.29 165.29H517.36V634.72Z", view);
        assert_pts(&subs[0].0, &[(0.0, 0.0), (0.75, 0.0), (0.75, 1.0)]);
    }

    #[test]
    fn 语法错误就地截断而不是崩() {
        // 少一个坐标 / 冒出不认识的命令,都只是把后面丢掉
        assert_eq!(parse("M0 0 L1", UNIT).len(), 0, "残缺的 L 连点都凑不齐两个");
        assert_pts(&parse("M0 0 L1 1 X9 9", UNIT)[0].0, &[(0.0, 0.0), (1.0, 1.0)]);
        assert!(parse("", UNIT).is_empty());
        assert!(parse("garbage", UNIT).is_empty());
    }

    #[test]
    fn 缓存命中同一份结果() {
        const D: &str = "M0 0H1V1Z";
        let a = cached(D, UNIT);
        let b = cached(D, UNIT);
        assert!(Rc::ptr_eq(&a, &b), "同一个 &'static str 该直接复用解析结果");
    }
}
