//! 图标体系(改造清单 #9)。替掉 mt-app 现有的「CL/CX/GK 两字母文本」与
//! 「三形圆点」两处占位。
//!
//! ```text
//! icons
//! ├── vector    形状 DSL + 唯一的绘制 Element(自绘,不走 asset / 不走位图)
//! ├── svg_path  SVG `d` 的解析与离散化 —— 品牌 logo 原样搬运官方 path 靠它
//! ├── brand     AI 厂商图标 + 厂商推断(BrandIcon.tsx / inferVendor.ts)
//! ├── tech      技术栈徽标的 Element 与菜单分组(TechIcon.tsx / projectKind.ts)
//! ├── tech_art  ↑ 用的 51 种 ProjectKind 与官方 logo —— **生成物**,见 tools/gen_tech_icons.mjs
//! ├── file      文件树图标的查表规则与 Element(fileIcon.ts / FileTree.tsx)
//! ├── file_art  ↑ 用的 263 枚官方图形状表 —— **生成物**,见 tools/gen_file_icons.mjs
//! ├── status    四态状态灯 + spinner 旋转(StatusDot.tsx)
//! └── usage_glyphs  用量面板 KPI 六枚图标的形状表(KpiCards.tsx)
//! ```
//!
//! # 为什么全是自绘
//!
//! 三条现成的路都走不通,判据写在 [`vector`] 的模块注释里(一句话:`svg()` 要
//! 宿主注册 asset source 且只出单色掩膜;`img(ImageFormat::Svg)` 在 gpui 0.2.2
//! 上红蓝互换)。自绘的额外好处是**几何是纯数据**,映射表和形状都能单测。
//!
//! 「自绘」不等于「自己重新设计」:厂商 logo 那种几百段贝塞尔的自由曲线,
//! 走 [`svg_path`] 把原版那条 `d` 原样解析进来 —— 几何是官方的,渲染仍是自绘。
//!
//! # 宿主接线总表(mt-app 消费批照抄)
//!
//! | 位置 | 现状 | 换成 |
//! |---|---|---|
//! | `ui.rs::status_dot` | div 拼的三形圆点 | [`StatusDot`](status::StatusDot),见 [`status`] 模块注释的完整片段 |
//! | `session_panel.rs:492` | `"CX" / "GK" / "CL"` 文本 | [`BrandIcon`](brand::BrandIcon) + [`AiVendor::for_session`](brand::AiVendor::for_session) |
//! | tab 栏 / pane 标题 | 无图标 | [`BrandIcon`] + [`AiVendor::from_session_type`](brand::AiVendor::from_session_type) |
//! | 项目列表 / 文件树根 | 无图标 | [`TechIcon`](tech::TechIcon) + [`ProjectKind::from_str`](tech_art::ProjectKind::from_str) |
//! | 文件树每一行 | 无图标 | [`FileIcon`](file::FileIcon) |
//!
//! 三个尺寸口径与原版对齐:品牌 13px、技术栈 14px、文件 14px、状态灯 10px(sm)
//! / 13px(md)。全部可 `.size(px(..))` 覆盖。
//!
//! # 一个完整的演示用法
//!
//! ```ignore
//! use gpui::{div, px, ParentElement as _, Styled as _};
//! use mt_ui::icons::{AiVendor, BrandIcon, FileIcon, ProjectKind, StatusDot, StatusKind, TechIcon};
//!
//! div()
//!     .flex()
//!     .items_center()
//!     .gap(px(6.0))
//!     // 项目行:技术栈徽标 + 聚合状态灯
//!     .child(TechIcon::new(ProjectKind::Rust))
//!     .child(StatusDot::new(StatusKind::AiWorking))
//!     // 会话行:厂商图标
//!     .child(BrandIcon::new(AiVendor::for_session("claude", Some("glm-4.6"))))
//!     // 文件树行
//!     .child(FileIcon::new("Cargo.toml", false, false))
//! ```

pub mod brand;
pub mod file;
pub mod file_art;
pub mod status;
pub mod svg_path;
pub mod tech;
pub mod tech_art;
pub mod usage_glyphs;
pub mod vector;

pub use brand::{ALL_VENDORS, AiVendor, BrandIcon};
pub use file::{FileIcon, art_of as file_art_of};
pub use file_art::FileArt;
pub use status::{ALL_STATUS_KINDS, SPIN_PERIOD, StatusDot, StatusKind};
pub use tech::{ALL_TECH_CATEGORIES, TechCategory, TechIcon};
pub use tech_art::{ALL_PROJECT_KINDS, ProjectKind};
pub use usage_glyphs::ALL_USAGE_GLYPHS;
pub use vector::{Geom, Ink, Pen, Shape, VectorIcon};

/// 全部形状表。单测用它做「所有图标的点都在单位方框内」这类跨模块约束。
#[cfg(test)]
pub(crate) fn all_shape_tables() -> Vec<&'static [Shape]> {
    let mut out = brand::shape_tables();
    out.extend(tech::shape_tables());
    out.extend(file::shape_tables());
    out.extend(status::shape_tables());
    out.extend(usage_glyphs::shape_tables());
    out
}
