//! 快速打开文件(Ctrl+Shift+O)的后端:项目文件索引 + 模糊打分 + `文件:行:列` 解析。
//!
//! 与全局搜索([`crate::search`])的「文件名」模式是两条独立的路:那边是子串匹配、按 Enter
//! 才遍历一次磁盘、结果按路径排序;这边是一次建好的内存索引,每次按键在内存里重新打分、
//! 按命中质量排序。遍历与忽略规则两边共用同一个 walker(见 [`index`] 模块注释)。
//!
//! - [`fuzzy`] —— 子序列匹配与打分,返回分数与命中字符下标(界面高亮用);
//! - [`index`] —— [`FileIndex`]:建索引(有文件数与耗时上限)、按查询取前 N 条、最近打开的
//!   同分加权;
//! - [`goto`] —— 拆查询末尾的 `:行[:列]`(认得 Windows 盘符的冒号),以及把粘进来的绝对路径
//!   换成相对项目根的写法。
//!
//! 打分规则、文件表结构与 `:行:列` 的拆法移植自 Winc(Apache-2.0),出处逐个写在各模块注释里。
//!
//! **不依赖 GPUI**,全是同步函数:[`FileIndex::build`] 要走一遍目录树、大索引上的
//! [`FileIndex::search`] 要扫几十万条路径,线程调度由调用方决定。

pub mod fuzzy;
pub mod goto;
pub mod index;

pub use fuzzy::{MAX_QUERY_CHARS, PathRef, normalize_query};
pub use goto::{Goto, relative_query, split_goto};
pub use index::{
    BUILD_TIMEOUT, BuildLimits, FileIndex, Incomplete, MAX_FILES, SearchOutcome, path_order,
};
