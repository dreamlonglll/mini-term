//! 技术栈徽标(对照 `src/components/TechIcon.tsx`)。
//!
//! 原版是 devicon 的 `*-original.svg` 裸资产,仓库里同样没有源文件;GPUI 侧
//! 自绘几何骨架 + **官方品牌色**。种类与 `src/utils/projectKind.ts` 的
//! `ProjectKind` 一一对应,展示名照抄 `PROJECT_KIND_LABELS`(专有名词,不进 i18n)。
//!
//! # 已知偏差
//!
//! - React / Vue / Node / Vite / Flutter 的骨架与官方 logo 同构(同心椭圆、双 V、
//!   六边形、三角+闪电、双叶片),辨识度接近;
//! - Java / Python / Go / PHP / Svelte / Next.js 的官方 logo 是字标或复杂插画,
//!   这里退成「品牌色片 + 简化字形」,认色不认形;
//! - devicon 的 `original` 变体在浅色主题下部分徽标观感差(原版注释里也点了这条),
//!   本实现同样不跟随主题 —— 品牌色改了就不是那个牌子。
//!
//! # 宿主接线(mt-app)
//!
//! 项目列表 / 文件树根节点:
//!
//! ```ignore
//! use mt_ui::icons::{ProjectKind, TechIcon};
//! if let Some(kind) = ProjectKind::from_str(&project.kind) {
//!     row = row.child(TechIcon::new(kind).size(px(14.0)));
//! }
//! ```
//!
//! `project.kind` 的字符串取值与前端 `ProjectKind` 相同(`classifyProject` 的产物),
//! 所以两版可以直接互读同一份 config.json。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::vector::{Geom, Ink, Shape, VectorIcon};

/// 项目技术栈。取值与 `src/types.ts` 的 `ProjectKind` 一字不差。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProjectKind {
    Java,
    Rust,
    Go,
    Python,
    Flutter,
    Php,
    Vue,
    Next,
    React,
    Svelte,
    Vite,
    Node,
}

/// 手动指定菜单的可选项顺序(常用在前)—— 照抄前端 `PROJECT_KINDS`。
pub const ALL_PROJECT_KINDS: &[ProjectKind] = &[
    ProjectKind::Java,
    ProjectKind::Rust,
    ProjectKind::Go,
    ProjectKind::Python,
    ProjectKind::Node,
    ProjectKind::React,
    ProjectKind::Vue,
    ProjectKind::Next,
    ProjectKind::Svelte,
    ProjectKind::Vite,
    ProjectKind::Flutter,
    ProjectKind::Php,
];

impl ProjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Python => "python",
            Self::Flutter => "flutter",
            Self::Php => "php",
            Self::Vue => "vuejs",
            Self::Next => "nextjs",
            Self::React => "react",
            Self::Svelte => "svelte",
            Self::Vite => "vite",
            Self::Node => "nodejs",
        }
    }

    // 与 `mt_app::tree::PaneStatus::from_str` 取同一个命名,不实现 `FromStr` trait
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "java" => Self::Java,
            "rust" => Self::Rust,
            "go" => Self::Go,
            "python" => Self::Python,
            "flutter" => Self::Flutter,
            "php" => Self::Php,
            "vuejs" => Self::Vue,
            "nextjs" => Self::Next,
            "react" => Self::React,
            "svelte" => Self::Svelte,
            "vite" => Self::Vite,
            "nodejs" => Self::Node,
            _ => return None,
        })
    }

    /// 展示名,照抄前端 `PROJECT_KIND_LABELS`。
    pub fn label(self) -> &'static str {
        match self {
            Self::Java => "Java",
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::Python => "Python",
            Self::Flutter => "Flutter",
            Self::Php => "PHP",
            Self::Vue => "Vue",
            Self::Next => "Next.js",
            Self::React => "React",
            Self::Svelte => "Svelte",
            Self::Vite => "Vite",
            Self::Node => "Node.js",
        }
    }

    fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Java => JAVA,
            Self::Rust => RUST,
            Self::Go => GO,
            Self::Python => PYTHON,
            Self::Flutter => FLUTTER,
            Self::Php => PHP,
            Self::Vue => VUE,
            Self::Next => NEXT,
            Self::React => REACT,
            Self::Svelte => SVELTE,
            Self::Vite => VITE,
            Self::Node => NODE,
        }
    }
}

// ───────────────────────── 形状表 ─────────────────────────

/// React:三个 60° 相错的椭圆轨道 + 核。与官方 logo 同构。
const REACT: &[Shape] = &[
    Shape::fill(REACT_CYAN, Geom::Circle { c: (0.5, 0.5), r: 0.115 }),
    Shape::line(
        REACT_CYAN,
        0.055,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.47, 0.18),
            tilt: 0.0,
        },
    ),
    Shape::line(
        REACT_CYAN,
        0.055,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.47, 0.18),
            tilt: 60.0,
        },
    ),
    Shape::line(
        REACT_CYAN,
        0.055,
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.47, 0.18),
            tilt: 120.0,
        },
    ),
];
const REACT_CYAN: Ink = Ink::Rgb(0x61, 0xda, 0xfb);

/// Vue:内外双 V(外深绿、内墨蓝),与官方 logo 同构。
const VUE: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x41, 0xb8, 0x83),
        Geom::Polygon(&[
            (0.02, 0.16),
            (0.22, 0.16),
            (0.50, 0.64),
            (0.78, 0.16),
            (0.98, 0.16),
            (0.50, 0.94),
        ]),
    ),
    Shape::fill(
        Ink::Rgb(0x35, 0x49, 0x5e),
        Geom::Polygon(&[
            (0.22, 0.16),
            (0.38, 0.16),
            (0.50, 0.38),
            (0.62, 0.16),
            (0.78, 0.16),
            (0.50, 0.64),
        ]),
    ),
];

/// Node.js:六边形。与官方 logo 同构。
const NODE: &[Shape] = &[Shape::fill(
    Ink::Rgb(0x53, 0x9e, 0x43),
    Geom::Polygon(&[
        (0.50, 0.03),
        (0.92, 0.27),
        (0.92, 0.73),
        (0.50, 0.97),
        (0.08, 0.73),
        (0.08, 0.27),
    ]),
)];

/// Vite:三角 + 闪电。与官方 logo 同构(渐变退成单色)。
const VITE: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x64, 0x6c, 0xff),
        Geom::Polygon(&[(0.04, 0.16), (0.96, 0.16), (0.50, 0.97)]),
    ),
    Shape::fill(
        Ink::Rgb(0xff, 0xd6, 0x2e),
        Geom::Polygon(&[
            (0.58, 0.10),
            (0.35, 0.50),
            (0.49, 0.50),
            (0.42, 0.86),
            (0.66, 0.44),
            (0.51, 0.44),
        ]),
    ),
];

/// Flutter:两片错位的叶片。与官方 logo 同构(深浅两蓝)。
const FLUTTER: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x47, 0xc5, 0xfb),
        Geom::Polygon(&[(0.92, 0.06), (0.44, 0.54), (0.20, 0.54), (0.68, 0.06)]),
    ),
    Shape::fill(
        Ink::Rgb(0x42, 0xa5, 0xf5),
        Geom::Polygon(&[(0.92, 0.52), (0.66, 0.52), (0.44, 0.74), (0.57, 0.87)]),
    ),
    Shape::fill(
        Ink::Rgb(0x0d, 0x61, 0xa9),
        Geom::Polygon(&[(0.57, 0.87), (0.92, 0.94), (0.68, 0.94), (0.44, 0.74)]),
    ),
];

/// Python:两条互扣的蛇身(蓝/黄)。
const PYTHON: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x37, 0x76, 0xab),
        Geom::Polygon(&[
            (0.30, 0.04),
            (0.70, 0.04),
            (0.70, 0.34),
            (0.46, 0.34),
            (0.46, 0.44),
            (0.94, 0.44),
            (0.94, 0.52),
            (0.50, 0.52),
            (0.50, 0.70),
            (0.06, 0.70),
            (0.06, 0.34),
            (0.30, 0.34),
        ]),
    ),
    Shape::fill(
        Ink::Rgb(0xff, 0xd4, 0x3b),
        Geom::Polygon(&[
            (0.70, 0.96),
            (0.30, 0.96),
            (0.30, 0.66),
            (0.54, 0.66),
            (0.54, 0.56),
            (0.06, 0.56),
            (0.06, 0.48),
            (0.50, 0.48),
            (0.50, 0.30),
            (0.94, 0.30),
            (0.94, 0.66),
            (0.70, 0.66),
        ]),
    ),
    // 两只眼睛:小尺寸下让互扣关系看得出来
    Shape::fill(Ink::Contrast, Geom::Circle { c: (0.38, 0.15), r: 0.045 }),
    Shape::fill(Ink::Contrast, Geom::Circle { c: (0.62, 0.85), r: 0.045 }),
];

/// Rust:齿轮环 + 中心孔。
const RUST: &[Shape] = &[
    Shape::line(
        RUST_TAN,
        0.10,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 0.36,
        },
    ),
    Shape::line(RUST_TAN, 0.09, Geom::Polyline(&[(0.50, 0.04), (0.50, 0.16)])),
    Shape::line(RUST_TAN, 0.09, Geom::Polyline(&[(0.50, 0.84), (0.50, 0.96)])),
    Shape::line(RUST_TAN, 0.09, Geom::Polyline(&[(0.04, 0.50), (0.16, 0.50)])),
    Shape::line(RUST_TAN, 0.09, Geom::Polyline(&[(0.84, 0.50), (0.96, 0.50)])),
    Shape::line(RUST_TAN, 0.08, Geom::Polyline(&[(0.17, 0.17), (0.26, 0.26)])),
    Shape::line(RUST_TAN, 0.08, Geom::Polyline(&[(0.83, 0.83), (0.74, 0.74)])),
    Shape::line(RUST_TAN, 0.08, Geom::Polyline(&[(0.83, 0.17), (0.74, 0.26)])),
    Shape::line(RUST_TAN, 0.08, Geom::Polyline(&[(0.17, 0.83), (0.26, 0.74)])),
];
const RUST_TAN: Ink = Ink::Rgb(0xde, 0xa5, 0x84);

/// Go:品牌青色片 + 速度线(gopher 画不出来,取其 wordmark 的青)。
const GO: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x00, 0xad, 0xd8),
        Geom::Rect {
            x: 0.04,
            y: 0.18,
            w: 0.92,
            h: 0.64,
            round: 0.20,
        },
    ),
    Shape::line(Ink::Contrast, 0.09, Geom::Polyline(&[(0.16, 0.38), (0.42, 0.38)])),
    Shape::line(Ink::Contrast, 0.09, Geom::Polyline(&[(0.16, 0.62), (0.42, 0.62)])),
    Shape::line(
        Ink::Contrast,
        0.09,
        Geom::Arc {
            c: (0.66, 0.50),
            r: 0.18,
            from: -30.0,
            sweep: 300.0,
        },
    ),
];

/// Java:咖啡杯 + 两缕蒸汽。
const JAVA: &[Shape] = &[
    Shape::line(
        Ink::Rgb(0xea, 0x2d, 0x2e),
        0.08,
        Geom::Polyline(&[(0.40, 0.30), (0.48, 0.18), (0.40, 0.06)]),
    ),
    Shape::line(
        Ink::Rgb(0xea, 0x2d, 0x2e),
        0.08,
        Geom::Polyline(&[(0.60, 0.30), (0.68, 0.18), (0.60, 0.06)]),
    ),
    Shape::fill(
        JAVA_BLUE,
        Geom::Polygon(&[(0.16, 0.42), (0.76, 0.42), (0.68, 0.90), (0.24, 0.90)]),
    ),
    Shape::line(
        JAVA_BLUE,
        0.07,
        Geom::Arc {
            c: (0.78, 0.56),
            r: 0.14,
            from: -80.0,
            sweep: 170.0,
        },
    ),
];
const JAVA_BLUE: Ink = Ink::Rgb(0x00, 0x74, 0xbd);

/// PHP:品牌紫的椭圆片(官方 elephant/wordmark 的辨识锚点就是这枚紫椭圆)。
const PHP: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0x77, 0x7b, 0xb4),
        Geom::Ellipse {
            c: (0.5, 0.5),
            r: (0.48, 0.30),
            tilt: 0.0,
        },
    ),
    Shape::line(Ink::Contrast, 0.075, Geom::Polyline(&[(0.26, 0.62), (0.34, 0.38)])),
    Shape::line(Ink::Contrast, 0.075, Geom::Polyline(&[(0.46, 0.62), (0.54, 0.38)])),
    Shape::line(Ink::Contrast, 0.075, Geom::Polyline(&[(0.66, 0.62), (0.74, 0.38)])),
];

/// Next.js:黑底白圈 + 一道斜线(官方是圆里一个 N)。
const NEXT: &[Shape] = &[
    Shape::fill(Ink::Rgb(0x0a, 0x0a, 0x0a), Geom::Circle { c: (0.5, 0.5), r: 0.48 }),
    Shape::line(
        Ink::Rgb(0xff, 0xff, 0xff),
        0.055,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 0.45,
        },
    ),
    Shape::line(
        Ink::Rgb(0xff, 0xff, 0xff),
        0.085,
        Geom::Polyline(&[(0.33, 0.72), (0.33, 0.28), (0.70, 0.78)]),
    ),
    Shape::line(
        Ink::Rgb(0xff, 0xff, 0xff),
        0.085,
        Geom::Polyline(&[(0.67, 0.28), (0.67, 0.56)]),
    ),
];

/// Svelte:品牌橙片 + S 形折线。
const SVELTE: &[Shape] = &[
    Shape::fill(
        Ink::Rgb(0xff, 0x3e, 0x00),
        Geom::Rect {
            x: 0.06,
            y: 0.06,
            w: 0.88,
            h: 0.88,
            round: 0.26,
        },
    ),
    Shape::line(
        Ink::Contrast,
        0.10,
        Geom::Polyline(&[(0.70, 0.28), (0.34, 0.28), (0.34, 0.48), (0.66, 0.52), (0.66, 0.72), (0.30, 0.72)]),
    ),
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    ALL_PROJECT_KINDS.iter().map(|k| k.shapes()).collect()
}

/// 技术栈徽标。
///
/// ```ignore
/// TechIcon::new(ProjectKind::Rust).size(px(14.0))
/// ```
#[derive(IntoElement)]
pub struct TechIcon {
    kind: ProjectKind,
    size: Pixels,
    contrast: Option<Hsla>,
}

impl TechIcon {
    /// 默认 14px —— 与 `TechIcon.tsx` 的 `size = 14` 一致。
    pub fn new(kind: ProjectKind) -> Self {
        Self {
            kind,
            size: px(14.0),
            contrast: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 品牌色片上「挖空」那部分的取色,默认面板底色。
    pub fn contrast(mut self, color: Hsla) -> Self {
        self.contrast = Some(color);
        self
    }
}

impl RenderOnce for TechIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut icon = VectorIcon::new(self.kind.shapes(), self.size);
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
    fn 种类字符串与前端一字不差() {
        // 与 projectKind.ts 的 ProjectKind 联合类型对齐;devicon 的目录名
        // (nodejs / vuejs / vitejs)与这里的 key 不是一回事,别混
        let expect = [
            "java", "rust", "go", "python", "nodejs", "react", "vuejs", "nextjs", "svelte",
            "vite", "flutter", "php",
        ];
        let actual: Vec<&str> = ALL_PROJECT_KINDS.iter().map(|k| k.as_str()).collect();
        assert_eq!(actual, expect, "顺序也要与前端 PROJECT_KINDS 一致");
        for k in ALL_PROJECT_KINDS {
            assert_eq!(ProjectKind::from_str(k.as_str()), Some(*k));
        }
        assert_eq!(ProjectKind::from_str("cobol"), None);
    }

    #[test]
    fn 展示名照抄前端标签表() {
        assert_eq!(ProjectKind::Node.label(), "Node.js");
        assert_eq!(ProjectKind::Next.label(), "Next.js");
        assert_eq!(ProjectKind::Vue.label(), "Vue");
        assert_eq!(ProjectKind::Php.label(), "PHP");
    }

    #[test]
    fn 每种都有形状_没有空表() {
        for k in ALL_PROJECT_KINDS {
            assert!(!k.shapes().is_empty(), "{} 没有形状", k.label());
        }
    }
}
