//! AI 厂商图标(对照 `src/components/BrandIcon.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版用 `@lobehub/icons` 的深路径 import 拿官方 logo(Color / Mono 两档)。
//! 那是 npm 包里的 SVG,GPUI 侧**把每家的那条 `d` 原样搬了过来** —— 形状是官方
//! 几何本身,不是简化标记;解析与离散见 [`super::svg_path`],
//! 填充规则(nonzero / evenodd)与颜色也照抄原版,含原版 `MONO_BRAND_COLORS`
//! 那条口径(官方 logo 本为黑白的品牌借主色提辨识度,其余跟随主题)。
//!
//! pi 不在 lobehub 里(`@lobehub/icons/es/Pi` 是 Inflection AI 的 pi.ai,与本
//! agent 无从属关系),它的官方标记内联在 `BrandIcon.tsx`,一并照搬。
//!
//! 商标注意(原样保留原版的红线):品牌 logo 仅作「该会话属于哪家 AI」的指示性使用,
//! 不得用作产品自身标识。
//!
//! # 已知偏差
//!
//! 只剩「渐变」这一类 —— `window.paint_path` 一次只吃一个纯色:
//!
//! - **Gemini**:原版是同一条 path 叠四层(底色 + 三道线性渐变),这里只画底色层;
//! - **Qwen**:原版是双色线性渐变,这里取中点纯色 + 同一透明度。
//!
//! 其余各家原版本就是单色填充,几何与颜色都是**逐点等价**。
//!
//! # 宿主接线(mt-app)
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
    /// oh-my-pi(omp,pi 的分支):GPUI 版新增,原版前端没有这一项
    Omp,
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
            Self::Omp => "omp",
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
            "omp" => Self::Omp,
            _ => return None,
        })
    }

    /// CLI 类型 → 厂商(前端 `inferVendor.ts` 的 `CLI_VENDOR`)。
    ///
    /// 会话记录能解析的三家,加上 hook 会上报 agent 名的 omp 在表里;其余 CLI
    /// 返回 `None`,由调用方走 [`Self::infer`] 或回退通用图标。
    pub fn from_session_type(session_type: &str) -> Option<Self> {
        match session_type {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::OpenAi),
            "grok" => Some(Self::Grok),
            "omp" => Some(Self::Omp),
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
            Self::Omp => "oh-my-pi",
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
            Self::Omp => OMP,
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
    // omp 必须排在 pi 前面:`oh-my-pi` 末尾的 `pi` 正落在词边界上,后置就被 pi 抢走
    (
        AiVendor::Omp,
        &[Needle::Word("omp"), Needle::Word("oh-my-pi")],
    ),
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
// 全部是**原版那条 path 原样搬过来**。`d` 字符串逐字符取自
// `node_modules/@lobehub/icons/es/{Brand}/components/{Color,Mono}.js`
// (pi 取自 `BrandIcon.tsx` 里内联的官方标记),解析与离散见 [`super::svg_path`]。
//
// 三条固定口径,改动前先看清楚:
// - **viewBox**:lobehub 全家都是 `0 0 24 24`;pi 是 `165.29 165.29 469.43 469.43`;
// - **fill-rule**:lobehub 的 Mono 变体在 `<svg>` 上挂 `fillRule="evenodd"`,
//   Color 变体多为 `nonzero` —— 这条**不能想当然**,Copilot 的眼睛、OpenCode 的
//   内框就是靠 evenodd 挖出来的,填错规则会变成两坨实心;
// - **颜色**:Color 变体用官方色值;Mono 变体本为黑白,沿用原版
//   `MONO_BRAND_COLORS` 的口径(只有 openai / copilot 借品牌主色,其余跟随主题)。

/// lobehub 全家的 viewBox。
const VB24: (f32, f32, f32) = (0.0, 0.0, 24.0);

/// Claude:官方 Color 变体(`Claude/components/Color.js`),八芒星「日冕」。
const CLAUDE: &[Shape] = &[Shape::fill(
    Ink::Rgb(0xd9, 0x77, 0x57),
    Geom::Path {
        d: "M4.709 15.955l4.72-2.647.08-.23-.08-.128H9.2l-.79-.048-2.698-.073-2.339-.097-2.266-.122-.571-.121L0 11.784l.055-.352.48-.321.686.06 1.52.103 2.278.158 1.652.097 2.449.255h.389l.055-.157-.134-.098-.103-.097-2.358-1.596-2.552-1.688-1.336-.972-.724-.491-.364-.462-.158-1.008.656-.722.881.06.225.061.893.686 1.908 1.476 2.491 1.833.365.304.145-.103.019-.073-.164-.274-1.355-2.446-1.446-2.49-.644-1.032-.17-.619a2.97 2.97 0 01-.104-.729L6.283.134 6.696 0l.996.134.42.364.62 1.414 1.002 2.229 1.555 3.03.456.898.243.832.091.255h.158V9.01l.128-1.706.237-2.095.23-2.695.08-.76.376-.91.747-.492.584.28.48.685-.067.444-.286 1.851-.559 2.903-.364 1.942h.212l.243-.242.985-1.306 1.652-2.064.73-.82.85-.904.547-.431h1.033l.76 1.129-.34 1.166-1.064 1.347-.881 1.142-1.264 1.7-.79 1.36.073.11.188-.02 2.856-.606 1.543-.28 1.841-.315.833.388.091.395-.328.807-1.969.486-2.309.462-3.439.813-.042.03.049.061 1.549.146.662.036h1.622l3.02.225.79.522.474.638-.079.485-1.215.62-1.64-.389-3.829-.91-1.312-.329h-.182v.11l1.093 1.068 2.006 1.81 2.509 2.33.127.578-.322.455-.34-.049-2.205-1.657-.851-.747-1.926-1.62h-.128v.17l.444.649 2.345 3.521.122 1.08-.17.353-.608.213-.668-.122-1.374-1.925-1.415-2.167-1.143-1.943-.14.08-.674 7.254-.316.37-.729.28-.607-.461-.322-.747.322-1.476.389-1.924.315-1.53.286-1.9.17-.632-.012-.042-.14.018-1.434 1.967-2.18 2.945-1.726 1.845-.414.164-.717-.37.067-.662.401-.589 2.388-3.036 1.44-1.882.93-1.086-.006-.158h-.055L4.132 18.56l-1.13.146-.487-.456.061-.746.231-.243 1.908-1.312-.006.006z",
        view: VB24,
        even_odd: false,
    },
)];

/// OpenAI(codex):官方 Mono 变体的六瓣结。色值取原版 `MONO_BRAND_COLORS.openai`。
const OPENAI: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x10, 0xa3, 0x7f),
    Geom::Path {
        d: "M9.205 8.658v-2.26c0-.19.072-.333.238-.428l4.543-2.616c.619-.357 1.356-.523 2.117-.523 2.854 0 4.662 2.212 4.662 4.566 0 .167 0 .357-.024.547l-4.71-2.759a.797.797 0 00-.856 0l-5.97 3.473zm10.609 8.8V12.06c0-.333-.143-.57-.429-.737l-5.97-3.473 1.95-1.118a.433.433 0 01.476 0l4.543 2.617c1.309.76 2.189 2.378 2.189 3.948 0 1.808-1.07 3.473-2.76 4.163zM7.802 12.703l-1.95-1.142c-.167-.095-.239-.238-.239-.428V5.899c0-2.545 1.95-4.472 4.591-4.472 1 0 1.927.333 2.712.928L8.23 5.067c-.285.166-.428.404-.428.737v6.898zM12 15.128l-2.795-1.57v-3.33L12 8.658l2.795 1.57v3.33L12 15.128zm1.796 7.23c-1 0-1.927-.332-2.712-.927l4.686-2.712c.285-.166.428-.404.428-.737v-6.898l1.974 1.142c.167.095.238.238.238.428v5.233c0 2.545-1.974 4.472-4.614 4.472zm-5.637-5.303l-4.544-2.617c-1.308-.761-2.188-2.378-2.188-3.948A4.482 4.482 0 014.21 6.327v5.423c0 .333.143.571.428.738l5.947 3.449-1.95 1.118a.432.432 0 01-.476 0zm-.262 3.9c-2.688 0-4.662-2.021-4.662-4.519 0-.19.024-.38.047-.57l4.686 2.71c.286.167.571.167.856 0l5.97-3.448v2.26c0 .19-.07.333-.237.428l-4.543 2.616c-.619.357-1.356.523-2.117.523zm5.899 2.83a5.947 5.947 0 005.827-4.756C22.287 18.339 24 15.84 24 13.296c0-1.665-.713-3.282-1.998-4.448.119-.5.19-.999.19-1.498 0-3.401-2.759-5.947-5.946-5.947-.642 0-1.26.095-1.88.31A5.962 5.962 0 0010.205 0a5.947 5.947 0 00-5.827 4.757C1.713 5.447 0 7.945 0 10.49c0 1.666.713 3.283 1.998 4.448-.119.5-.19 1-.19 1.499 0 3.401 2.759 5.946 5.946 5.946.642 0 1.26-.095 1.88-.309a5.96 5.96 0 004.162 1.713z",
        view: VB24,
        even_odd: true,
    },
)];

/// pi(pi.dev)官方标记,取自 `BrandIcon.tsx` 内联的那两条 path。
///
/// viewBox 收到图形自身的包围盒(与原版同一裁剪)—— 原始 800×800 画布留白 20%,
/// 不裁的话并排时比其他品牌图标明显小一圈。第一条 evenodd 挖出那个方洞。
const PI: &[Shape] = &[
    Shape::fill(
        Ink::Current,
        Geom::Path {
            d: "M165.29 165.29H517.36V400H400V517.36H282.65V634.72H165.29ZM282.65 282.65V400H400V282.65Z",
            view: VB_PI,
            even_odd: true,
        },
    ),
    Shape::fill(
        Ink::Current,
        Geom::Path {
            d: "M517.36 400H634.72V634.72H517.36Z",
            view: VB_PI,
            even_odd: false,
        },
    ),
];
const VB_PI: (f32, f32, f32) = (165.29, 165.29, 469.43);

/// oh-my-pi(omp)官方标记:π 加一枚插头(仓库 `assets/icon.svg`,120×90 画布)。
///
/// 原件全是轴对齐的圆角矩形,这里按基本形逐个搬(不用 path):坐标除以 120、
/// 纵向把 90 高的图形居中到方框(y 加 15)。π 本体跟随主题色(原件是近白色,
/// 亮色主题下会消失);插头保留品牌橙 `#f97316`,两根插脚是原件的深色。
/// 原件左右各一枚 2px 的装饰小圆点略去 —— 13px 上只剩噪点。
const OMP: &[Shape] = &[
    // 横杠
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.0833,
            y: 0.1917,
            w: 0.8333,
            h: 0.1,
            round: 0.0167,
        },
    ),
    // 左腿
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.2083,
            y: 0.2917,
            w: 0.1,
            h: 0.5167,
            round: 0.0167,
        },
    ),
    // 右腿(短一截,给插头让位)
    Shape::fill(
        Ink::Current,
        Geom::Rect {
            x: 0.625,
            y: 0.2917,
            w: 0.1,
            h: 0.375,
            round: 0.0167,
        },
    ),
    // 插头
    Shape::fill(
        Ink::Rgb(0xf9, 0x73, 0x16),
        Geom::Rect {
            x: 0.5917,
            y: 0.5833,
            w: 0.1667,
            h: 0.1333,
            round: 0.025,
        },
    ),
    // 两根插脚
    Shape::fill(
        Ink::Rgb(0x0d, 0x0d, 0x0d),
        Geom::Rect {
            x: 0.6333,
            y: 0.6167,
            w: 0.025,
            h: 0.0667,
            round: 0.0083,
        },
    ),
    Shape::fill(
        Ink::Rgb(0x0d, 0x0d, 0x0d),
        Geom::Rect {
            x: 0.6833,
            y: 0.6167,
            w: 0.025,
            h: 0.0667,
            round: 0.0083,
        },
    ),
];

/// Gemini:官方 Color 变体的四角星。
///
/// **已知偏差**:原版是同一条 path 叠四层 —— 底色 `#3186FF` 加三道线性渐变
/// (`#08B962` / `#F94543` / `#FABC12`,末端 stop-opacity 都是 0)。
/// `paint_path` 一次只能给一个纯色,渐变叠不出来,这里只画底色那层。
const GEMINI: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x31, 0x86, 0xff),
    Geom::Path {
        d: "M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z",
        view: VB24,
        even_odd: false,
    },
)];

/// OpenCode:官方 Mono 变体 —— 外框套内框的两条子路径,靠 evenodd 挖空。
/// 官方 logo 为纯黑,跟随主题色(与原版 Mono 同)。
const OPENCODE: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Path {
        d: "M16 6H8v12h8V6zm4 16H4V2h16v20z",
        view: VB24,
        even_odd: true,
    },
)];

/// Grok(xAI):官方 Mono 变体的斜杠 + 挑锋(**不是** X)。跟随主题色。
const GROK: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Path {
        d: "M9.27 15.29l7.978-5.897c.391-.29.95-.177 1.137.272.98 2.369.542 5.215-1.41 7.169-1.951 1.954-4.667 2.382-7.149 1.406l-2.711 1.257c3.889 2.661 8.611 2.003 11.562-.953 2.341-2.344 3.066-5.539 2.388-8.42l.006.007c-.983-4.232.242-5.924 2.75-9.383.06-.082.12-.164.179-.248l-3.301 3.305v-.01L9.267 15.292M7.623 16.723c-2.792-2.67-2.31-6.801.071-9.184 1.761-1.763 4.647-2.483 7.166-1.425l2.705-1.25a7.808 7.808 0 00-1.829-1A8.975 8.975 0 005.984 5.83c-2.533 2.536-3.33 6.436-1.962 9.764 1.022 2.487-.653 4.246-2.34 6.022-.599.63-1.199 1.259-1.682 1.925l7.62-6.815",
        view: VB24,
        even_odd: true,
    },
)];

/// Qwen:官方 Color 变体。
///
/// **已知偏差**:原版填的是 `#6336E7 → #6F69F7` 的横向线性渐变(两端 opacity .84),
/// 这里取渐变中点的纯色 + 同一透明度。
const QWEN: &[Shape] = &[Shape::fill(
    Ink::RgbAlpha(0x69, 0x50, 0xef, 0.84),
    Geom::Path {
        d: "M12.604 1.34c.393.69.784 1.382 1.174 2.075a.18.18 0 00.157.091h5.552c.174 0 .322.11.446.327l1.454 2.57c.19.337.24.478.024.837-.26.43-.513.864-.76 1.3l-.367.658c-.106.196-.223.28-.04.512l2.652 4.637c.172.301.111.494-.043.77-.437.785-.882 1.564-1.335 2.34-.159.272-.352.375-.68.37-.777-.016-1.552-.01-2.327.016a.099.099 0 00-.081.05 575.097 575.097 0 01-2.705 4.74c-.169.293-.38.363-.725.364-.997.003-2.002.004-3.017.002a.537.537 0 01-.465-.271l-1.335-2.323a.09.09 0 00-.083-.049H4.982c-.285.03-.553-.001-.805-.092l-1.603-2.77a.543.543 0 01-.002-.54l1.207-2.12a.198.198 0 000-.197 550.951 550.951 0 01-1.875-3.272l-.79-1.395c-.16-.31-.173-.496.095-.965.465-.813.927-1.625 1.387-2.436.132-.234.304-.334.584-.335a338.3 338.3 0 012.589-.001.124.124 0 00.107-.063l2.806-4.895a.488.488 0 01.422-.246c.524-.001 1.053 0 1.583-.006L11.704 1c.341-.003.724.032.9.34zm-3.432.403a.06.06 0 00-.052.03L6.254 6.788a.157.157 0 01-.135.078H3.253c-.056 0-.07.025-.041.074l5.81 10.156c.025.042.013.062-.034.063l-2.795.015a.218.218 0 00-.2.116l-1.32 2.31c-.044.078-.021.118.068.118l5.716.008c.046 0 .08.02.104.061l1.403 2.454c.046.081.092.082.139 0l5.006-8.76.783-1.382a.055.055 0 01.096 0l1.424 2.53a.122.122 0 00.107.062l2.763-.02a.04.04 0 00.035-.02.041.041 0 000-.04l-2.9-5.086a.108.108 0 010-.113l.293-.507 1.12-1.977c.024-.041.012-.062-.035-.062H9.2c-.059 0-.073-.026-.043-.077l1.434-2.505a.107.107 0 000-.114L9.225 1.774a.06.06 0 00-.053-.031zm6.29 8.02c.046 0 .058.02.034.06l-.832 1.465-2.613 4.585a.056.056 0 01-.05.029.058.058 0 01-.05-.029L8.498 9.841c-.02-.034-.01-.052.028-.054l.216-.012 6.722-.012z",
        view: VB24,
        even_odd: false,
    },
)];

/// DeepSeek:官方 Color 变体的鲸。
const DEEPSEEK: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x4d, 0x6b, 0xfe),
    Geom::Path {
        d: "M23.748 4.482c-.254-.124-.364.113-.512.234-.051.039-.094.09-.137.136-.372.397-.806.657-1.373.626-.829-.046-1.537.214-2.163.848-.133-.782-.575-1.248-1.247-1.548-.352-.156-.708-.311-.955-.65-.172-.241-.219-.51-.305-.774-.055-.16-.11-.323-.293-.35-.2-.031-.278.136-.356.276-.313.572-.434 1.202-.422 1.84.027 1.436.633 2.58 1.838 3.393.137.093.172.187.129.323-.082.28-.18.552-.266.833-.055.179-.137.217-.329.14a5.526 5.526 0 01-1.736-1.18c-.857-.828-1.631-1.742-2.597-2.458a11.365 11.365 0 00-.689-.471c-.985-.957.13-1.743.388-1.836.27-.098.093-.432-.779-.428-.872.004-1.67.295-2.687.684a3.055 3.055 0 01-.465.137 9.597 9.597 0 00-2.883-.102c-1.885.21-3.39 1.102-4.497 2.623C.082 8.606-.231 10.684.152 12.85c.403 2.284 1.569 4.175 3.36 5.653 1.858 1.533 3.997 2.284 6.438 2.14 1.482-.085 3.133-.284 4.994-1.86.47.234.962.327 1.78.397.63.059 1.236-.03 1.705-.128.735-.156.684-.837.419-.961-2.155-1.004-1.682-.595-2.113-.926 1.096-1.296 2.746-2.642 3.392-7.003.05-.347.007-.565 0-.845-.004-.17.035-.237.23-.256a4.173 4.173 0 001.545-.475c1.396-.763 1.96-2.015 2.093-3.517.02-.23-.004-.467-.247-.588zM11.581 18c-2.089-1.642-3.102-2.183-3.52-2.16-.392.024-.321.471-.235.763.09.288.207.486.371.739.114.167.192.416-.113.603-.673.416-1.842-.14-1.897-.167-1.361-.802-2.5-1.86-3.301-3.307-.774-1.393-1.224-2.887-1.298-4.482-.02-.386.093-.522.477-.592a4.696 4.696 0 011.529-.039c2.132.312 3.946 1.265 5.468 2.774.868.86 1.525 1.887 2.202 2.891.72 1.066 1.494 2.082 2.48 2.914.348.292.625.514.891.677-.802.09-2.14.11-3.054-.614zm1-6.44a.306.306 0 01.415-.287.302.302 0 01.2.288.306.306 0 01-.31.307.303.303 0 01-.304-.308zm3.11 1.596c-.2.081-.399.151-.59.16a1.245 1.245 0 01-.798-.254c-.274-.23-.47-.358-.552-.758a1.73 1.73 0 01.016-.588c.07-.327-.008-.537-.239-.727-.187-.156-.426-.199-.688-.199a.559.559 0 01-.254-.078c-.11-.054-.2-.19-.114-.358.028-.054.16-.186.192-.21.356-.202.767-.136 1.146.016.352.144.618.408 1.001.782.391.451.462.576.685.914.176.265.336.537.445.848.067.195-.019.354-.25.452z",
        view: VB24,
        even_odd: false,
    },
)];

/// 智谱:官方 Color 变体。
const ZHIPU: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x38, 0x59, 0xff),
    Geom::Path {
        d: "M11.991 23.503a.24.24 0 00-.244.248.24.24 0 00.244.249.24.24 0 00.245-.249.24.24 0 00-.22-.247l-.025-.001zM9.671 5.365a1.697 1.697 0 011.099 2.132l-.071.172-.016.04-.018.054c-.07.16-.104.32-.104.498-.035.71.47 1.279 1.186 1.314h.366c1.309.053 2.338 1.173 2.286 2.523-.052 1.332-1.152 2.38-2.478 2.327h-.174c-.715.018-1.274.64-1.239 1.368 0 .124.018.23.053.337.209.373.54.658.96.8.75.23 1.517-.125 1.9-.782l.018-.035c.402-.64 1.17-.96 1.92-.711.854.284 1.378 1.226 1.099 2.167a1.661 1.661 0 01-2.077 1.102 1.711 1.711 0 01-.907-.711l-.017-.035c-.2-.323-.463-.58-.851-.711l-.056-.018a1.646 1.646 0 00-1.954.746 1.66 1.66 0 01-1.065.764 1.677 1.677 0 01-1.989-1.279c-.209-.906.332-1.83 1.257-2.043a1.51 1.51 0 01.296-.035h.018c.68-.071 1.151-.622 1.116-1.333a1.307 1.307 0 00-.227-.693 2.515 2.515 0 01-.366-1.403 2.39 2.39 0 01.366-1.208c.14-.195.21-.444.227-.693.018-.71-.506-1.261-1.186-1.332l-.07-.018a1.43 1.43 0 01-.299-.07l-.05-.019a1.7 1.7 0 01-1.047-2.114 1.68 1.68 0 012.094-1.101zm-5.575 10.11c.26-.264.639-.367.994-.27.355.096.633.379.728.74.095.362-.007.748-.267 1.013-.402.41-1.053.41-1.455 0a1.062 1.062 0 010-1.482zm14.845-.294c.359-.09.738.024.992.297.254.274.344.665.237 1.025-.107.36-.396.634-.756.718-.551.128-1.1-.22-1.23-.781a1.05 1.05 0 01.757-1.26zm-.064-4.39c.314.32.49.753.49 1.206 0 .452-.176.886-.49 1.206-.315.32-.74.5-1.185.5-.444 0-.87-.18-1.184-.5a1.727 1.727 0 010-2.412 1.654 1.654 0 012.369 0zm-11.243.163c.364.484.447 1.128.218 1.691a1.665 1.665 0 01-2.188.923c-.855-.36-1.26-1.358-.907-2.228a1.68 1.68 0 011.33-1.038c.593-.08 1.183.169 1.547.652zm11.545-4.221c.368 0 .708.2.892.524.184.324.184.724 0 1.048a1.026 1.026 0 01-.892.524c-.568 0-1.03-.47-1.03-1.048 0-.579.462-1.048 1.03-1.048zm-14.358 0c.368 0 .707.2.891.524.184.324.184.724 0 1.048a1.026 1.026 0 01-.891.524c-.569 0-1.03-.47-1.03-1.048 0-.579.461-1.048 1.03-1.048zm10.031-1.475c.925 0 1.675.764 1.675 1.706s-.75 1.705-1.675 1.705-1.674-.763-1.674-1.705c0-.942.75-1.706 1.674-1.706zm-2.626-.684c.362-.082.653-.356.761-.718a1.062 1.062 0 00-.238-1.028 1.017 1.017 0 00-.996-.294c-.547.14-.881.7-.752 1.257.13.558.675.907 1.225.783zm0 16.876c.359-.087.644-.36.75-.72a1.062 1.062 0 00-.237-1.019 1.018 1.018 0 00-.985-.301 1.037 1.037 0 00-.762.717c-.108.361-.017.754.239 1.028.245.263.606.377.953.305l.043-.01zM17.19 3.5a.631.631 0 00.628-.64c0-.355-.279-.64-.628-.64a.631.631 0 00-.628.64c0 .355.28.64.628.64zm-10.38 0a.631.631 0 00.628-.64c0-.355-.28-.64-.628-.64a.631.631 0 00-.628.64c0 .355.279.64.628.64zm-5.182 7.852a.631.631 0 00-.628.64c0 .354.28.639.628.639a.63.63 0 00.627-.606l.001-.034a.62.62 0 00-.628-.64zm5.182 9.13a.631.631 0 00-.628.64c0 .355.279.64.628.64a.631.631 0 00.628-.64c0-.355-.28-.64-.628-.64zm10.38.018a.631.631 0 00-.628.64c0 .355.28.64.628.64a.631.631 0 00.628-.64c0-.355-.279-.64-.628-.64zm5.182-9.148a.631.631 0 00-.628.64c0 .354.279.639.628.639a.631.631 0 00.628-.64c0-.355-.28-.64-.628-.64zm-.384-4.992a.24.24 0 00.244-.249.24.24 0 00-.244-.249.24.24 0 00-.244.249c0 .142.122.249.244.249zM11.991.497a.24.24 0 00.245-.248A.24.24 0 0011.99 0a.24.24 0 00-.244.249c0 .133.108.236.223.247l.021.001zM2.011 6.36a.24.24 0 00.245-.249.24.24 0 00-.244-.249.24.24 0 00-.244.249.24.24 0 00.244.249zm0 11.263a.24.24 0 00-.243.248.24.24 0 00.244.249.24.24 0 00.244-.249.252.252 0 00-.244-.248zm19.995-.018a.24.24 0 00-.245.248.24.24 0 00.245.25.24.24 0 00.244-.25.252.252 0 00-.244-.248z",
        view: VB24,
        even_odd: false,
    },
)];

/// GitHub Copilot:官方 Mono 变体。色值取原版 `MONO_BRAND_COLORS.copilot`。
const COPILOT: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x89, 0x57, 0xe5),
    Geom::Path {
        d: "M19.245 5.364c1.322 1.36 1.877 3.216 2.11 5.817.622 0 1.2.135 1.592.654l.73.964c.21.278.323.61.323.955v2.62c0 .339-.173.669-.453.868C20.239 19.602 16.157 21.5 12 21.5c-4.6 0-9.205-2.583-11.547-4.258-.28-.2-.452-.53-.453-.868v-2.62c0-.345.113-.679.321-.956l.73-.963c.392-.517.974-.654 1.593-.654l.029-.297c.25-2.446.81-4.213 2.082-5.52 2.461-2.54 5.71-2.851 7.146-2.864h.198c1.436.013 4.685.323 7.146 2.864zm-7.244 4.328c-.284 0-.613.016-.962.05-.123.447-.305.85-.57 1.108-1.05 1.023-2.316 1.18-2.994 1.18-.638 0-1.306-.13-1.851-.464-.516.165-1.012.403-1.044.996a65.882 65.882 0 00-.063 2.884l-.002.48c-.002.563-.005 1.126-.013 1.69.002.326.204.63.51.765 2.482 1.102 4.83 1.657 6.99 1.657 2.156 0 4.504-.555 6.985-1.657a.854.854 0 00.51-.766c.03-1.682.006-3.372-.076-5.053-.031-.596-.528-.83-1.046-.996-.546.333-1.212.464-1.85.464-.677 0-1.942-.157-2.993-1.18-.266-.258-.447-.661-.57-1.108-.32-.032-.64-.049-.96-.05zm-2.525 4.013c.539 0 .976.426.976.95v1.753c0 .525-.437.95-.976.95a.964.964 0 01-.976-.95v-1.752c0-.525.437-.951.976-.951zm5 0c.539 0 .976.426.976.95v1.753c0 .525-.437.95-.976.95a.964.964 0 01-.976-.95v-1.752c0-.525.437-.951.976-.951zM7.635 5.087c-1.05.102-1.935.438-2.385.906-.975 1.037-.765 3.668-.21 4.224.405.394 1.17.657 1.995.657h.09c.649-.013 1.785-.176 2.73-1.11.435-.41.705-1.433.675-2.47-.03-.834-.27-1.52-.63-1.813-.39-.336-1.275-.482-2.265-.394zm6.465.394c-.36.292-.6.98-.63 1.813-.03 1.037.24 2.06.675 2.47.968.957 2.136 1.104 2.776 1.11h.044c.825 0 1.59-.263 1.995-.657.555-.556.765-3.187-.21-4.224-.45-.468-1.335-.804-2.385-.906-.99-.088-1.875.058-2.265.394zM12 7.615c-.24 0-.525.015-.84.044.03.16.045.336.06.526l-.001.159a2.94 2.94 0 01-.014.25c.225-.022.425-.027.612-.028h.366c.187 0 .387.006.612.028-.015-.146-.015-.277-.015-.409.015-.19.03-.365.06-.526a9.29 9.29 0 00-.84-.044z",
        view: VB24,
        even_odd: true,
    },
)];

/// Ollama:官方 Mono 变体的羊驼。官方为纯黑,跟随主题色。
const OLLAMA: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Path {
        d: "M7.905 1.09c.216.085.411.225.588.41.295.306.544.744.734 1.263.191.522.315 1.1.362 1.68a5.054 5.054 0 012.049-.636l.051-.004c.87-.07 1.73.087 2.48.474.101.053.2.11.297.17.05-.569.172-1.134.36-1.644.19-.52.439-.957.733-1.264a1.67 1.67 0 01.589-.41c.257-.1.53-.118.796-.042.401.114.745.368 1.016.737.248.337.434.769.561 1.287.23.934.27 2.163.115 3.645l.053.04.026.019c.757.576 1.284 1.397 1.563 2.35.435 1.487.216 3.155-.534 4.088l-.018.021.002.003c.417.762.67 1.567.724 2.4l.002.03c.064 1.065-.2 2.137-.814 3.19l-.007.01.01.024c.472 1.157.62 2.322.438 3.486l-.006.039a.651.651 0 01-.747.536.648.648 0 01-.54-.742c.167-1.033.01-2.069-.48-3.123a.643.643 0 01.04-.617l.004-.006c.604-.924.854-1.83.8-2.72-.046-.779-.325-1.544-.8-2.273a.644.644 0 01.18-.886l.009-.006c.243-.159.467-.565.58-1.12a4.229 4.229 0 00-.095-1.974c-.205-.7-.58-1.284-1.105-1.683-.595-.454-1.383-.673-2.38-.61a.653.653 0 01-.632-.371c-.314-.665-.772-1.141-1.343-1.436a3.288 3.288 0 00-1.772-.332c-1.245.099-2.343.801-2.67 1.686a.652.652 0 01-.61.425c-1.067.002-1.893.252-2.497.703-.522.39-.878.935-1.066 1.588a4.07 4.07 0 00-.068 1.886c.112.558.331 1.02.582 1.269l.008.007c.212.207.257.53.109.785-.36.622-.629 1.549-.673 2.44-.05 1.018.186 1.902.719 2.536l.016.019a.643.643 0 01.095.69c-.576 1.236-.753 2.252-.562 3.052a.652.652 0 01-1.269.298c-.243-1.018-.078-2.184.473-3.498l.014-.035-.008-.012a4.339 4.339 0 01-.598-1.309l-.005-.019a5.764 5.764 0 01-.177-1.785c.044-.91.278-1.842.622-2.59l.012-.026-.002-.002c-.293-.418-.51-.953-.63-1.545l-.005-.024a5.352 5.352 0 01.093-2.49c.262-.915.777-1.701 1.536-2.269.06-.045.123-.09.186-.132-.159-1.493-.119-2.73.112-3.67.127-.518.314-.95.562-1.287.27-.368.614-.622 1.015-.737.266-.076.54-.059.797.042zm4.116 9.09c.936 0 1.8.313 2.446.855.63.527 1.005 1.235 1.005 1.94 0 .888-.406 1.58-1.133 2.022-.62.375-1.451.557-2.403.557-1.009 0-1.871-.259-2.493-.734-.617-.47-.963-1.13-.963-1.845 0-.707.398-1.417 1.056-1.946.668-.537 1.55-.849 2.485-.849zm0 .896a3.07 3.07 0 00-1.916.65c-.461.37-.722.835-.722 1.25 0 .428.21.829.61 1.134.455.347 1.124.548 1.943.548.799 0 1.473-.147 1.932-.426.463-.28.7-.686.7-1.257 0-.423-.246-.89-.683-1.256-.484-.405-1.14-.643-1.864-.643zm.662 1.21l.004.004c.12.151.095.37-.056.49l-.292.23v.446a.375.375 0 01-.376.373.375.375 0 01-.376-.373v-.46l-.271-.218a.347.347 0 01-.052-.49.353.353 0 01.494-.051l.215.172.22-.174a.353.353 0 01.49.051zm-5.04-1.919c.478 0 .867.39.867.871a.87.87 0 01-.868.871.87.87 0 01-.867-.87.87.87 0 01.867-.872zm8.706 0c.48 0 .868.39.868.871a.87.87 0 01-.868.871.87.87 0 01-.867-.87.87.87 0 01.867-.872zM7.44 2.3l-.003.002a.659.659 0 00-.285.238l-.005.006c-.138.189-.258.467-.348.832-.17.692-.216 1.631-.124 2.782.43-.128.899-.208 1.404-.237l.01-.001.019-.034c.046-.082.095-.161.148-.239.123-.771.022-1.692-.253-2.444-.134-.364-.297-.65-.453-.813a.628.628 0 00-.107-.09L7.44 2.3zm9.174.04l-.002.001a.628.628 0 00-.107.09c-.156.163-.32.45-.453.814-.29.794-.387 1.776-.23 2.572l.058.097.008.014h.03a5.184 5.184 0 011.466.212c.086-1.124.038-2.043-.128-2.722-.09-.365-.21-.643-.349-.832l-.004-.006a.659.659 0 00-.285-.239h-.004z",
        view: VB24,
        even_odd: true,
    },
)];

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
    AiVendor::Omp,
];

/// 厂商图标。`vendor` 为 `None` 时画通用机器人(与原版回退 lucide `Bot` 同)。
///
/// ```ignore
/// BrandIcon::new(AiVendor::from_session_type(&pane.agent)).size(px(13.0))
/// ```
///
/// 没有 `contrast()` —— 官方 path 全是单色填充,`Ink::Contrast`(实心底上的字形色)
/// 一处也用不上;那是自绘简化标记时代「品牌色片 + 内嵌字形」双色结构留下的,
/// 已随官方 path 一起去掉。[`StatusDot`](super::status::StatusDot) /
/// [`TechIcon`](super::tech::TechIcon) 仍有这档墨水,别照着它们抄。
#[derive(IntoElement)]
pub struct BrandIcon {
    vendor: Option<AiVendor>,
    size: Pixels,
    /// `Ink::Current` 的取色(Grok / OpenCode / Ollama / pi / 未知回退跟这个走)。
    color: Option<Hsla>,
}

impl BrandIcon {
    /// 默认 13px —— 与 `BrandIcon.tsx` 的 `size = 13` 一致。
    pub fn new(vendor: Option<AiVendor>) -> Self {
        Self {
            vendor,
            size: px(13.0),
            color: None,
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
}

impl RenderOnce for BrandIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let shapes = self.vendor.map(AiVendor::shapes).unwrap_or(UNKNOWN_BOT);
        let mut icon = VectorIcon::new(shapes, self.size);
        if let Some(c) = self.color {
            icon = icon.ink(c);
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
        // omp 没有会话记录,但 hook 会上报 agent 名 —— pane tab 靠这条认图标
        assert_eq!(AiVendor::for_session("omp", None), Some(AiVendor::Omp));
        // 没有会话记录的 agent 没有 CLI 兜底
        assert_eq!(AiVendor::for_session("opencode", None), None);
    }

    /// omp 的推断必须压过 pi:`oh-my-pi` 末尾的 `pi` 正落在词边界上。
    #[test]
    fn omp_的推断压过_pi() {
        assert_eq!(AiVendor::infer(Some("omp"), None), Some(AiVendor::Omp));
        assert_eq!(AiVendor::infer(None, Some("oh-my-pi")), Some(AiVendor::Omp));
        assert_eq!(
            AiVendor::infer(None, Some("omp --model claude-sonnet-5")),
            Some(AiVendor::Omp)
        );
        // 词边界:compose / ompx 都不是 omp
        assert_eq!(AiVendor::infer(None, Some("docker compose up")), None);
        assert_eq!(AiVendor::infer(None, Some("ompx")), None);
        // 单独的 pi 仍归 pi
        assert_eq!(AiVendor::infer(None, Some("pi")), Some(AiVendor::Pi));
    }

    /// omp 标记按基本形搬:六笔(横杠、两腿、插头、两插脚)全在单位方框内,
    /// 插头压在右腿下端上(原件的构图,插头「接」在短腿上)。
    #[test]
    fn omp_标记六笔都在方框内且插头接在右腿上() {
        assert_eq!(OMP.len(), 6);
        for shape in OMP {
            let (pts, closed) = shape.geom.points();
            assert!(closed, "圆角矩形应闭合");
            for (x, y) in pts {
                assert!(
                    (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                    "({x},{y}) 出框"
                );
            }
        }
        let (leg, plug) = (&OMP[2].geom, &OMP[3].geom);
        let (
            Geom::Rect {
                x: lx,
                y: ly,
                w: lw,
                h: lh,
                ..
            },
            Geom::Rect {
                x: px,
                y: py,
                w: pw,
                ..
            },
        ) = (*leg, *plug)
        else {
            panic!("右腿与插头都该是矩形");
        };
        assert!(px < lx && px + pw > lx + lw, "插头横向罩住右腿");
        assert!(py < ly + lh, "插头顶边压在右腿下端之上");
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
    fn pi_的官方_path_落在四分格上_且洞还是洞() {
        // 官方 path 在 viewBox 165.29..634.72 下全是轴对齐矩形,归一后每个
        // 坐标都该落在 1/4 格上 —— viewBox 传错(比如照抄了 0 0 24 24)会立刻炸
        let subs: Vec<_> = PI.iter().flat_map(|s| s.geom.subpaths().to_vec()).collect();
        assert_eq!(subs.len(), 3, "外框 + 洞 + 右竖");
        for (pts, _) in &subs {
            for (x, y) in pts {
                for v in [x, y] {
                    assert!((v * 4.0 - (v * 4.0).round()).abs() < 0.001, "{v} 不在 1/4 格上");
                }
            }
        }
        // 洞是第一条 Shape 的第二条子路径,evenodd 才挖得动
        assert!(PI[0].geom.even_odd(), "少了 evenodd 那个方洞会被填实");
        assert_eq!(subs[1].0.len(), 4, "洞是个正方形");
    }

    #[test]
    fn 官方_path_都解析得出形状() {
        // 解析失败会静默返回空/残缺子路径 —— 图标变成一片空白,肉眼在 13px 上
        // 未必看得出来,这里按「子路径数 + 总点数」钉住
        let want: &[(AiVendor, usize)] = &[
            (AiVendor::Claude, 1),
            // knot 的八瓣各是一条闭合子路径
            (AiVendor::OpenAi, 8),
            (AiVendor::Gemini, 1),
            // 外框 + 内框,靠 evenodd 挖空
            (AiVendor::OpenCode, 2),
            // 斜杠 + 挑锋两笔,都不闭合
            (AiVendor::Grok, 2),
            (AiVendor::Qwen, 3),
            (AiVendor::DeepSeek, 4),
            (AiVendor::Zhipu, 22),
            (AiVendor::Copilot, 7),
            (AiVendor::Ollama, 8),
        ];
        for (vendor, subpaths) in want {
            let got: Vec<_> = vendor
                .shapes()
                .iter()
                .flat_map(|s| s.geom.subpaths().to_vec())
                .collect();
            assert_eq!(got.len(), *subpaths, "{} 的子路径数不对", vendor.label());
            let points: usize = got.iter().map(|(p, _)| p.len()).sum();
            assert!(points > 8, "{} 只解析出 {points} 个点", vendor.label());
        }
    }

    #[test]
    fn 填充规则照抄原版() {
        // lobehub 的 Mono 变体在 <svg> 上挂 fillRule="evenodd",Color 变体多为
        // nonzero。填错的后果是「洞被填实」或「自交处被挖空」,两边都很难看
        for v in [AiVendor::OpenAi, AiVendor::OpenCode, AiVendor::Grok, AiVendor::Copilot, AiVendor::Ollama] {
            assert!(v.shapes().iter().all(|s| s.geom.even_odd()), "{} 该是 evenodd", v.label());
        }
        for v in [AiVendor::Claude, AiVendor::Gemini, AiVendor::Qwen, AiVendor::DeepSeek, AiVendor::Zhipu] {
            assert!(v.shapes().iter().all(|s| !s.geom.even_odd()), "{} 该是 nonzero", v.label());
        }
    }

    /// 一个点在不在某枚图标的填充区里 —— 按该 `Shape` 自己的填充规则,
    /// 把它的**全部**子路径一起算(洞就是这么挖出来的)。
    fn covered(shapes: &[Shape], x: f32, y: f32) -> bool {
        shapes.iter().any(|shape| {
            let even_odd = shape.geom.even_odd();
            let mut winding = 0i32;
            let mut crossings = 0u32;
            for (pts, _) in shape.geom.subpaths().iter() {
                for i in 0..pts.len() {
                    let (x0, y0) = pts[i];
                    let (x1, y1) = pts[(i + 1) % pts.len()];
                    if (y0 > y) == (y1 > y) {
                        continue;
                    }
                    let t = (y - y0) / (y1 - y0);
                    if x0 + t * (x1 - x0) <= x {
                        continue;
                    }
                    crossings += 1;
                    winding += if y1 > y0 { 1 } else { -1 };
                }
            }
            if even_odd {
                crossings % 2 == 1
            } else {
                winding != 0
            }
        })
    }

    #[test]
    fn evenodd_的洞真的是洞() {
        // OpenCode 官方 path `M16 6H8v12h8V6zm4 16H4V2h16v20z` 是外框套内框。
        // 少了 evenodd 会整块填实 —— 那是这枚图标唯一的辨识特征
        assert!(covered(OPENCODE, 0.5, 0.1), "外框上边");
        assert!(covered(OPENCODE, 0.2, 0.5), "外框左边");
        assert!(!covered(OPENCODE, 0.5, 0.5), "内框必须是空的");
        // Copilot 是线稿头:轮廓实、脸内空、两只眼睛是实心竖条
        assert!(covered(COPILOT, 0.05, 0.575), "头部轮廓左缘");
        assert!(covered(COPILOT, 0.375, 0.625), "左眼是实心的");
        assert!(covered(COPILOT, 0.625, 0.625), "右眼是实心的");
        assert!(!covered(COPILOT, 0.5, 0.625), "两眼之间的脸是空的");
        // pi 的方洞
        assert!(!covered(PI, 0.375, 0.375), "pi 的方洞");
    }

    /// 把一枚图标打成 ASCII —— 这个仓库没法在单测里截图,改形状表时用它肉眼过一遍。
    ///
    /// `cargo test -p mt-ui 品牌图标预览 -- --ignored --nocapture`
    ///
    /// 只画填充(`Pen::Fill`),描边形状在这里看不见 —— 品牌表现在全是填充。
    #[test]
    #[ignore = "调试用:打印 ASCII 预览,不做断言"]
    fn 品牌图标预览() {
        const W: usize = 40;
        for vendor in ALL_VENDORS {
            println!("\n── {} ──", vendor.label());
            for row in 0..W / 2 {
                let line: String = (0..W)
                    .map(|col| {
                        let x = (col as f32 + 0.5) / W as f32;
                        let y = (row as f32 + 0.5) / (W / 2) as f32;
                        if covered(vendor.shapes(), x, y) { '#' } else { '·' }
                    })
                    .collect();
                println!("{line}");
            }
        }
    }

    #[test]
    fn 每家都有形状_没有空表() {
        for v in ALL_VENDORS {
            assert!(!v.shapes().is_empty(), "{} 没有形状", v.label());
        }
        assert!(!UNKNOWN_BOT.is_empty());
    }
}
