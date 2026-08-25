//! 技术栈徽标(对照 `src/components/TechIcon.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版用 devicon 的 `*-original.svg` 裸资产;GPUI 侧搬的是**同一批资产**——
//! 官方 logo 的那条 `d` 经 `tools/gen_tech_icons.mjs` 烘焙进 [`super::tech_art`],
//! 渲染仍是自绘(为什么不能让 gpui 去读 SVG,判据在 [`super::vector`] 的模块注释)。
//! 也就是说几何与配色都是官方的,不再是改造前那套「品牌色片 + 简化字形」的
//! 认色不认形的骨架。种类也从 12 扩到 51,覆盖主流语言 / 前端 / 后端 / 移动 / 基础设施。
//!
//! 本模块只剩 Element 与分组枚举;[`ProjectKind`] 连同它的字符串、展示名、形状表
//! 全在生成物里(见 [`super::tech_art`] 的模块注释)。
//!
//! # 已知偏差
//!
//! - **渐变**:`paint_path` 一次只吃一个纯色,渐变按 stop 的 offset 做梯形积分取均值。
//!   Angular 与 Kotlin 官方就是「红 → 紫」的整条渐变,均值必然落在洋红,与官方观感
//!   有出入(试过 devicon 的 plain 变体,那两枚要么没有配色、要么就是同一个洋红,
//!   没有更好的选择);Node.js 会从立体渐变变成扁平纯绿。形状一律不受影响;
//! - **深色底不可读的一批**:devicon 里 Rust / Apple / Express / Remix / Django 这些
//!   官方就是纯黑,画在深色面板上等于隐形。生成器按 WCAG 对比度检出后,单色的整枚改成
//!   跟随主题色(浅色主题下会自动变回深色)、多色的逐笔保色相提亮。
//!
//! 跑 `node tools/verify_icons.mjs tech` 可以逐枚比对官方原图与烘焙结果。
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
//! 「手动指定项目类型」的菜单按 [`TechCategory`] 分二级子菜单 ——
//! 五十多项平铺成一条长龙没法用。[`ALL_PROJECT_KINDS`](super::tech_art::ALL_PROJECT_KINDS)
//! 已按分组聚拢,直接顺序扫一遍即可分段。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::tech_art::ProjectKind;
use super::vector::VectorIcon;

/// 项目类型在「手动指定」菜单里的二级分组。
///
/// 分组本身要翻译,但**具体种类名不翻译**(Rust / React / Docker 都是专有名词)。
/// 文案 key 给宿主查 `mt-i18n`,mt-ui 这层不依赖 i18n。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TechCategory {
    Language,
    Frontend,
    Backend,
    Mobile,
    Infra,
}

/// 菜单里的分组顺序。
pub const ALL_TECH_CATEGORIES: &[TechCategory] = &[
    TechCategory::Language,
    TechCategory::Frontend,
    TechCategory::Backend,
    TechCategory::Mobile,
    TechCategory::Infra,
];

impl TechCategory {
    /// `projectList` 命名空间下的文案 key。
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Language => "menu.kindCategory.language",
            Self::Frontend => "menu.kindCategory.frontend",
            Self::Backend => "menu.kindCategory.backend",
            Self::Mobile => "menu.kindCategory.mobile",
            Self::Infra => "menu.kindCategory.infra",
        }
    }
}

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [super::vector::Shape]> {
    super::tech_art::ALL_PROJECT_KINDS
        .iter()
        .map(|k| k.shapes())
        .collect()
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
    color: Option<Hsla>,
}

impl TechIcon {
    /// 默认 14px —— 与 `TechIcon.tsx` 的 `size = 14` 一致。
    pub fn new(kind: ProjectKind) -> Self {
        Self {
            kind,
            size: px(14.0),
            color: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 把整枚徽标压成一个颜色,盖掉官方配色(置灰、禁用态)。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    pub fn kind(&self) -> ProjectKind {
        self.kind
    }
}

impl RenderOnce for TechIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let icon = VectorIcon::new(self.kind.shapes(), self.size);
        match self.color {
            Some(color) => icon.force_ink(color),
            None => icon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tech_art::ALL_PROJECT_KINDS;
    use super::*;

    #[test]
    fn 存量配置里的十二个种类一个都不能少() {
        // 这些字符串**落在用户配置**里(项目的 kindOverride)。改一个字、少一项,
        // 存量配置就读不回来 —— 徽标会从用户设定的那个变回自动探测的结果
        for legacy in [
            "java", "rust", "go", "python", "nodejs", "react", "vuejs", "nextjs", "svelte",
            "vite", "flutter", "php",
        ] {
            assert!(
                ProjectKind::from_str(legacy).is_some(),
                "{legacy} 从表里消失了,存量配置会读不回来"
            );
        }
    }

    #[test]
    fn 字符串双向可逆且无重复() {
        let mut seen: Vec<&str> = Vec::new();
        for k in ALL_PROJECT_KINDS {
            let s = k.as_str();
            assert_eq!(ProjectKind::from_str(s), Some(*k), "{s} 转不回来");
            assert!(!seen.contains(&s), "{s} 重复了");
            seen.push(s);
        }
        assert_eq!(ProjectKind::from_str("cobol"), None);
    }

    #[test]
    fn 每种都有图有名有分组() {
        for k in ALL_PROJECT_KINDS {
            assert!(!k.shapes().is_empty(), "{:?} 没有形状", k);
            assert!(!k.label().is_empty(), "{:?} 没有展示名", k);
            assert!(
                ALL_TECH_CATEGORIES.contains(&k.category()),
                "{:?} 的分组不在菜单顺序表里",
                k
            );
        }
    }

    #[test]
    fn 菜单顺序已按分组聚拢() {
        // 菜单是「一个分组一个子菜单」,顺序表要是交错的,分段就得先排序 ——
        // 生成器已经拢好了,这里钉住它
        let mut order: Vec<TechCategory> = Vec::new();
        for k in ALL_PROJECT_KINDS {
            let c = k.category();
            if order.last() != Some(&c) {
                assert!(!order.contains(&c), "{c:?} 分组被拆成了不连续的两段");
                order.push(c);
            }
        }
        assert_eq!(order, ALL_TECH_CATEGORIES, "分组顺序与菜单顺序表不一致");
    }

    #[test]
    fn 覆盖面够广() {
        // 这次改造的目的就是「支持市面上流行的开发语言」,种类数掉下来说明清单被误删
        assert!(
            ALL_PROJECT_KINDS.len() >= 50,
            "只剩 {} 种",
            ALL_PROJECT_KINDS.len()
        );
        for must in ["rust", "kotlin", "swift", "ruby", "csharp", "cpp", "django", "docker"] {
            assert!(ProjectKind::from_str(must).is_some(), "缺了 {must}");
        }
    }
}
