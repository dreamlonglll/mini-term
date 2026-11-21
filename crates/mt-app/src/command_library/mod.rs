//! 命令库(issue #81):把常用命令存下来,点一下写进当前终端并回车。
//!
//! # 形态
//!
//! 终端控制条多一颗「命令」钮(与查找 / 分屏同排),点开一个带搜索的浮层
//! ([`popover::CommandPopover`]):按分组列出命令,行上 ▷ 运行、悬停出
//! 复制 / 编辑 / 删除,底部「新增命令」「新建分组」。快捷键 Ctrl+Shift+K 同效。
//! 浮层的宿主是 `terminal_area`(它手里有 pane 矩形与焦点还原那套),本模块
//! 只出面板本体、编辑弹窗与纯逻辑。
//!
//! # 数据
//!
//! **全局一份**(`config.commandLibrary`),不按项目 / SSH 连接绑定 —— 一条部署
//! 命令在哪台机器上都是同一句;要按机器区分的话用分组。分组沿用 SSH 连接那套
//! 「组名即键」的口径(见 `store/commands.rs`)。
//!
//! # 运行语义
//!
//! 走 `AppStore::write_to_pane`,与用户自己敲这条命令**同一条链路**:AI 输入检测
//! 看得见它,pane 因此能正常进入 AI 会话状态(ADR 0002 的纪律)。`Ctrl+↵` /
//! `Ctrl+点击` 只敲命令不回车 —— 给「先改个参数再跑」留口子。

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Entity, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputState};
use mt_config::{CommandLibrary, SavedCommand};
use mt_ui::icons::{Geom, Ink, Shape};

use crate::i18n::t;
use crate::menu;
use crate::prompt::{autofocus, confirm_footer, is_open, kind, open_guarded, show_prompt};
use crate::store::AppStore;
use crate::ui;

pub mod popover;

pub use popover::{CommandPopover, PopoverEvent};

// ─── 纯逻辑 ───────────────────────────────────────────────────

/// 归一化分组名:trim 后空串视为未分组(`None`)。
pub fn normalize_group(group: Option<&str>) -> Option<&str> {
    group.map(str::trim).filter(|g| !g.is_empty())
}

/// 折叠集合里「未分组」桶的键 —— 组名不可能是空串,拿它当键不撞。
pub const UNGROUPED_KEY: &str = "";

/// 一个分组桶。`group = None` = 未分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBucket {
    pub group: Option<String>,
    pub items: Vec<SavedCommand>,
}

impl CommandBucket {
    /// 折叠集合用的键。
    pub fn key(&self) -> &str {
        self.group.as_deref().unwrap_or(UNGROUPED_KEY)
    }
}

/// 按分组归桶。顺序:**未分组在前**(没归组的命令就是「顶层」,与 issue 截图
/// 一致,也与 SSH 面板「未分组垫底」有意不同 —— 那边未分组是兜底,这边是常态),
/// 然后具名分组按「命令里首次出现的顺序」,再接显式创建的空分组。
///
/// 未分组桶只在**有命令**时出现(空的未分组没有意义);具名空组照样出桶 ——
/// 用户刚建的组得看得见,才能往里加东西。
pub fn build_buckets(lib: &CommandLibrary) -> Vec<CommandBucket> {
    let mut out: Vec<CommandBucket> = Vec::new();
    let ungrouped: Vec<SavedCommand> = lib
        .commands
        .iter()
        .filter(|c| normalize_group(c.group.as_deref()).is_none())
        .cloned()
        .collect();
    if !ungrouped.is_empty() {
        out.push(CommandBucket {
            group: None,
            items: ungrouped,
        });
    }
    let ensure = |out: &mut Vec<CommandBucket>, name: &str| -> usize {
        if let Some(i) = out.iter().position(|b| b.group.as_deref() == Some(name)) {
            i
        } else {
            out.push(CommandBucket {
                group: Some(name.to_string()),
                items: Vec::new(),
            });
            out.len() - 1
        }
    };
    for cmd in &lib.commands {
        if let Some(g) = normalize_group(cmd.group.as_deref()) {
            let g = g.to_string();
            let idx = ensure(&mut out, &g);
            out[idx].items.push(cmd.clone());
        }
    }
    for raw in &lib.groups {
        if let Some(g) = normalize_group(Some(raw)) {
            ensure(&mut out, g);
        }
    }
    out
}

/// 具名分组名(编辑弹窗的分组下拉候选)。
pub fn group_names(lib: &CommandLibrary) -> Vec<String> {
    build_buckets(lib)
        .into_iter()
        .filter_map(|b| b.group)
        .collect()
}

/// 一条命令是否命中查询:名称或命令文本**包含**查询串(大小写不敏感)。
///
/// 用子串而不是项目切换器那种子序列模糊匹配:命令文本是字面量,`dk` 模糊命中
/// `docker` 反而让人摸不着头脑;子串匹配的结果一眼能解释。
pub fn matches_query(cmd: &SavedCommand, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    cmd.name.to_lowercase().contains(&q) || cmd.command.to_lowercase().contains(&q)
}

/// 浮层里实际画出来的桶:查询非空时只留命中的命令、丢掉空桶、**忽略折叠**
/// (搜出来的东西藏在折叠组里等于没搜到);查询为空时原样出桶,折叠只影响
/// 桶内条目是否展开,由 [`VisibleBucket::collapsed`] 告诉渲染层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleBucket {
    pub group: Option<String>,
    /// 桶里的**全部**条数(折叠时标题行右侧照样显示总数)。
    pub total: usize,
    pub collapsed: bool,
    /// 展开时要画的条目;折叠时为空。
    pub items: Vec<SavedCommand>,
}

impl VisibleBucket {
    pub fn key(&self) -> &str {
        self.group.as_deref().unwrap_or(UNGROUPED_KEY)
    }
}

pub fn visible_buckets(
    lib: &CommandLibrary,
    query: &str,
    collapsed: &HashSet<String>,
) -> Vec<VisibleBucket> {
    let searching = !query.trim().is_empty();
    build_buckets(lib)
        .into_iter()
        .filter_map(|bucket| {
            let key = bucket.key().to_string();
            let items: Vec<SavedCommand> = if searching {
                bucket
                    .items
                    .into_iter()
                    .filter(|c| matches_query(c, query))
                    .collect()
            } else {
                bucket.items
            };
            if searching && items.is_empty() {
                return None;
            }
            let is_collapsed = !searching && collapsed.contains(&key);
            Some(VisibleBucket {
                group: bucket.group,
                total: items.len(),
                collapsed: is_collapsed,
                items: if is_collapsed { Vec::new() } else { items },
            })
        })
        .collect()
}

/// 把可见桶拍平成「键盘游标能走到的行」—— 折叠桶里的条目不在其中。
pub fn flatten_rows(buckets: &[VisibleBucket]) -> Vec<SavedCommand> {
    buckets
        .iter()
        .flat_map(|b| b.items.iter().cloned())
        .collect()
}

/// 分组改名后的显式分组列表(逐条 trim、丢空名、按首次出现去重 ——
/// 重命名成已有组名时两个桶自然合并)。与 `ssh_conn::merge_ssh_groups_on_rename`
/// 同一算法,不共用是因为那边的注释与测试都钉在 SSH 语义上。
pub fn merge_groups_on_rename(groups: &[String], old_name: &str, new_name: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for raw in groups {
        let n = if raw.trim() == old_name {
            new_name.to_string()
        } else {
            raw.trim().to_string()
        };
        if !n.is_empty() && seen.insert(n.clone()) {
            out.push(n);
        }
    }
    out
}

/// 真正写进 PTY 的字节串。命令两头的空白去掉(尾部换行尤其要去:用户从别处
/// 粘进来的命令常带一个 `\n`,留着就是「回车两次」);`newline` 时补一个 `\r`
/// —— 终端里的回车是 CR,与 `store::write_launcher_command` 同款。空命令返回空串。
pub fn run_payload(command: &str, newline: bool) -> String {
    let body = command.trim();
    if body.is_empty() {
        return String::new();
    }
    if newline {
        format!("{body}\r")
    } else {
        body.to_string()
    }
}

// ─── 图标(viewBox 16 归一化,描边口径与 terminal_area 的控件簇一致)───

const fn cu(v: f32) -> f32 {
    v / 16.0
}
const STROKE: f32 = 1.3 / 16.0;

/// 控制条上的「命令」钮:「播放列表」——三条横线 + 右下角一枚实心 ▶。
///
/// 第一版画的是圆角框里的 `>_`,与右缘「终端面板」那颗几乎同形,用户点名换掉;
/// 「一列可以跑的东西」正是这个功能本身,而且簇里其余图标都是线框,实心三角
/// 一眼能分出来。
pub const ICON_COMMANDS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(2.0), cu(4.0)), (cu(14.0), cu(4.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(2.0), cu(8.0)), (cu(14.0), cu(8.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(2.0), cu(12.0)), (cu(8.0), cu(12.0))]),
    ),
    Shape::fill(
        Ink::Current,
        Geom::Polygon(&[
            (cu(10.5), cu(9.5)),
            (cu(14.5), cu(12.0)),
            (cu(10.5), cu(14.5)),
        ]),
    ),
];

/// ▷ 运行(实心三角)。
pub const ICON_PLAY: &[Shape] = &[Shape::fill(
    Ink::Current,
    Geom::Polygon(&[(cu(5.0), cu(3.5)), (cu(12.5), cu(8.0)), (cu(5.0), cu(12.5))]),
)];

/// 复制(两只叠着的框)。
pub const ICON_COPY: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: cu(5.5),
            y: cu(5.5),
            w: cu(8.0),
            h: cu(8.0),
            round: cu(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(3.0), cu(10.5)), (cu(3.0), cu(3.0)), (cu(10.5), cu(3.0))]),
    ),
];

/// 编辑(铅笔)。
pub const ICON_EDIT: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polygon(&[
        (cu(2.5), cu(13.5)),
        (cu(5.5), cu(12.9)),
        (cu(13.0), cu(5.4)),
        (cu(10.6), cu(3.0)),
        (cu(3.1), cu(10.5)),
    ]),
)];

/// 删除(垃圾桶)。
pub const ICON_TRASH: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(3.0), cu(4.5)), (cu(13.0), cu(4.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (cu(6.5), cu(4.5)),
            (cu(6.5), cu(3.0)),
            (cu(9.5), cu(3.0)),
            (cu(9.5), cu(4.5)),
        ]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (cu(4.5), cu(4.5)),
            (cu(5.3), cu(13.5)),
            (cu(10.7), cu(13.5)),
            (cu(11.5), cu(4.5)),
        ]),
    ),
];

/// ＋ 新增。
pub const ICON_PLUS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(8.0), cu(3.0)), (cu(8.0), cu(13.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(3.0), cu(8.0)), (cu(13.0), cu(8.0))]),
    ),
];

/// 带 ＋ 的文件夹(新建分组)。
pub const ICON_FOLDER_PLUS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polygon(&[
            (cu(1.5), cu(4.5)),
            (cu(6.0), cu(4.5)),
            (cu(7.5), cu(6.0)),
            (cu(14.5), cu(6.0)),
            (cu(14.5), cu(13.5)),
            (cu(1.5), cu(13.5)),
        ]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(8.0), cu(8.5)), (cu(8.0), cu(11.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(6.5), cu(10.0)), (cu(9.5), cu(10.0))]),
    ),
];

/// 分组标题前的折叠指示:›。只画这一枚,展开时绕中心顺时针转 90° 成 ˅
/// (见 `fold_motion`;原先另有一枚 ˅ 两态硬换)。
pub const ICON_CHEVRON_RIGHT: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polyline(&[(cu(6.0), cu(4.0)), (cu(10.0), cu(8.0)), (cu(6.0), cu(12.0))]),
)];

/// ✕(解散分组)。
pub const ICON_CLOSE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(4.0), cu(4.0)), (cu(12.0), cu(12.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(cu(12.0), cu(4.0)), (cu(4.0), cu(12.0))]),
    ),
];

// ─── 新增 / 编辑弹窗 ──────────────────────────────────────────

/// 编辑弹窗的三个输入框 + 校验错误。builder 每帧重跑,状态挂在 `Rc` 上。
struct EditorForm {
    name: Entity<InputState>,
    command: Entity<InputState>,
    group: Entity<InputState>,
    /// `on_ok` 校验不过时点亮的红字;一旦通过就清掉。
    error: RefCell<Option<&'static str>>,
}

/// 打开「新增 / 编辑命令」弹窗。`existing = Some` 是编辑(id 沿用),`None` 是
/// 新增;`default_group` 给「从某个分组的标题行点新增」预填分组。
///
/// 浮层与弹窗**不共存**:宿主(`terminal_area`)在收到 [`PopoverEvent::Edit`]
/// 时先关浮层再调这里 —— 否则 Dialog 抢到的焦点会被浮层关闭时的焦点还原覆盖掉。
pub fn open_editor(
    store: Entity<AppStore>,
    existing: Option<SavedCommand>,
    default_group: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    // 守卫在建输入框**之前**判(与 `prompt::show_prompt` 同一条理由)
    if is_open(kind::COMMAND_EDITOR) {
        return;
    }
    let editing = existing.is_some();
    let (init_name, init_command, init_group) = match &existing {
        Some(cmd) => (
            cmd.name.clone(),
            cmd.command.clone(),
            cmd.group.clone().unwrap_or_default(),
        ),
        None => (
            String::new(),
            String::new(),
            default_group.unwrap_or_default(),
        ),
    };
    let id = existing.map(|c| c.id).unwrap_or_else(|| {
        let taken: HashSet<String> = store
            .read(cx)
            .command_library()
            .commands
            .iter()
            .map(|c| c.id.clone())
            .collect();
        crate::tree::gen_unique_id("cmd", |candidate| taken.contains(candidate))
    });

    let form = Rc::new(EditorForm {
        name: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("commandLibrary", "editor.namePlaceholder"))
                .default_value(init_name)
        }),
        command: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("commandLibrary", "editor.commandPlaceholder"))
                .default_value(init_command)
        }),
        group: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("commandLibrary", "editor.groupPlaceholder"))
                .default_value(init_group)
        }),
        error: RefCell::new(None),
    });
    let name_for_focus = form.name.clone();

    open_guarded(kind::COMMAND_EDITOR, window, cx, {
        let store = store.clone();
        move |dialog, _window, cx| {
            let form_ok = form.clone();
            let store_ok = store.clone();
            let id = id.clone();
            dialog
                .title(if editing {
                    t("commandLibrary", "editor.titleEdit")
                } else {
                    t("commandLibrary", "editor.titleNew")
                })
                .w(px(440.0))
                .overlay_closable(true)
                .footer(confirm_footer(
                    t("commandLibrary", "editor.save"),
                    Some(t("commandLibrary", "editor.cancel")),
                ))
                .child(render_editor_body(&form, &store, cx))
                .on_ok(move |_: &ClickEvent, window, cx| {
                    let name = form_ok.name.read(cx).value().to_string();
                    let command = form_ok.command.read(cx).value().to_string();
                    let group = form_ok.group.read(cx).value().to_string();
                    if name.trim().is_empty() || command.trim().is_empty() {
                        *form_ok.error.borrow_mut() =
                            Some(t("commandLibrary", "editor.errorRequired"));
                        // builder 每帧重跑读到 error 就画红字;这里主动要一帧
                        window.refresh();
                        return false;
                    }
                    store_ok.update(cx, |store, cx| {
                        store.upsert_saved_command(
                            SavedCommand {
                                id: id.clone(),
                                name,
                                command,
                                group: Some(group),
                            },
                            cx,
                        );
                    });
                    true
                })
        }
    });

    autofocus(&name_for_focus, window, cx);
}

fn render_editor_body(
    form: &Rc<EditorForm>,
    store: &Entity<AppStore>,
    cx: &mut App,
) -> impl IntoElement {
    let error = *form.error.borrow();
    let options = group_names(store.read(cx).command_library());
    let group_input = form.group.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .px(px(20.0))
        .child(field(
            t("commandLibrary", "editor.nameLabel"),
            None,
            Input::new(&form.name),
        ))
        .child(field(
            t("commandLibrary", "editor.commandLabel"),
            Some(t("commandLibrary", "editor.commandHint")),
            Input::new(&form.command),
        ))
        .child(field(
            t("commandLibrary", "editor.groupLabel"),
            Some(t("commandLibrary", "editor.groupHint")),
            div()
                .flex()
                .gap(px(8.0))
                .child(div().flex_1().child(Input::new(&form.group)))
                .child(
                    // 与 SSH 表单的分组选择同款:▾ 弹菜单列出已有分组,选中即回填
                    ui::ghost_button("cmd-group-pick", "▾")
                        .when(options.is_empty(), |el| el.opacity(0.4))
                        .on_click(
                            move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                                if options.is_empty() {
                                    return;
                                }
                                let entries: Vec<menu::MenuEntry> = options
                                    .iter()
                                    .map(|name| {
                                        let group = group_input.clone();
                                        let name = name.clone();
                                        menu::item(name.clone(), move |window, cx| {
                                            group.update(cx, |s, cx| {
                                                s.set_value(name.clone(), window, cx)
                                            });
                                        })
                                    })
                                    .collect();
                                menu::show(event.position(), entries, window, cx);
                            },
                        ),
                ),
        ))
        .when_some(error, |el, msg| {
            el.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_error())
                    .child(msg),
            )
        })
}

/// 一行带标签(+ 可选灰字提示)的表单字段。
fn field(
    label: impl Into<SharedString>,
    hint: Option<&'static str>,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(label.into()),
        )
        .child(control)
        .when_some(hint, |el, hint| {
            el.child(
                div()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(hint),
            )
        })
}

// ─── 分组弹窗(复用通用 prompt)────────────────────────────────

/// 「新建分组」输入框。重名时静默不建(与 SSH 面板同款:用户看到的就是那个组还在)。
pub fn prompt_new_group(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    show_prompt(
        t("commandLibrary", "group.newTitle"),
        t("commandLibrary", "group.newPlaceholder"),
        "",
        move |value, _window, cx| {
            store.update(cx, |store, cx| {
                store.create_command_group(&value, cx);
            });
        },
        window,
        cx,
    );
}

/// 「重命名分组」输入框,默认值是旧名(全选,多半是整个换掉)。
pub fn prompt_rename_group(
    store: Entity<AppStore>,
    old_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let default = old_name.clone();
    show_prompt(
        t("commandLibrary", "group.renameTitle"),
        t("commandLibrary", "group.newPlaceholder"),
        default,
        move |value, _window, cx| {
            store.update(cx, |store, cx| {
                store.rename_command_group(&old_name, &value, cx);
            });
        },
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(id: &str, name: &str, command: &str, group: Option<&str>) -> SavedCommand {
        SavedCommand {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            group: group.map(str::to_string),
        }
    }

    fn lib(commands: Vec<SavedCommand>, groups: &[&str]) -> CommandLibrary {
        CommandLibrary {
            commands,
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    #[test]
    fn 归桶_未分组在前_具名按首次出现_空组垫后() {
        let lib = lib(
            vec![
                cmd("1", "a", "a", Some("部署")),
                cmd("2", "b", "b", None),
                cmd("3", "c", "c", Some("Docker")),
                cmd("4", "d", "d", Some("部署")),
                cmd("5", "e", "e", Some("  ")), // 空白组名 = 未分组
            ],
            &["日志", "部署"],
        );
        let buckets = build_buckets(&lib);
        let keys: Vec<Option<&str>> = buckets.iter().map(|b| b.group.as_deref()).collect();
        assert_eq!(keys, vec![None, Some("部署"), Some("Docker"), Some("日志")]);
        assert_eq!(buckets[0].items.len(), 2, "未分组两条(含空白组名那条)");
        assert_eq!(buckets[1].items.len(), 2);
        assert!(buckets[3].items.is_empty(), "显式空组照样出桶");
    }

    #[test]
    fn 全部有组时没有未分组桶() {
        let lib = lib(vec![cmd("1", "a", "a", Some("部署"))], &[]);
        let buckets = build_buckets(&lib);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].group.as_deref(), Some("部署"));
        assert_eq!(group_names(&lib), vec!["部署"]);
    }

    #[test]
    fn 匹配_名称或命令子串_大小写不敏感() {
        let c = cmd("1", "查看容器", "docker ps -a", None);
        assert!(matches_query(&c, "DOCKER"));
        assert!(matches_query(&c, "容器"));
        assert!(matches_query(&c, "  ps "));
        assert!(matches_query(&c, ""));
        assert!(!matches_query(&c, "dk"), "不做子序列模糊");
    }

    #[test]
    fn 可见桶_搜索时丢空桶且忽略折叠() {
        let lib = lib(
            vec![
                cmd("1", "启动", "cd /opt && ./start", Some("部署")),
                cmd("2", "看日志", "tail -f app.log", Some("日志")),
                cmd("3", "清屏", "clear", None),
            ],
            &["Docker"],
        );
        let mut collapsed = HashSet::new();
        collapsed.insert("部署".to_string());

        // 不搜索:三个有命令的桶 + 空组 Docker;部署折叠、条目不画但总数在
        let all = visible_buckets(&lib, "", &collapsed);
        assert_eq!(all.len(), 4);
        let deploy = all
            .iter()
            .find(|b| b.group.as_deref() == Some("部署"))
            .unwrap();
        assert!(deploy.collapsed);
        assert_eq!(deploy.total, 1);
        assert!(deploy.items.is_empty());
        assert_eq!(flatten_rows(&all).len(), 2, "折叠桶的条目不进键盘游标");

        // 搜索:只剩命中的,折叠失效,空桶消失
        let hit = visible_buckets(&lib, "start", &collapsed);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].group.as_deref(), Some("部署"));
        assert!(!hit[0].collapsed);
        assert_eq!(hit[0].items[0].id, "1");

        assert!(visible_buckets(&lib, "不存在", &collapsed).is_empty());
    }

    #[test]
    fn 改名合并去重() {
        let groups = vec!["a".to_string(), "b".to_string(), " a ".to_string()];
        assert_eq!(merge_groups_on_rename(&groups, "a", "b"), vec!["b"]);
        assert_eq!(merge_groups_on_rename(&groups, "b", "c"), vec!["a", "c"]);
    }

    #[test]
    fn 运行载荷_去两头空白_按需补回车() {
        assert_eq!(run_payload("  ls -la \n", true), "ls -la\r");
        assert_eq!(run_payload("ls -la\n", false), "ls -la");
        assert_eq!(run_payload("   \n", true), "");
    }

    #[test]
    fn 归一化组名() {
        assert_eq!(normalize_group(Some(" 部署 ")), Some("部署"));
        assert_eq!(normalize_group(Some("   ")), None);
        assert_eq!(normalize_group(None), None);
    }

    /// 图标的所有顶点都落在单位方框内 —— 画出去的那一段会被裁掉,肉眼很难察觉
    #[test]
    fn 图标顶点都在单位方框内() {
        for (name, icon) in [
            ("commands", ICON_COMMANDS),
            ("play", ICON_PLAY),
            ("copy", ICON_COPY),
            ("edit", ICON_EDIT),
            ("trash", ICON_TRASH),
            ("plus", ICON_PLUS),
            ("folder-plus", ICON_FOLDER_PLUS),
            ("chevron-right", ICON_CHEVRON_RIGHT),
            ("close", ICON_CLOSE),
        ] {
            for shape in icon {
                let (points, _) = shape.geom.points();
                for (x, y) in points {
                    assert!(
                        (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                        "{name} 有顶点出框: ({x}, {y})"
                    );
                }
            }
        }
    }
}
