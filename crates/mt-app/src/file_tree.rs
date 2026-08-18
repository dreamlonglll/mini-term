//! 中栏:文件树。对应 `src/components/FileTree.tsx` 的主干。
//!
//! - 列目录走 [`mt_project::fs::list_directory`](mt_project::fs::list_directory)
//!   (`.gitignore` 过滤与排序都在那边,这里不重复实现);它是**阻塞**函数,
//!   一律丢 background executor,不能在主线程上跑;
//! - 目录变化走 [`mt_project::watch::FsWatcher`](mt_project::watch::FsWatcher):
//!   sink 里往 channel 丢,主线程上的前台任务醒来后失效缓存并重列 ——
//!   与 AI 状态、终端重绘是同一套跨线程唤醒模式;
//! - 双击文件用 [`mt_project::editor`](mt_project::editor) 打开(编辑器取自配置)。
//!
//! 文件拖进终端(旧版把路径写进 PTY)本轮不做,gpui 的拖放另开一批。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder,
    px,
};
use mt_project::fs::FileEntry;
use mt_project::watch::FsWatcher;

use crate::store::AppStore;
use crate::ui;

pub struct FileTree {
    store: Entity<AppStore>,
    /// 已列出的目录 → 子项。
    entries: HashMap<PathBuf, Vec<FileEntry>>,
    /// 正在列的目录(防重复排队)。
    loading: HashSet<PathBuf>,
    watcher: Arc<FsWatcher>,
    watched: HashSet<PathBuf>,
    /// 当前挂着的项目;换项目时整表作废。
    current_project: Option<String>,
    _fs_task: Task<()>,
}

impl FileTree {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            this.sync_project(cx);
            cx.notify();
        })
        .detach();

        let (tx, mut rx) = mpsc::unbounded::<PathBuf>();
        let watcher = Arc::new(FsWatcher::new(move |change| {
            // notify 自己的线程:只把「哪个目录变了」丢过去,重列在主线程排。
            let dir = match change.path.parent() {
                Some(parent) => parent.to_path_buf(),
                None => change.path.clone(),
            };
            let _ = tx.unbounded_send(dir);
        }));

        let fs_task = cx.spawn(async move |this, cx| {
            while let Some(dir) = rx.next().await {
                if this
                    .update(cx, |tree: &mut FileTree, cx| {
                        tree.invalidate(&dir, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        let mut this = Self {
            store,
            entries: HashMap::new(),
            loading: HashSet::new(),
            watcher,
            watched: HashSet::new(),
            current_project: None,
            _fs_task: fs_task,
        };
        this.sync_project(cx);
        this
    }

    /// 活动项目变了:清空缓存与监听,重列根目录。
    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let (project_id, root) = {
            let store = self.store.read(cx);
            match store.active_project() {
                Some(p) => (Some(p.id.clone()), Some(PathBuf::from(&p.path))),
                None => (None, None),
            }
        };
        if project_id == self.current_project {
            return;
        }
        for dir in std::mem::take(&mut self.watched) {
            self.watcher.unwatch(&dir);
        }
        self.entries.clear();
        self.loading.clear();
        self.current_project = project_id;
        if let Some(root) = root {
            self.load_dir(root.clone(), root, cx);
        }
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .active_project()
            .map(|p| PathBuf::from(&p.path))
    }

    /// 列一个目录(后台线程)+ 挂监听。
    fn load_dir(&mut self, root: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        if self.loading.contains(&dir) {
            return;
        }
        self.loading.insert(dir.clone());

        if self.watched.insert(dir.clone())
            && let Err(err) = self
                .watcher
                .watch(&dir, &root.to_string_lossy().to_string())
        {
            eprintln!("[files] 监听 {} 失败: {err:#}", dir.display());
        }

        let task_dir = dir.clone();
        let task_root = root.clone();
        cx.spawn(async move |this, cx| {
            // list_directory 是阻塞 IO(还要逐级读 .gitignore),必须离开主线程
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::fs::list_directory(&task_root, &task_dir) })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                tree.loading.remove(&dir);
                match result {
                    Ok(entries) => {
                        tree.entries.insert(dir, entries);
                    }
                    Err(err) => eprintln!("[files] 列目录失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 目录内容变了:已列过的重列一次。
    fn invalidate(&mut self, dir: &Path, cx: &mut Context<Self>) {
        if !self.entries.contains_key(dir) {
            return;
        }
        let Some(root) = self.project_root(cx) else {
            return;
        };
        self.load_dir(root, dir.to_path_buf(), cx);
    }

    fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(project_id) = self.current_project.clone() else {
            return;
        };
        let key = path.to_string_lossy().to_string();
        let expanded = self.store.read(cx).is_dir_expanded(&project_id, &key);
        self.store.update(cx, |store, cx| {
            store.set_dir_expanded(&project_id, &key, !expanded, cx)
        });
        if !expanded {
            if let Some(root) = self.project_root(cx) {
                self.load_dir(root, path.clone(), cx);
            }
        } else {
            self.watched.remove(&path);
            self.watcher.unwatch(&path);
        }
        cx.notify();
    }

    fn open_file(&self, path: PathBuf, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let config = store.config();
        let editors: Vec<mt_project::editor::Editor> = config
            .editors
            .iter()
            .map(|e| mt_project::editor::Editor {
                name: e.name.clone(),
                command: e.command.clone(),
            })
            .collect();
        let editor = mt_project::editor::select_editor(
            &editors,
            config.default_editor.as_deref(),
            None,
        )
        .cloned();
        // spawn 外部进程同样可能卡(网络盘 / 杀软),丢后台
        cx.background_executor()
            .spawn(async move {
                let result = match editor {
                    Some(_) => mt_project::editor::open_in_editor(editor.as_ref(), &path),
                    // 没配编辑器就用系统默认程序打开,别只给一句报错
                    None => mt_project::editor::open_path_with_default_app(&path),
                };
                if let Err(err) = result {
                    eprintln!("[files] 打开失败: {err:#}");
                }
            })
            .detach();
    }

    /// 把树按展开状态拍平成可渲染的行。
    fn rows(&self, project_id: &str, dir: &Path, depth: usize, cx: &App, out: &mut Vec<Row>) {
        let Some(entries) = self.entries.get(dir) else {
            return;
        };
        let store = self.store.read(cx);
        for entry in entries {
            let key = entry.path.to_string_lossy().to_string();
            let expanded = entry.is_dir && store.is_dir_expanded(project_id, &key);
            out.push(Row {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                ignored: entry.ignored,
                depth,
                expanded,
            });
            if expanded {
                self.rows(project_id, &entry.path, depth + 1, cx, out);
            }
        }
    }
}

struct Row {
    name: String,
    path: PathBuf,
    is_dir: bool,
    ignored: bool,
    depth: usize,
    expanded: bool,
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .items_center()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(px(11.0))
            .text_color(ui::text_muted())
            .child("文件");

        let Some(project_id) = self.current_project.clone() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header);
        };
        let Some(root) = self.project_root(cx) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header);
        };

        let mut rows = Vec::new();
        self.rows(&project_id, &root, 0, cx, &mut rows);

        let mut list = div()
            .id("file-tree-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll();
        for row in rows {
            list = list.child(self.render_row(row, cx));
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            .child(list)
    }
}

impl FileTree {
    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let path = row.path.clone();
        let is_dir = row.is_dir;
        let indent = px(6.0 + row.depth as f32 * 12.0);
        let color = if row.ignored {
            ui::text_muted()
        } else if row.is_dir {
            ui::color_folder()
        } else {
            ui::color_file()
        };

        div()
            .id(SharedString::from(format!("fs-{}", row.path.display())))
            .flex()
            .items_center()
            .gap(px(4.0))
            .pl(indent)
            .pr(px(6.0))
            .py(px(2.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(color)
            .hover(|el| el.bg(ui::bg_overlay()))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                if is_dir {
                    this.toggle_dir(path.clone(), cx);
                } else if event.click_count() >= 2 {
                    this.open_file(path.clone(), cx);
                }
            }))
            .child(
                div()
                    .w(px(10.0))
                    .text_color(ui::text_muted())
                    .when(row.is_dir, |el| {
                        el.child(if row.expanded { "▾" } else { "▸" })
                    }),
            )
            .child(div().child(row.name))
            .into_any_element()
    }
}
