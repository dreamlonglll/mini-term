//! AI 厂商图标(对照 `src/components/BrandIcon.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版用 `@lobehub/icons` 的深路径 import 拿官方 logo(Color / Mono 两档),
//! 那是 npm 包里的 SVG 资产,仓库里没有源文件可搬。GPUI 侧改为**自绘简化标记**:
//! 形状是各家 logo 的几何骨架,**品牌色逐个照抄官方值**,并沿用原版
//! `MONO_BRAND_COLORS` 的口径(官方 logo 本为黑白的品牌借主色提辨识度)。
//!
//! 唯一一枚**逐点等价**的是 pi —— 它的官方 path 就写在 `BrandIcon.tsx` 里
//! (轴对齐矩形,坐标全落在 1/4 格上),照搬无损。
//!
//! 商标注意(原样保留原版的红线):品牌 logo 仅作「该会话属于哪家 AI」的指示性使用,
//! 不得用作产品自身标识。
//!
//! # 已知偏差
//!
//! - 简化标记不是官方 logo 的像素级复刻,只保证「一眼认得出是哪家」;
//! - 原版 Color 变体是多色的(Claude/Gemini/Qwen/DeepSeek/Zhipu),这里每家收敛成
//!   1~2 个色 —— 想换成官方资产时只要替掉本文件的形状表,调用方一行不用改。
//!
//! # 宿主接线(mt-app)
//!
//! `session_panel.rs` 现在是两个字母的文本徽标:
//!
//! ```ignore
//! let badge = match session.session_type.as_str() {
//!     "codex" => "CX", "grok" => "GK", _ => "CL",
//! };
//! ```
//!
//! 换成:
//!
//! ```ignore
//! use mt_ui::icons::{AiVendor, BrandIcon};
//! // 会话面板/分支树:最新模型名优先,识别不出回落 CLI(对齐 vendorForSession)
//! let vendor = AiVendor::for_session(&session.session_type, session.model.as_deref());
//! // …child(BrandIcon::new(vendor).size(px(13.0)))
//! ```
//!
//! tab 栏 / pane 标题要的是「跑的是哪个 CLI」,用
//! `AiVendor::from_session_type(&pane.agent)`;从启动器命令文本猜厂商用
//! [`AiVendor::infer`](AiVendor::infer)(与前端 `inferVendor` 同规则同优先级)。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::vector::{Geom, Ink, Shape, VectorIcon};

/// AI 厂商。取值与 `src/types.ts` 的 `AiVendor` 一字不差(序列化口径共用)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AiVendor {
    Claude,
    OpenAi,
    Pi,
    Gemini,
    OpenCode,
    Grok,
    Qwen,
    DeepSeek,
    Zhipu,
    Copilot,
    Ollama,
}

impl AiVendor {
    /// 前端 `AiVendor` 的字符串值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Pi => "pi",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Grok => "grok",
            Self::Qwen => "qwen",
            Self::DeepSeek => "deepseek",
            Self::Zhipu => "zhipu",
            Self::Copilot => "copilot",
            Self::Ollama => "ollama",
        }
    }

    // 与 `mt_app::tree::PaneStatus::from_str` 取同一个命名(返回 Option 而非 Result,
    // 失败没有可报的错误细节),不实现 `FromStr` trait
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "claude" => Self::Claude,
            "openai" => Self::OpenAi,
            "pi" => Self::Pi,
            "gemini" => Self::Gemini,
            "opencode" => Self::OpenCode,
            "grok" => Self::Grok,
            "qwen" => Self::Qwen,
            "deepseek" => Self::DeepSeek,
            "zhipu" => Self::Zhipu,
            "copilot" => Self::Copilot,
            "ollama" => Self::Ollama,
            _ => return None,
        })
    }

    /// CLI 类型 → 厂商(前端 `inferVendor.ts` 的 `CLI_VENDOR`)。
    ///
    /// 只有会话记录能解析的三家在表里;其余 CLI 返回 `None`,由调用方走
    /// [`Self::infer`] 或回退通用图标。
    pub fn from_session_type(session_type: &str) -> Option<Self> {
        match session_type {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::OpenAi),
            "grok" => Some(Self::Grok),
            _ => None,
        }
    }

    /// 会话的厂商口径(前端 `vendorForSession`):**最新模型名优先**。
    ///
    /// claude CLI 挂 GLM/DeepSeek 中转是常见用法,CLI ≠ 模型厂商;模型识别不出
    /// 才回落 CLI 图标。**pane tab 刻意不用这个口径**(它表达「跑的是哪个 CLI」)。
    pub fn for_session(session_type: &str, model: Option<&str>) -> Option<Self> {
        model
            .and_then(|m| Self::infer(None, Some(m)))
            .or_else(|| Self::from_session_type(session_type))
    }

    /// 从 hook 上报的 agent 名 / 启动命令文本推断厂商。
    ///
    /// 规则与优先级逐条照抄 `src/utils/inferVendor.ts` 的 `RULES`(顺序即优先级):
    /// pi 最前(多模型 harness,`pi --model claude-…` 该显示 harness),
    /// openai 最后(关键词面最宽,`gpt` / `o1`~`o4` 容易误伤)。
    /// 词边界与 JS 的 `\b` 同义 —— `[A-Za-z0-9_]` 才算词字符,所以 `copilot` 里的
    /// `pi` 不会被 `\bpi\b` 命中。
    pub fn infer(agent: Option<&str>, command: Option<&str>) -> Option<Self> {
        for source in [agent, command].into_iter().flatten() {
            let hay = source.to_ascii_lowercase();
            for (vendor, needles) in RULES {
                if needles.iter().any(|n| n.hits(&hay)) {
                    return Some(*vendor);
                }
            }
        }
        None
    }

    /// 展示名(专有名词,不进 i18n)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::OpenAi => "OpenAI",
            Self::Pi => "pi",
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
            Self::Grok => "Grok",
            Self::Qwen => "Qwen",
            Self::DeepSeek => "DeepSeek",
            Self::Zhipu => "Zhipu",
            Self::Copilot => "GitHub Copilot",
            Self::Ollama => "Ollama",
        }
    }

    fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Claude => CLAUDE,
            Self::OpenAi => OPENAI,
            Self::Pi => PI,
            Self::Gemini => GEMINI,
            Self::OpenCode => OPENCODE,
            Self::Grok => GROK,
            Self::Qwen => QWEN,
            Self::DeepSeek => DEEPSEEK,
            Self::Zhipu => ZHIPU,
            Self::Copilot => COPILOT,
            Self::Ollama => OLLAMA,
        }
    }
}

/// 一条关键词的匹配方式。前端那张表里除了 `chatglm` 全是带 `\b` 的词匹配。
#[derive(Clone, Copy)]
enum Needle {
    /// `\bword\b`
    Word(&'static str),
    /// 裸子串(前端的 `chatglm`:后面常直接跟版本号,没有词边界)
    Substr(&'static str),
}

impl Needle {
    fn hits(self, hay: &str) -> bool {
        match self {
            Needle::Word(w) => word_match(hay, w),
            Needle::Substr(s) => hay.contains(s),
        }
    }
}

/// 顺序即优先级,与 `inferVendor.ts` 的 `RULES` 逐行对应。
///
/// **顺序是语义的一部分**:pi 在最前(多模型 harness),openai 在最后
/// (`gpt` / `o1`~`o4` 面最宽,提前会把 `glm` 挂 codex 之类的组合判错);
/// zhipu 的 `chatglm` 必须留在 zhipu 这一行而不是挪到表尾,否则
/// `chatglm3 + ollama` 这种串会先命中 ollama。
const RULES: &[(AiVendor, &[Needle])] = &[
    (AiVendor::Pi, &[Needle::Word("pi")]),
    (
        AiVendor::Claude,
        &[Needle::Word("claude"), Needle::Word("anthropic")],
    ),
    (AiVendor::Gemini, &[Needle::Word("gemini")]),
    (AiVendor::OpenCode, &[Needle::Word("opencode")]),
    (
        AiVendor::Grok,
        &[Needle::Word("grok"), Needle::Word("xai")],
    ),
    (
        AiVendor::Qwen,
        &[Needle::Word("qwen"), Needle::Word("dashscope")],
    ),
    (AiVendor::DeepSeek, &[Needle::Word("deepseek")]),
    (
        AiVendor::Zhipu,
        &[
            Needle::Word("glm"),
            Needle::Word("zhipu"),
            Needle::Substr("chatglm"),
        ],
    ),
    (AiVendor::Copilot, &[Needle::Word("copilot")]),
    (AiVendor::Ollama, &[Needle::Word("ollama")]),
    (
        AiVendor::OpenAi,
        &[
            Needle::Word("codex"),
            Needle::Word("openai"),
            Needle::Word("gpt"),
            Needle::Word("o1"),
            Needle::Word("o2"),
            Needle::Word("o3"),
            Needle::Word("o4"),
        ],
    ),
];

/// JS 正则 `\bword\b` 的等价判定。`\w` 是 ASCII 的 `[A-Za-z0-9_]`,
/// 所以中文字符算「非词字符」,`跑claude吧` 里的 claude 是能命中的(与前端一致)。
fn word_match(hay: &str, needle: &str) -> bool {
    let (hb, nb) = (hay.as_bytes(), needle.as_bytes());
    if nb.is_empty() || hb.len() < nb.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for start in 0..=hb.len() - nb.len() {
        if &hb[start..start + nb.len()] != nb {
            continue;
        }
        let left_ok = start == 0 || !is_word(hb[start - 1]);
        let end = start + nb.len();
        let right_ok = end == hb.len() || !is_word(hb[end]);
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

// ───────────────────────── 形状表 ─────────────────────────
//
// 坐标在 0..1 单位方框内,见 [`super::vector`]。

/// Claude:八芒星「日冕」。等分八向的锥形芒,长短交替(对角略短)。
const CLAUDE: &[Shape] = &[
    Shape::line(CLAY, 0.13, Geom::Polyline(&[(0.50, 0.50), (0.94, 0.50)])),
    Shape::line(CLAY, 0.13, Geom::Polyline(&[(0.50, 0.50), (0.06, 0.50)])),
    Shape::line(CLAY, 0.13, Geom::Polyline(&[(0.50, 0.50), (0.50, 0.94)])),
    Shape::line(CLAY, 0.13, Geom::Polyline(&[(0.50, 0.50), (0.50, 0.06)])),
    Shape::line(CLAY, 0.11, Geom::Polyline(&[(0.50, 0.50), (0.79, 0.79)])),
    Shape::line(CLAY, 0.11, Geom::Polyline(&[(0.50, 0.50), (0.21, 0.21)])),
    Shape::line(CLAY, 0.11, Geom::Polyline(&[(0.50, 0.50), (0.79, 0.21)])),
    Shape::line(CLAY, 0.11, Geom::Polyline(&[(0.50, 0.50), (0.21, 0.79)])),
];
/// Claude 官方橙(clay)。
const CLAY: Ink = Ink::Rgb(0xd9, 0x77, 0x57);

/// OpenAI(codex):三个互成 60° 的椭圆环叠出六瓣结 —— 官方 knot 的几何骨架。
/// 色值取原版 `MONO_BRAND_COLORS.openai`。
const OPENAI: &[Shape] = &[
    Shape::line(
        OPENAI_GREEN,
        0.085,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.45, 0.235),
            tilt: 0.0,
        },
    ),
    Shape::line(
        OPENAI_GREEN,
        0.085,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.45, 0.235),
            tilt: 60.0,
        },
    ),
    Shape::line(
        OPENAI_GREEN,
        0.085,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.45, 0.235),
            tilt: 120.0,
        },
    ),
];
const OPENAI_GREEN: Ink = Ink::Rgb(0x10, 0xa3, 0x7f);

/// pi(pi.dev)官方标记 —— **逐点等价**。
///
/// `BrandIcon.tsx` 里的原始 path 是 `viewBox="165.29 165.29 469.43 469.43"` 下的
/// 轴对齐矩形,坐标恰好落在 1/4 格上((v-165.29)/469.43 ∈ {0, ¼, ½, ¾, 1}),
/// 于是 evenodd 的那个洞可以无损拆成四块矩形的并集。
const PI: &[Shape] = &[
    // 顶横:x 0..¾, y 0..¼
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.0,
            y: 0.0,
            w: 0.75,
            h: 0.25,
            round: 0.0,
        },
    ),
    // 左竖:x 0..¼, y ¼..1
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.0,
            y: 0.25,
            w: 0.25,
            h: 0.75,
            round: 0.0,
        },
    ),
    // 中竖上段:x ½..¾, y ¼..½
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.5,
            y: 0.25,
            w: 0.25,
            h: 0.25,
            round: 0.0,
        },
    ),
    // 阶梯:x ¼..½, y ½..¾
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.25,
            y: 0.5,
            w: 0.25,
            h: 0.25,
            round: 0.0,
        },
    ),
    // 右竖(原 path 的第二段):x ¾..1, y ½..1
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.75,
            y: 0.5,
            w: 0.25,
            h: 0.5,
            round: 0.0,
        },
    ),
];

/// Gemini:四角星(sparkle)。
const GEMINI: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x42, 0x85, 0xf4),
    Geom::Polygon(&[
        (0.50, 0.02),
        (0.60, 0.40),
        (0.98, 0.50),
        (0.60, 0.60),
        (0.50, 0.98),
        (0.40, 0.60),
        (0.02, 0.50),
        (0.40, 0.40),
    ]),
)];

/// OpenCode:终端提示符(`>_`)。官方 logo 为纯黑,跟随主题色(与原版 Mono 同)。
const OPENCODE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Rect {
            x: 0.06,
            y: 0.12,
            w: 0.88,
            h: 0.76,
            round: 0.14,
        },
    ),
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Polyline(&[(0.26, 0.34), (0.46, 0.50), (0.26, 0.66)]),
    ),
    Shape::line(Ink::Current, 0.09, Geom::Polyline(&[(0.54, 0.68), (0.74, 0.68)])),
];

/// Grok(xAI):两片斜刃交叉的 X。官方为纯黑,跟随主题色(与原版 Mono 同)。
const GROK: &[Shape] = &[
    Shape::fill(
        Ink::Current,
        Geom::Polygon(&[(0.06, 0.06), (0.28, 0.06), (0.94, 0.94), (0.72, 0.94)]),
    ),
    Shape::fill(
        Ink::Current,
        Geom::Polygon(&[(0.94, 0.06), (0.72, 0.06), (0.06, 0.94), (0.28, 0.94)]),
    ),
];

/// Qwen:品牌色圆角片 + 内嵌菱形。
const QWEN: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x61, 0x5c, 0xed),
        Geom::Rect {
            x: 0.04,
            y: 0.04,
            w: 0.92,
            h: 0.92,
            round: 0.24,
        },
    ),
    Shape::fill(
        Ink::Contrast,
        Geom::Polygon(&[(0.50, 0.20), (0.80, 0.50), (0.50, 0.80), (0.20, 0.50)]),
    ),
];

/// DeepSeek:品牌色圆角片 + 内嵌波纹(取其「鲸」的水波意象)。
const DEEPSEEK: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x4d, 0x6b, 0xfe),
        Geom::Rect {
            x: 0.04,
            y: 0.04,
            w: 0.92,
            h: 0.92,
            round: 0.24,
        },
    ),
    Shape::line(
        Ink::Contrast,
        0.10,
        Geom::Polyline(&[(0.18, 0.58), (0.34, 0.40), (0.50, 0.58), (0.66, 0.40), (0.82, 0.58)]),
    ),
];

/// 智谱:品牌色圆角片 + 内嵌三角。
const ZHIPU: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x38, 0x59, 0xff),
        Geom::Rect {
            x: 0.04,
            y: 0.04,
            w: 0.92,
            h: 0.92,
            round: 0.24,
        },
    ),
    Shape::fill(
        Ink::Contrast,
        Geom::Polygon(&[(0.50, 0.20), (0.82, 0.76), (0.18, 0.76)]),
    ),
];

/// GitHub Copilot:护目镜式的头。色值取原版 `MONO_BRAND_COLORS.copilot`。
const COPILOT: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x89, 0x57, 0xe5),
        Geom::Rect {
            x: 0.06,
            y: 0.26,
            w: 0.88,
            h: 0.52,
            round: 0.26,
        },
    ),
    Shape::fill(Ink::Contrast, Geom::Circle { c: (0.33, 0.52), r: 0.09 }),
    Shape::fill(Ink::Contrast, Geom::Circle { c: (0.67, 0.52), r: 0.09 }),
    // 两只耳朵:让它在小尺寸下不至于退化成一颗胶囊
    Shape::fill(
        Ink::Rgb(0x89, 0x57, 0xe5),
        Geom::Polygon(&[(0.20, 0.30), (0.38, 0.14), (0.44, 0.30)]),
    ),
    Shape::fill(
        Ink::Rgb(0x89, 0x57, 0xe5),
        Geom::Polygon(&[(0.80, 0.30), (0.62, 0.14), (0.56, 0.30)]),
    ),
];

/// Ollama:羊驼剪影(头 + 两耳)。官方为纯黑,跟随主题色。
const OLLAMA: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Rect {
            x: 0.22,
            y: 0.36,
            w: 0.56,
            h: 0.56,
            round: 0.22,
        },
    ),
    Shape::line(Ink::Current, 0.09, Geom::Polyline(&[(0.30, 0.36), (0.26, 0.08)])),
    Shape::line(Ink::Current, 0.09, Geom::Polyline(&[(0.70, 0.36), (0.74, 0.08)])),
];

/// 识别不出厂商时的通用机器人(对齐原版回退到 lucide `Bot`)。
pub const UNKNOWN_BOT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.08,
        Geom::Rect {
            x: 0.10,
            y: 0.30,
            w: 0.80,
            h: 0.60,
            round: 0.16,
        },
    ),
    Shape::line(Ink::Current, 0.08, Geom::Polyline(&[(0.50, 0.30), (0.50, 0.12)])),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.50, 0.09), r: 0.07 }),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.34, 0.58), r: 0.07 }),
    Shape::fill(Ink::Current, Geom::Circle { c: (0.66, 0.58), r: 0.07 }),
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    let mut out: Vec<&'static [Shape]> = ALL_VENDORS.iter().map(|v| v.shapes()).collect();
    out.push(UNKNOWN_BOT);
    out
}

/// 全部厂商(设置页/演示列表用)。
pub const ALL_VENDORS: &[AiVendor] = &[
    AiVendor::Claude,
    AiVendor::OpenAi,
    AiVendor::Pi,
    AiVendor::Gemini,
    AiVendor::OpenCode,
    AiVendor::Grok,
    AiVendor::Qwen,
    AiVendor::DeepSeek,
    AiVendor::Zhipu,
    AiVendor::Copilot,
    AiVendor::Ollama,
];

/// 厂商图标。`vendor` 为 `None` 时画通用机器人(与原版回退 lucide `Bot` 同)。
///
/// ```ignore
/// BrandIcon::new(AiVendor::from_session_type(&pane.agent)).size(px(13.0))
/// ```
#[derive(IntoElement)]
pub struct BrandIcon {
    vendor: Option<AiVendor>,
    size: Pixels,
    /// `Ink::Current` 的取色(Grok / OpenCode / Ollama / pi / 未知回退跟这个走)。
    color: Option<Hsla>,
    /// `Ink::Contrast` 的取色(品牌色片上的内嵌字形)。
    contrast: Option<Hsla>,
}

impl BrandIcon {
    /// 默认 13px —— 与 `BrandIcon.tsx` 的 `size = 13` 一致。
    pub fn new(vendor: Option<AiVendor>) -> Self {
        Self {
            vendor,
            size: px(13.0),
            color: None,
            contrast: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    pub fn contrast(mut self, color: Hsla) -> Self {
        self.contrast = Some(color);
        self
    }
}

impl RenderOnce for BrandIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let shapes = self.vendor.map(AiVendor::shapes).unwrap_or(UNKNOWN_BOT);
        let mut icon = VectorIcon::new(shapes, self.size);
        if let Some(c) = self.color {
            icon = icon.ink(c);
        }
        if let Some(c) = self.contrast {
            icon = icon.contrast(c);
        }
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 厂商字符串与前端一字不差() {
        for v in ALL_VENDORS {
            assert_eq!(AiVendor::from_str(v.as_str()), Some(*v));
        }
        assert_eq!(AiVendor::from_str("mistral"), None);
    }

    #[test]
    fn 推断规则的优先级照抄前端() {
        // pi 最前:多模型 harness 该显示 harness 而不是被代理的模型
        assert_eq!(
            AiVendor::infer(None, Some("pi --model claude-sonnet-5")),
            Some(AiVendor::Pi)
        );
        // 反向不误伤:copilot / pip / ping 里的 pi 不在词边界上
        assert_eq!(
            AiVendor::infer(None, Some("gh copilot suggest")),
            Some(AiVendor::Copilot)
        );
        assert_eq!(AiVendor::infer(None, Some("pip install x")), None);
        assert_eq!(AiVendor::infer(None, Some("ping localhost")), None);
        // agent 优先于 command
        assert_eq!(
            AiVendor::infer(Some("claude"), Some("codex exec")),
            Some(AiVendor::Claude)
        );
        // openai 面最宽,放最后
        assert_eq!(AiVendor::infer(None, Some("codex")), Some(AiVendor::OpenAi));
        assert_eq!(AiVendor::infer(None, Some("gpt-5-mini")), Some(AiVendor::OpenAi));
        assert_eq!(AiVendor::infer(None, Some("o3-pro")), Some(AiVendor::OpenAi));
        // o1~o4 的词边界:foo3 不该命中
        assert_eq!(AiVendor::infer(None, Some("foo3")), None);
        // chatglm 后接版本号没有词边界,单独放行
        assert_eq!(AiVendor::infer(None, Some("chatglm3")), Some(AiVendor::Zhipu));
        // chatglm 留在 zhipu 那一行(优先级 8),不能被后面的 ollama 抢走
        assert_eq!(
            AiVendor::infer(None, Some("chatglm3 via ollama")),
            Some(AiVendor::Zhipu)
        );
        assert_eq!(AiVendor::infer(None, Some("glm-4.6")), Some(AiVendor::Zhipu));
        // xai 是 grok 的别名
        assert_eq!(AiVendor::infer(None, Some("XAI_API_KEY")), None, "下划线是词字符");
        assert_eq!(AiVendor::infer(None, Some("xai grok-4")), Some(AiVendor::Grok));
    }

    #[test]
    fn 会话口径是模型优先_cli_兜底() {
        // claude CLI 挂 GLM 中转:图标该是智谱
        assert_eq!(
            AiVendor::for_session("claude", Some("glm-4.6")),
            Some(AiVendor::Zhipu)
        );
        // 模型识别不出 → 回落 CLI
        assert_eq!(
            AiVendor::for_session("codex", Some("some-internal-model")),
            Some(AiVendor::OpenAi)
        );
        assert_eq!(AiVendor::for_session("grok", None), Some(AiVendor::Grok));
        // 没有会话记录的 agent 没有 CLI 兜底
        assert_eq!(AiVendor::for_session("opencode", None), None);
    }

    #[test]
    fn 词边界与_js_的_b_同义() {
        assert!(word_match("run claude now", "claude"));
        assert!(word_match("claude", "claude"));
        assert!(word_match("跑claude吧", "claude"), "非 ASCII 算非词字符");
        assert!(!word_match("claudex", "claude"));
        assert!(!word_match("my_claude", "claude"), "下划线是词字符");
        assert!(!word_match("x", "claude"));
    }

    #[test]
    fn pi_标记与官方_path_逐点等价() {
        // 官方 path(BrandIcon.tsx)在 viewBox 165.29..634.72 下,五块矩形的
        // 单位坐标必须落在 1/4 格上;抽样几个点验证覆盖与挖空
        let inside = |x: f32, y: f32| {
            PI.iter().any(|s| match s.geom {
                Geom::Rect { x: rx, y: ry, w, h, .. } => {
                    x >= rx && x <= rx + w && y >= ry && y <= ry + h
                }
                _ => false,
            })
        };
        assert!(inside(0.4, 0.1), "顶横");
        assert!(inside(0.1, 0.9), "左竖");
        assert!(inside(0.6, 0.3), "中竖上段");
        assert!(inside(0.3, 0.6), "阶梯");
        assert!(inside(0.9, 0.9), "右竖");
        assert!(!inside(0.35, 0.35), "evenodd 的洞必须还是洞");
        assert!(!inside(0.9, 0.1), "右上角是空的");
        assert!(!inside(0.6, 0.9), "右下的空档");
    }

    #[test]
    fn 每家都有形状_没有空表() {
        for v in ALL_VENDORS {
            assert!(!v.shapes().is_empty(), "{} 没有形状", v.label());
        }
        assert!(!UNKNOWN_BOT.is_empty());
    }
}
