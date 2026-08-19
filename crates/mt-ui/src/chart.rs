//! 自绘趋势图(面积曲线 + 柱 + 网格 + 双轴)——替 recharts 的 `ComposedChart`。
//!
//! 对照原版 `src/components/usage/DailyChart.tsx`:
//!
//! | 原版 | 这里 |
//! |---|---|
//! | `<Area type="monotone" dataKey="cost" stroke=accent 1.8 fill=渐变>` | [`ChartModel::area`] 采样曲线 + [`ChartCanvas`] 的面积/描边 |
//! | `<Bar dataKey="calls" fill=text-muted opacity=.28 radius=[2,2,0,0] maxBarSize=14>` | [`ChartModel::bars`] + 上圆角 quad |
//! | `<CartesianGrid vertical={false} strokeDasharray="3 4">` | [`ChartStyle::grid_dash`] 的虚线 quad |
//! | `<YAxis orientation=left width=52>` / `right width=44` | [`ChartModel::left_ticks`] / [`right_ticks`](ChartModel::right_ticks),**标签由宿主摆** |
//! | `<XAxis minTickGap={24}>` | [`label_step`] |
//! | `dot={r:2.5 / 1.8 / false}` | [`ChartModel::dots`] |
//!
//! # 三条设计决定
//!
//! 1. **归一化缓存**。曲线采样存的是 0..1 的相对坐标(x 从左到右,y 从底到顶),
//!    与像素尺寸无关 —— 拖窗口改大小不必重算,数据不变就整个 [`ChartModel`]
//!    复用(宿主拿 [`ChartKey`] 判等)。单调三次插值对坐标是仿射等变的,
//!    先归一化再缩放与直接在像素上插值结果完全一致。
//! 2. **文字不进元素**。gpui 的自绘元素要画字得自己 shape,而轴刻度的字号/字族
//!    是壳的主题量(`ui::font_px`)。所以本元素只画几何,刻度文本由宿主用普通
//!    `div` 绝对定位摆在两侧 —— 位置就是「第 i 条刻度线」,等距,宿主自己能算。
//! 3. **渐变靠分段**。`paint_path` 只吃单色(V 批拓扑图同款约束),面积的竖向
//!    渐变切成 [`ChartStyle::gradient_bands`] 条横带,每条取该带中点的插值色。
//!    ⚠️ 与 V 批**相反**:那边是不透明描边,段间要留 2% 重叠防缝;这里是
//!    **半透明**填充,重叠区会二次混合出一条更深的横线(0.3 alpha 叠一次就到
//!    0.48,肉眼可见),所以这里**严格相邻不重叠** —— 抗锯齿在共享边上留下的
//!    是 a·b/4 ≈ 0.02 量级的淡缝,比重叠的深线小一个数量级。

use std::rc::Rc;

use gpui::{
    App, Bounds, Corners, Element, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    PathBuilder, Pixels, Point, Style, Window, fill, point, px, size,
};

// ─── 纯几何 ──────────────────────────────────────────────────

/// 「好看的」刻度值。对应 recharts 默认的 `domain={[0, 'auto']}` + `tickCount={5}`:
/// 从 0 起、步长取 1/2/5/10 × 10^k 里第一个能让刻度数不超过 `count` 的。
///
/// 返回值升序、首项恒为 0、末项 ≥ `max`(轴顶就是末项)。
/// `max <= 0`(例如价格表缺失导致成本全 0)时只返回 `[0.0]` —— 一条贴底的基线,
/// 不编造一个假的量纲出来。
pub fn nice_ticks(max: f64, count: usize) -> Vec<f64> {
    if !max.is_finite() || max <= 0.0 {
        return vec![0.0];
    }
    let count = count.max(2);
    let raw = max / (count - 1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = mag * if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    let steps = (max / step).ceil().max(1.0) as usize;
    (0..=steps).map(|i| i as f64 * step).collect()
}

/// 分类轴(band scale)的第 i 格中心,归一化到 0..1。
///
/// 有柱状系列时 recharts 的 X 轴是 band scale,**折线/面积的点也落在格中心**
/// (不是格边界)——照抄这一条,否则曲线会与柱子错开半格。
pub fn band_center(index: usize, count: usize) -> f32 {
    if count == 0 {
        return 0.5;
    }
    (index as f32 + 0.5) / count as f32
}

/// 单调三次插值(Fritsch–Carlson)的各点切线,输入是等距节点上的值。
///
/// d3 的 `curveMonotoneX`(recharts `type="monotone"` 用的那条)是同族方法,
/// 差别只在端点切线的取法;肉眼无差,不为此复刻 d3 的 `slope2/slope3`。
/// **不过冲**是这条曲线的定义性质:成本序列有 0 的地方曲线不许探到负值下方。
pub fn monotone_tangents(values: &[f32]) -> Vec<f32> {
    let n = values.len();
    if n < 2 {
        return vec![0.0; n];
    }
    // 节点等距,取 h = 1 —— 切线单位是「每格」
    let d: Vec<f32> = (0..n - 1).map(|i| values[i + 1] - values[i]).collect();
    let mut m = Vec::with_capacity(n);
    m.push(d[0]);
    for i in 1..n - 1 {
        m.push(if d[i - 1] * d[i] <= 0.0 {
            // 极值点:切线取 0,曲线在这里「压平」而不是甩出去
            0.0
        } else {
            (d[i - 1] + d[i]) / 2.0
        });
    }
    m.push(d[n - 2]);
    for i in 0..n - 1 {
        if d[i].abs() < f32::EPSILON {
            m[i] = 0.0;
            m[i + 1] = 0.0;
            continue;
        }
        let (alpha, beta) = (m[i] / d[i], m[i + 1] / d[i]);
        let s = alpha * alpha + beta * beta;
        if s > 9.0 {
            let tau = 3.0 / s.sqrt();
            m[i] = tau * alpha * d[i];
            m[i + 1] = tau * beta * d[i];
        }
    }
    m
}

/// 每格切多少段采样。段数随格数递减,总点数封顶 —— 一年的自定义范围(365 格)
/// 也不会攒出几千个顶点让 tessellation 变成每帧的大头。
pub fn samples_per_segment(count: usize) -> usize {
    if count <= 1 {
        return 1;
    }
    (MAX_CURVE_POINTS / (count - 1)).clamp(1, 8)
}

/// 曲线采样点总数上限。
const MAX_CURVE_POINTS: usize = 600;

/// 把等距节点上的值采样成折线(归一化坐标,x 落在 band 中心之间)。
///
/// 返回的 y 与输入同量纲(调用方自己先除以轴顶)。
pub fn sample_curve(values: &[f32]) -> Vec<(f32, f32)> {
    let n = values.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(band_center(0, 1), values[0])];
    }
    let m = monotone_tangents(values);
    let per = samples_per_segment(n);
    let mut out = Vec::with_capacity((n - 1) * per + 1);
    for i in 0..n - 1 {
        let (x0, x1) = (band_center(i, n), band_center(i + 1, n));
        for s in 0..per {
            let t = s as f32 / per as f32;
            let (t2, t3) = (t * t, t * t * t);
            // Hermite 基:h00 h10 h01 h11
            let y = (2.0 * t3 - 3.0 * t2 + 1.0) * values[i]
                + (t3 - 2.0 * t2 + t) * m[i]
                + (-2.0 * t3 + 3.0 * t2) * values[i + 1]
                + (t3 - t2) * m[i + 1];
            out.push((x0 + (x1 - x0) * t, y));
        }
    }
    out.push((band_center(n - 1, n), values[n - 1]));
    out
}

/// X 轴每隔几格摆一个刻度标签(recharts 的 `minTickGap`)。
///
/// `width` 是绘图区像素宽,`label_width` 是一个标签的估计宽度。
/// 原版按真实文本测量,这里按估计值 —— 差一格的观感差异,不值得为它把
/// 文本 shaping 拉进纯几何层。
pub fn label_step(count: usize, width: f32, label_width: f32, min_gap: f32) -> usize {
    if count <= 1 || width <= 0.0 {
        return 1;
    }
    let band = width / count as f32;
    let need = (label_width + min_gap).max(1.0);
    ((need / band).ceil() as usize).max(1)
}

/// 面积在某条渐变横带里的**上沿**点列(归一化坐标)。
///
/// 曲线大多数时候只穿过一两条带,其余带的上沿是一整条贴边直线 —— 一条直线上的
/// 中间点全部压掉,别把几百个共线顶点白喂给 tessellator(面积是每帧重画的,
/// 这里省下来的是实打实的每帧开销)。
///
/// 结果为空或整条贴在 `lo` 上,说明这条带里没有面积,调用方应当整带跳过。
pub fn band_top_edge(area: &[(f32, f32)], lo: f32, hi: f32) -> Vec<(f32, f32)> {
    let clamp = |y: f32| y.clamp(lo, hi);
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(8);
    for (i, (x, y)) in area.iter().enumerate() {
        let cy = clamp(*y);
        // 前后都与自己等高 = 一段直线的中间点,丢掉
        let prev_same = i > 0 && clamp(area[i - 1].1) == cy;
        let next_same = i + 1 < area.len() && clamp(area[i + 1].1) == cy;
        if prev_same && next_same {
            continue;
        }
        out.push((*x, cy));
    }
    out
}

// ─── 图表模型 ────────────────────────────────────────────────

/// 数据指纹。宿主拿它判「数据没变 → 直接复用上一份 [`ChartModel`]」,
/// 避免每帧重建曲线(性能红线:趋势图不许每帧重建 path)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChartKey {
    len: usize,
    /// 两个序列的逐值哈希(f64 按 bits 取)。
    hash: u64,
}

impl ChartKey {
    pub fn of(area: &[f64], bars: &[f64]) -> Self {
        // FNV-1a:短序列够用,不引第三方哈希
        let mut hash: u64 = 0xcbf29ce484222325;
        let mut feed = |v: u64| {
            for b in v.to_le_bytes() {
                hash ^= b as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        };
        for v in area {
            feed(v.to_bits());
        }
        feed(0xffff_ffff_ffff_ffff);
        for v in bars {
            feed(v.to_bits());
        }
        Self {
            len: area.len().max(bars.len()),
            hash,
        }
    }
}

/// 一张图的全部几何,坐标已归一化(x:0=左 1=右;y:0=底 1=轴顶)。
#[derive(Clone, Debug)]
pub struct ChartModel {
    /// 面积/折线的采样点。
    pub area: Vec<(f32, f32)>,
    /// 每格一个数据点(圆点);`dot_radius` 为 0 时宿主不画。
    pub dots: Vec<(f32, f32)>,
    /// 柱高(每格一个,0..1)。
    pub bars: Vec<f32>,
    /// 网格线/左轴刻度所在的归一化高度。
    pub grid: Vec<f32>,
    /// 左轴刻度值(成本),升序,首项 0。
    pub left_ticks: Vec<f64>,
    /// 右轴刻度值(调用数),升序,首项 0。
    pub right_ticks: Vec<f64>,
    /// 分几格。
    pub count: usize,
    /// 圆点半径(px),0 = 不画。
    pub dot_radius: f32,
}

impl ChartModel {
    /// `area` 走左轴,`bars` 走右轴;两者长度应相同(以 `area` 为准)。
    pub fn build(area: &[f64], bars: &[f64], tick_count: usize) -> Self {
        let count = area.len();
        let left_ticks = nice_ticks(area.iter().copied().fold(0.0, f64::max), tick_count);
        let right_ticks = nice_ticks(bars.iter().copied().fold(0.0, f64::max), tick_count);
        let left_top = left_ticks.last().copied().unwrap_or(0.0);
        let right_top = right_ticks.last().copied().unwrap_or(0.0);

        let norm: Vec<f32> = area
            .iter()
            .map(|v| if left_top > 0.0 { (v / left_top) as f32 } else { 0.0 })
            .collect();
        let curve = sample_curve(&norm);
        let dots: Vec<(f32, f32)> = norm
            .iter()
            .enumerate()
            .map(|(i, v)| (band_center(i, count), *v))
            .collect();
        // 原版:≤40 格画 r2.5 的点,≤90 格 r1.8,再多不画(点会连成一条粗线)
        let dot_radius = if count <= 40 {
            2.5
        } else if count <= 90 {
            1.8
        } else {
            0.0
        };
        let grid = if left_ticks.len() > 1 {
            let top = left_ticks.len() - 1;
            (0..=top).map(|i| i as f32 / top as f32).collect()
        } else {
            vec![0.0]
        };

        Self {
            area: curve,
            dots,
            bars: bars
                .iter()
                .map(|v| if right_top > 0.0 { (v / right_top) as f32 } else { 0.0 })
                .collect(),
            grid,
            left_ticks,
            right_ticks,
            count,
            dot_radius,
        }
    }

    /// 左轴顶(= 最后一条刻度)。
    pub fn left_top(&self) -> f64 {
        self.left_ticks.last().copied().unwrap_or(0.0)
    }

    /// 右轴顶。
    pub fn right_top(&self) -> f64 {
        self.right_ticks.last().copied().unwrap_or(0.0)
    }
}

// ─── 元素 ────────────────────────────────────────────────────

/// 画笔参数。默认值逐条抄 `DailyChart.tsx`。
#[derive(Clone, Copy, Debug)]
pub struct ChartStyle {
    /// 曲线描边宽度(原版 `strokeWidth={1.8}`)。
    pub line_width: f32,
    /// 柱子最大宽度(原版 `maxBarSize={14}`)。
    pub max_bar: f32,
    /// 柱子占格宽的比例(recharts 默认 `barCategoryGap="10%"` → 两侧各让 10%)。
    pub bar_ratio: f32,
    /// 柱子上圆角(原版 `radius={[2,2,0,0]}`)。
    pub bar_radius: f32,
    /// 网格虚线(实线段长, 间隔),原版 `strokeDasharray="3 4"`。
    pub grid_dash: (f32, f32),
    /// 面积渐变切几条横带。
    pub gradient_bands: usize,
}

impl Default for ChartStyle {
    fn default() -> Self {
        Self {
            line_width: 1.8,
            max_bar: 14.0,
            bar_ratio: 0.8,
            bar_radius: 2.0,
            grid_dash: (3.0, 4.0),
            // V 批拓扑图用 8 条,那是 48px 行高;这里面积有 190px 高,
            // 8 条的 alpha 台阶(0.035/条)在大块色面上能看出来,加倍到 16
            gradient_bands: 16,
        }
    }
}

/// 配色。全部由宿主从主题取。
#[derive(Clone, Copy, Debug)]
pub struct ChartColors {
    /// 面积渐变顶色(原版 `accent` @ 0.3)。
    pub area_top: Hsla,
    /// 面积渐变底色(原版 `accent` @ 0.02)。
    pub area_bottom: Hsla,
    /// 曲线描边(`accent`)。
    pub line: Hsla,
    /// 柱(`text-muted` @ 0.28)。
    pub bar: Hsla,
    /// 网格(`border-default`)。
    pub grid: Hsla,
    /// 数据点(`accent`)。
    pub dot: Hsla,
}

/// 趋势图的绘图区元素。**元素的 bounds 就是绘图区**(轴标签由宿主摆在外面)。
pub struct ChartCanvas {
    model: Rc<ChartModel>,
    colors: ChartColors,
    style: ChartStyle,
    height: Pixels,
}

impl ChartCanvas {
    pub fn new(model: Rc<ChartModel>, colors: ChartColors, height: Pixels) -> Self {
        Self {
            model,
            colors,
            style: ChartStyle::default(),
            height,
        }
    }

    pub fn style(mut self, style: ChartStyle) -> Self {
        self.style = style;
        self
    }

    /// 面积渐变的第 k 条横带的颜色(带中点处的线性插值)。
    fn band_color(&self, k: usize) -> Hsla {
        let bands = self.style.gradient_bands.max(1);
        let t = (k as f32 + 0.5) / bands as f32;
        lerp_hsla(self.colors.area_top, self.colors.area_bottom, t)
    }
}

/// RGB 空间插值(HSL 上插会绕色相环)。
fn lerp_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (ra, rb) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
    gpui::Rgba {
        r: ra.r + (rb.r - ra.r) * t,
        g: ra.g + (rb.g - ra.g) * t,
        b: ra.b + (rb.b - ra.b) * t,
        a: ra.a + (rb.a - ra.a) * t,
    }
    .into()
}

impl IntoElement for ChartCanvas {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ChartCanvas {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = self.height.into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if w <= 1.0 || h <= 1.0 || self.model.count == 0 {
            return;
        }
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        // 归一化 → 像素。y 归一化的 0 在底部,屏幕坐标 y 向下,所以要翻一下
        let map = |x: f32, y: f32| -> Point<Pixels> {
            point(px(ox + x * w), px(oy + (1.0 - y.clamp(0.0, 1.0)) * h))
        };

        // ① 网格(横向虚线)。原版 `vertical={false}`,竖线一根都没有
        let (dash, gap) = self.style.grid_dash;
        for gy in &self.model.grid {
            // 最底下那条(值 0)贴着绘图区下沿:再往下 1px 就画到 X 轴条上了
            let y = (oy + (1.0 - gy) * h).min(oy + h - 1.0);
            let mut x = ox;
            while x < ox + w {
                let seg = dash.min(ox + w - x);
                window.paint_quad(fill(
                    Bounds::new(point(px(x), px(y)), size(px(seg), px(1.0))),
                    self.colors.grid,
                ));
                x += dash + gap;
            }
        }

        // ② 柱(调用数,右轴口径)。画在面积之下 —— 原版 JSX 里 Bar 在 Area 之前,
        //    SVG 没有 z-index,层叠就是书写顺序
        let band = w / self.model.count as f32;
        let bar_w = (band * self.style.bar_ratio).min(self.style.max_bar).max(1.0);
        for (i, ratio) in self.model.bars.iter().enumerate() {
            let bar_h = (ratio.clamp(0.0, 1.0) * h).max(if *ratio > 0.0 { 1.0 } else { 0.0 });
            if bar_h <= 0.0 {
                continue;
            }
            let cx_px = ox + band_center(i, self.model.count) * w;
            let r = self.style.bar_radius.min(bar_w / 2.0).min(bar_h);
            window.paint_quad(
                fill(
                    Bounds::new(
                        point(px(cx_px - bar_w / 2.0), px(oy + h - bar_h)),
                        size(px(bar_w), px(bar_h)),
                    ),
                    self.colors.bar,
                )
                .corner_radii(Corners {
                    top_left: px(r),
                    top_right: px(r),
                    bottom_left: px(0.0),
                    bottom_right: px(0.0),
                }),
            );
        }

        // ③ 面积(竖向渐变 → 分段横带,严格相邻不重叠,见模块注释)
        let bands = self.style.gradient_bands.max(1);
        for k in 0..bands {
            // 带的上下边界(归一化,1=顶)
            let hi = 1.0 - k as f32 / bands as f32;
            let lo = 1.0 - (k + 1) as f32 / bands as f32;
            let top = band_top_edge(&self.model.area, lo, hi);
            // 上沿整条压在带底 → 这带没有面积(曲线还在下面)
            if top.len() < 2 || top.iter().all(|(_, y)| *y <= lo + 1e-6) {
                continue;
            }
            let mut builder = PathBuilder::fill();
            builder.move_to(map(top[0].0, top[0].1));
            for (x, y) in &top[1..] {
                builder.line_to(map(*x, *y));
            }
            // 回程是带底那条直线,两个端点就够(逐点画等于白喂 tessellator)
            builder.line_to(map(top[top.len() - 1].0, lo));
            builder.line_to(map(top[0].0, lo));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, self.band_color(k));
            }
        }

        // ④ 曲线本体
        if self.model.area.len() >= 2 {
            let mut builder = PathBuilder::stroke(px(self.style.line_width.max(0.5)));
            let mut pts = self.model.area.iter();
            if let Some((x, y)) = pts.next() {
                builder.move_to(map(*x, *y));
            }
            for (x, y) in pts {
                builder.line_to(map(*x, *y));
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, self.colors.line);
            }
        }

        // ⑤ 数据点(圆 = 全圆角 quad,比 tessellate 一圈三角形便宜)
        if self.model.dot_radius > 0.0 {
            let r = self.model.dot_radius;
            for (x, y) in &self.model.dots {
                let c = map(*x, *y);
                window.paint_quad(
                    fill(
                        Bounds::new(
                            point(c.x - px(r), c.y - px(r)),
                            size(px(r * 2.0), px(r * 2.0)),
                        ),
                        self.colors.dot,
                    )
                    .corner_radii(Corners::all(px(r))),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 刻度从零起步长取一二五十() {
        assert_eq!(nice_ticks(4.0, 5), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(nice_ticks(3.5, 5), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(nice_ticks(10.0, 5), vec![0.0, 5.0, 10.0]);
        assert_eq!(nice_ticks(1000.0, 5), vec![0.0, 500.0, 1000.0]);
        // 小数量纲(成本常在几分钱级别)
        let t = nice_ticks(0.07, 5);
        assert_eq!(t.first(), Some(&0.0));
        assert!(t.last().copied().unwrap() >= 0.07);
        assert!(t.len() <= 5, "刻度不该超过 tickCount:{t:?}");
    }

    #[test]
    fn 刻度末项恒不小于最大值且数量有界() {
        for max in [
            0.001, 0.017, 0.9, 1.0, 1.3, 7.7, 23.0, 99.0, 101.0, 1_234.0, 987_654.0,
        ] {
            let t = nice_ticks(max, 5);
            assert_eq!(t[0], 0.0);
            assert!(
                t.last().copied().unwrap() >= max - 1e-9,
                "max={max} 的轴顶盖不住数据:{t:?}"
            );
            assert!(t.len() >= 2 && t.len() <= 5, "max={max} 刻度数 {}", t.len());
            // 等距
            let step = t[1] - t[0];
            for w in t.windows(2) {
                assert!((w[1] - w[0] - step).abs() < step * 1e-6);
            }
        }
    }

    #[test]
    fn 全零与非法值退成单条基线() {
        // 价格表缺失 → 成本全 0:不编造量纲,只给一条贴底的基线
        assert_eq!(nice_ticks(0.0, 5), vec![0.0]);
        assert_eq!(nice_ticks(-1.0, 5), vec![0.0]);
        assert_eq!(nice_ticks(f64::NAN, 5), vec![0.0]);
        assert_eq!(nice_ticks(f64::INFINITY, 5), vec![0.0]);
    }

    #[test]
    fn 点落在格中心不落在格边界() {
        // 有柱子时 X 是 band scale,曲线点与柱心必须重合
        assert!((band_center(0, 2) - 0.25).abs() < 1e-6);
        assert!((band_center(1, 2) - 0.75).abs() < 1e-6);
        assert!((band_center(0, 1) - 0.5).abs() < 1e-6);
        assert_eq!(band_center(0, 0), 0.5, "空数据不该除零");
    }

    #[test]
    fn 单调插值不过冲() {
        // 0 → 峰 → 0 的序列,采样点不许探到 0 以下(会画出负成本的鼓包)
        let values = [0.0, 0.0, 1.0, 0.0, 0.0, 0.4, 0.0];
        let pts = sample_curve(&values);
        for (x, y) in &pts {
            assert!(*y >= -1e-5, "在 x={x} 处过冲到 {y}");
            assert!(*y <= 1.0 + 1e-5, "在 x={x} 处过冲到 {y}");
        }
    }

    #[test]
    fn 采样点穿过每个数据点() {
        let values = [0.2, 0.9, 0.5, 0.7];
        let pts = sample_curve(&values);
        let per = samples_per_segment(values.len());
        for (i, v) in values.iter().enumerate() {
            let idx = (i * per).min(pts.len() - 1);
            assert!(
                (pts[idx].1 - v).abs() < 1e-5,
                "第 {i} 个数据点没被穿过:{:?}",
                pts[idx]
            );
            assert!((pts[idx].0 - band_center(i, values.len())).abs() < 1e-5);
        }
    }

    #[test]
    fn 采样点数封顶() {
        // 一年的自定义范围也不该攒出几千个顶点
        for n in [2usize, 7, 30, 90, 180, 365, 1000] {
            let values: Vec<f32> = (0..n).map(|i| (i % 7) as f32 / 7.0).collect();
            let pts = sample_curve(&values);
            assert!(pts.len() <= MAX_CURVE_POINTS + n, "n={n} 采出了 {}", pts.len());
            assert!(!pts.is_empty());
        }
        assert_eq!(sample_curve(&[]).len(), 0);
        assert_eq!(sample_curve(&[0.5]).len(), 1);
    }

    #[test]
    fn 标签间隔按可用宽度稀释() {
        // 30 格挤在 300px 里,每格 10px,28px 的标签 + 24px 间隙 → 每 6 格一个
        assert_eq!(label_step(30, 300.0, 28.0, 24.0), 6);
        // 宽松时逐格都摆
        assert_eq!(label_step(5, 600.0, 28.0, 24.0), 1);
        // 退化输入
        assert_eq!(label_step(0, 600.0, 28.0, 24.0), 1);
        assert_eq!(label_step(10, 0.0, 28.0, 24.0), 1);
    }

    #[test]
    fn 模型归一化与圆点档位() {
        let area = [0.0, 1.0, 2.0, 4.0];
        let bars = [0.0, 10.0, 20.0, 40.0];
        let m = ChartModel::build(&area, &bars, 5);
        assert_eq!(m.left_ticks, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(m.left_top(), 4.0);
        assert_eq!(m.right_top(), 40.0);
        assert_eq!(m.bars, vec![0.0, 0.25, 0.5, 1.0]);
        assert_eq!(m.grid.len(), 5);
        assert!((m.grid[0] - 0.0).abs() < 1e-6 && (m.grid[4] - 1.0).abs() < 1e-6);
        assert_eq!(m.dot_radius, 2.5, "≤40 格用大点");

        let many: Vec<f64> = (0..60).map(|i| i as f64).collect();
        assert_eq!(ChartModel::build(&many, &many, 5).dot_radius, 1.8);
        let lots: Vec<f64> = (0..120).map(|i| i as f64).collect();
        assert_eq!(ChartModel::build(&lots, &lots, 5).dot_radius, 0.0, "过密就不画点");
    }

    #[test]
    fn 空数据与全零都不炸() {
        let m = ChartModel::build(&[], &[], 5);
        assert_eq!(m.count, 0);
        assert!(m.area.is_empty() && m.bars.is_empty());
        assert_eq!(m.left_ticks, vec![0.0]);

        let zeros = [0.0f64; 5];
        let m = ChartModel::build(&zeros, &zeros, 5);
        assert_eq!(m.bars, vec![0.0; 5], "全零时柱高全 0 而不是 NaN");
        assert!(m.area.iter().all(|(_, y)| y.abs() < 1e-6));
        assert_eq!(m.grid, vec![0.0], "只有一条贴底基线");
    }

    #[test]
    fn 单点数据也能建模型() {
        let m = ChartModel::build(&[3.0], &[7.0], 5);
        assert_eq!(m.count, 1);
        assert_eq!(m.area.len(), 1);
        assert!((m.area[0].0 - 0.5).abs() < 1e-6, "单点落在正中");
        assert_eq!(m.bars.len(), 1);
    }

    #[test]
    fn 渐变横带的上沿压掉共线中间点() {
        // 一条从底爬到顶再回来的折线,取中间那条带 [0.4, 0.6]
        let area = vec![
            (0.0, 0.0),
            (0.1, 0.0),
            (0.2, 0.0),
            (0.3, 0.5),
            (0.4, 1.0),
            (0.5, 1.0),
            (0.6, 1.0),
            (0.7, 0.5),
            (0.8, 0.0),
            (0.9, 0.0),
        ];
        let edge = band_top_edge(&area, 0.4, 0.6);
        // 首尾必须在(带底那条回程线要靠它们定端点)
        assert_eq!(edge.first().map(|p| p.0), Some(0.0));
        assert_eq!(edge.last().map(|p| p.0), Some(0.9));
        // 三段各自的中间点被压掉:0.1 / 0.5 / 0.9 之类不该全在
        assert!(edge.len() < area.len(), "没压掉任何点:{edge:?}");
        // 值全被钳进带内
        for (_, y) in &edge {
            assert!((0.4..=0.6).contains(y), "{y} 越出带外");
        }
        // 曲线整段在带下 → 上沿贴底,调用方据此跳过
        let flat = band_top_edge(&[(0.0, 0.0), (0.5, 0.0), (1.0, 0.0)], 0.4, 0.6);
        assert!(flat.iter().all(|(_, y)| (*y - 0.4).abs() < 1e-6));
        // 退化输入
        assert!(band_top_edge(&[], 0.0, 1.0).is_empty());
        assert_eq!(band_top_edge(&[(0.5, 0.5)], 0.0, 1.0).len(), 1);
    }

    #[test]
    fn 指纹只在数据变化时变() {
        let a = [1.0, 2.0, 3.0];
        let b = [4.0, 5.0, 6.0];
        assert_eq!(ChartKey::of(&a, &b), ChartKey::of(&a, &b));
        assert_ne!(ChartKey::of(&a, &b), ChartKey::of(&b, &a));
        assert_ne!(ChartKey::of(&a, &b), ChartKey::of(&[1.0, 2.0, 3.0001], &b));
        // 两个序列的边界要分得开(拼接歧义)
        assert_ne!(
            ChartKey::of(&[1.0, 2.0], &[3.0]),
            ChartKey::of(&[1.0], &[2.0, 3.0])
        );
    }
}
