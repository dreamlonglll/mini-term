//! 文件树图标(对照 `src/utils/fileIcon.ts` + `src/components/FileTree.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版调 `@baybreezy/file-extension-icon`(Material Icon Theme),按文件名/扩展名
//! 返回一枚**专属图标**。GPUI 侧搬的是同一个包的同一批图:官方 SVG 的那条 `d`
//! 经 `tools/gen_file_icons.mjs` 烘焙进 [`super::file_art`],渲染仍是自绘
//! (为什么不能让 gpui 去读 SVG,判据在 [`super::vector`] 的模块注释)。
//! 也就是说**几何与颜色都是官方的**,一类型一张图,不是按类别归并的简化标记。
//!
//! 本模块只剩「文件名 → 哪枚图」这条查表规则和那个 Element;图本身全在生成物里。
//!
//! # 查表顺序即语义
//!
//! **整名 → 前缀 → 扩展名 → 兜底**,这条顺序是 Material Icon Theme 的核心语义,
//! 错了整张表就废了:`Cargo.lock` 是锁文件不是 toml,`Dockerfile` 压根没有扩展名,
//! `.gitignore` 的 "gitignore" 是文件名不是扩展名。目录另走一张按目录名的表
//! (`src` / `node_modules` / `.github` 各有专属图),分开合两态。
//!
//! # 已知偏差
//!
//! 全部记在 [`super::file_art`] 的生成日志里,现存三类共 9 枚:Kotlin 与 `.idea`
//! 的渐变降级成单色(`paint_path` 一次只吃一个纯色,与 brand.rs 的 Gemini/Qwen 同因)、
//! Docker 那两枚的 clip 求交失败而少一笔。跑
//! `node tools/verify_file_icons.mjs` 可以逐枚比对官方原图与烘焙结果。
//!
//! # 宿主接线(mt-app 的文件树)
//!
//! ```ignore
//! use mt_ui::icons::FileIcon;
//! // 目录:展开态自己传;文件:后两个参数给 false
//! row = row.child(FileIcon::new(&entry.name, entry.is_dir, expanded).size(px(14.0)));
//! ```
//!
//! 项目根节点仍应优先用技术栈徽标(原版 `FileTree.tsx:349` 就是这么做的):
//!
//! ```ignore
//! match dir_kind {
//!     Some(kind) => TechIcon::new(kind).size(px(14.0)).into_any_element(),
//!     None => FileIcon::new(&entry.name, true, expanded).into_any_element(),
//! }
//! ```

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::file_art::{
    ARTS, FILE_EXACT, FILE_EXT, FILE_FALLBACK, FILE_PREFIX, FOLDER_FALLBACK, FOLDER_NAMES,
    FOLDER_OPEN_FALLBACK, FileArt,
};
use super::vector::VectorIcon;

/// 文件名/目录名 → 图标。`is_open` 只对目录有意义。
///
/// 认不出来也总有图可画(通用文件/文件夹),所以不返回 `Option`。
pub fn art_of(name: &str, is_dir: bool, is_open: bool) -> &'static FileArt {
    // 绝大多数文件名本来就是小写:先扫一眼,真有大写才分配那份 String
    // (文件树满屏时这条每行每帧都要走一遍)
    let lowered: String;
    let lower: &str = if name.bytes().any(|b| b.is_ascii_uppercase()) {
        lowered = name.to_ascii_lowercase();
        &lowered
    } else {
        name
    };
    if is_dir {
        let idx = lookup_pair(FOLDER_NAMES, lower)
            .map(|(closed, open)| if is_open { open } else { closed })
            .unwrap_or(if is_open {
                FOLDER_OPEN_FALLBACK
            } else {
                FOLDER_FALLBACK
            });
        return art(idx);
    }

    // 1. 整名命中(Cargo.lock 不是 toml,Dockerfile 没有扩展名)
    if let Some(idx) = lookup(FILE_EXACT, lower) {
        return art(idx);
    }
    // 2. 前缀命中(.env.production / Dockerfile.dev / docker-compose.override.yml)。
    //    表已按前缀长度倒序,先中的就是最长的那条
    for (prefix, idx) in FILE_PREFIX {
        if lower.starts_with(prefix) {
            return art(*idx);
        }
    }
    // 3. 扩展名。`.gitignore` 这类点开头的裸文件没有扩展名(整名已在第 1 步兜住)
    if let Some(idx) = extension_of(lower).and_then(|ext| lookup(FILE_EXT, ext)) {
        return art(idx);
    }
    art(FILE_FALLBACK)
}

fn art(idx: u16) -> &'static FileArt {
    // 下标由生成器写死,越界只可能是生成物被手改坏了 —— 那也画个通用文件图标,
    // 不值得为它 panic 掉整个文件树
    ARTS.get(idx as usize).unwrap_or(&ARTS[0])
}

/// 扩展名(小写、不含点)。没有扩展名或以点开头的裸文件返回 `None`。
fn extension_of(lower: &str) -> Option<&str> {
    let idx = lower.rfind('.')?;
    // `.gitignore` 的点在 0 位:那是「隐藏文件」不是「扩展名」
    if idx == 0 || idx + 1 >= lower.len() {
        return None;
    }
    Some(&lower[idx + 1..])
}

/// 生成器把键排好了序,这里二分。
fn lookup(table: &[(&str, u16)], key: &str) -> Option<u16> {
    table
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|i| table[i].1)
}

fn lookup_pair(table: &[(&str, (u16, u16))], key: &str) -> Option<(u16, u16)> {
    table
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|i| table[i].1)
}

/// 文件树用的图标。
///
/// ```ignore
/// FileIcon::new(&entry.name, entry.is_dir, expanded).size(px(14.0))
/// ```
#[derive(IntoElement)]
pub struct FileIcon {
    art: &'static FileArt,
    size: Pixels,
    color: Option<Hsla>,
}

impl FileIcon {
    /// 默认 14px —— 与原版 `w-3.5 h-3.5` 一致。
    pub fn new(name: &str, is_dir: bool, is_open: bool) -> Self {
        Self::of_art(art_of(name, is_dir, is_open))
    }

    /// 通用文件夹(拿不到目录名的场合:项目列表、拖拽预览)。
    pub fn folder(is_open: bool) -> Self {
        Self::of_art(art(if is_open {
            FOLDER_OPEN_FALLBACK
        } else {
            FOLDER_FALLBACK
        }))
    }

    pub fn of_art(art: &'static FileArt) -> Self {
        Self {
            art,
            size: px(14.0),
            color: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 把整枚图标压成一个颜色,盖掉官方多色。
    ///
    /// 文件树里 `.gitignore` 掉的行压成 muted 走这里 —— 原版那边是给
    /// `<img>` 挂父级 opacity,GPUI 侧没有位图可以挂,改成与文字同色的单色化,
    /// 「被忽略」这层语义比保留彩色更要紧。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// 这一行拿到的是哪枚图(单测与对账用)。
    pub fn art(&self) -> &'static FileArt {
        self.art
    }
}

impl RenderOnce for FileIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let icon = VectorIcon::new(self.art.shapes, self.size);
        match self.color {
            Some(color) => icon.force_ink(color),
            None => icon,
        }
    }
}

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [super::vector::Shape]> {
    ARTS.iter().map(|a| a.shapes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图标身份用「是不是同一张形状表」比,不比几何也不比名字 ——
    /// 名字只是「第一个请到这枚图的 key」,换个生成顺序就会变。
    fn same(a: &'static FileArt, b: &'static FileArt) -> bool {
        std::ptr::eq(a.shapes.as_ptr(), b.shapes.as_ptr())
    }

    fn of(name: &str) -> &'static FileArt {
        art_of(name, false, false)
    }

    #[test]
    fn 目录两态各有各的图() {
        let closed = art_of("src", true, false);
        let open = art_of("src", true, true);
        assert!(!same(closed, open), "src 的开合两态应是两张图");
        // 目录不看扩展名:`build.gradle/` 这种目录名不能被判成 java
        assert!(same(
            art_of("build.gradle", true, false),
            art_of("随便什么没登记的目录名", true, false)
        ));
    }

    #[test]
    fn 目录按名换图() {
        // 原版有、改造前的 GPUI 版没有的那条能力
        let generic = art_of("没登记过的目录", true, false);
        for name in ["src", "node_modules", ".github", "dist", "test", "docs"] {
            assert!(
                !same(art_of(name, true, false), generic),
                "{name}/ 应该有专属图标"
            );
        }
    }

    #[test]
    fn 特殊文件名压过扩展名() {
        // 这条是 Material Icon Theme 的核心语义,顺序错了整张表就废了
        assert!(!same(of("Cargo.lock"), of("Cargo.toml")), "锁文件不是 toml");
        assert!(!same(of("package-lock.json"), of("a.json")), "锁文件不是 json");
        assert!(!same(of("go.mod"), of("a.mod")), "go.mod 该是 go 的图");
        assert!(!same(of("package.json"), of("a.json")), "package.json 是 node 的图");
        // 同族的锁文件共用一张锁图
        assert!(same(of("Cargo.lock"), of("poetry.lock")));
        // 反过来钉一条:`tsconfig.json` 在 Material 里**没有**专属图,与普通 json 同图。
        // 生成器据此把它当冗余条目从整名表里剔了 —— 哪天上游补了图,这条会失败提醒重跑
        assert!(same(of("tsconfig.json"), of("a.json")));
    }

    #[test]
    fn 大小写不敏感() {
        assert!(same(of("DOCKERFILE"), of("dockerfile")));
        assert!(same(of("Makefile"), of("makefile")));
        assert!(same(of("README.MD"), of("readme.md")));
        assert!(same(of("Main.RS"), of("main.rs")));
        assert!(same(art_of("SRC", true, false), art_of("src", true, false)));
    }

    #[test]
    fn 点开头的裸文件不当成扩展名() {
        // `.gitignore` 的 "gitignore" 不是扩展名,整名表必须先兜住它
        assert!(!same(of(".gitignore"), of("未登记.qqq")), ".gitignore 该有专属图");
        assert!(!same(of(".editorconfig"), of("未登记.qqq")));
        // 没登记的点开头文件回落通用图标,而不是把 "unknownrc" 当扩展名
        assert!(same(of(".unknownrc"), of("未登记.qqq")));
        assert_eq!(extension_of(".gitignore"), None);
        assert_eq!(extension_of("a.rs"), Some("rs"));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of("trailing."), None);
    }

    #[test]
    fn 前缀规则兜住变体() {
        assert!(same(of("Dockerfile.dev"), of("Dockerfile")), "Dockerfile.dev 该是 docker 图");
        assert!(same(of(".env.production"), of(".env")));
        assert!(same(of("docker-compose.override.yml"), of("docker-compose.yml")));
        // 前缀表按长度倒序,别让短的抢在长的前面
        assert!(!same(of("docker-compose.yml"), of("a.yml")), "compose 不是普通 yaml");
    }

    #[test]
    fn 常见扩展名各有各的图() {
        // 一类型一张图是本次改造的全部意义所在:同类不同扩展名不能再撞一起
        let distinct = [
            "main.rs", "main.go", "a.py", "A.java", "a.rb", "a.php", "index.ts", "index.js",
            "App.vue", "page.svelte", "theme.css", "style.scss", "data.yaml", "notes.md",
            "photo.png", "clip.mp4", "song.mp3", "app.sqlite3", "deploy.sh", "run.ps1",
        ];
        for (i, a) in distinct.iter().enumerate() {
            for b in &distinct[i + 1..] {
                assert!(!same(of(a), of(b)), "{a} 与 {b} 拿到了同一张图");
            }
            assert!(!same(of(a), of("未登记.qqq")), "{a} 落到通用图标了");
        }
        // .tsx/.jsx 是 react 图,与纯 ts/js 不同 —— 原版就是分开的
        assert!(!same(of("app.tsx"), of("index.ts")));
        assert!(!same(of("app.jsx"), of("index.js")));
    }

    #[test]
    fn 索引表可二分且无重复键() {
        // 键没排序的话二分会随机查不到,是最容易悄悄写错的一类
        for table in [FILE_EXACT, FILE_EXT] {
            let keys: Vec<&str> = table.iter().map(|(k, _)| *k).collect();
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "键没有严格升序");
            for k in &keys {
                assert_eq!(*k, k.to_ascii_lowercase(), "键 {k} 不是小写");
            }
        }
        let folder_keys: Vec<&str> = FOLDER_NAMES.iter().map(|(k, _)| *k).collect();
        assert!(folder_keys.windows(2).all(|w| w[0] < w[1]), "目录键没有严格升序");
        // 前缀表是线性匹配,要求的是长的排前面
        let lens: Vec<usize> = FILE_PREFIX.iter().map(|(k, _)| k.len()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "前缀表没按长度倒序");
    }

    #[test]
    fn 每枚图都有下标可达且画得出东西() {
        for (i, a) in ARTS.iter().enumerate() {
            assert!(!a.shapes.is_empty(), "第 {i} 枚 `{}` 是空图", a.name);
        }
        // 三个兜底下标必须在范围内,否则 art() 会静默回落到第 0 枚
        for idx in [FILE_FALLBACK, FOLDER_FALLBACK, FOLDER_OPEN_FALLBACK] {
            assert!((idx as usize) < ARTS.len(), "兜底下标 {idx} 越界");
        }
        for (k, idx) in FILE_EXACT.iter().chain(FILE_EXT).chain(FILE_PREFIX) {
            assert!((*idx as usize) < ARTS.len(), "{k} 的下标越界");
        }
        for (k, (c, o)) in FOLDER_NAMES {
            assert!((*c as usize) < ARTS.len() && (*o as usize) < ARTS.len(), "{k} 的下标越界");
        }
    }
}
