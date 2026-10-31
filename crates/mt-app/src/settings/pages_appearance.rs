//! 设置面板的 appearance(语言 / 主题 / 外置皮肤)与 font(字号 / 字族 / 连字)两页。
//!
//! 外置皮肤那一段是本文件的大头:卡片数据([`ThemeCard`])、卡片缩略图
//! ([`ThemeThumbnail`])、导入(目录 / zip)、删除确认都在这儿。两条写盘路径
//! 统一走 `run_theme_job` 丢后台;「更多皮肤」不写盘,它只是把浏览器指向仓库的
//! 皮肤库([`THEME_GALLERY_URL`])。

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Hsla, ImageAssetLoader, InteractiveElement, IntoElement,
    ParentElement, PathPromptOptions, RenderImage, Resource, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use mt_ui::theme_bridge::{ThemePackListing, ThemeSlot, resolve_theme_pack};

use crate::i18n::{Locale, t, tr};
use crate::prompt::Confirm;
use crate::ui;

use super::widgets::{
    banner, choice_group, font_family_input, mini_bar, page_root, section, toggle_row,
};
use super::{SettingsPage, SettingsView, choice_value};

/// 「更多皮肤」按钮的去处:仓库里的成品皮肤库(`theme/`)。
///
/// 那批皮肤**不随安装包分发**(三平台打包都只装二进制),所以界面这里给的是
/// 一条外链而不是一次本地生成 —— 用户在浏览器里挑,下载后走「添加皮肤」导入。
///
/// 指向 `main` 而非某个 tag:皮肤库是会持续添新的分发目录,钉在发版 tag 上
/// 会让老版本用户永远停在当时那几份。
const THEME_GALLERY_URL: &str = "https://github.com/dreamlonglll/mini-term/tree/main/theme";

// ─── 外置皮肤卡片的数据 ───────────────────────────────────────

/// 一张皮肤卡片要画的东西(刷新列表时算一次,不每帧重解析色值)。
///
/// **每一项都取 [`resolve_theme_pack`] 算出来的成品值,不再自己拍脑袋** ——
/// 预览与真实界面必须是同一份数据喂出来的,否则卡片是「看着像」而不是「就是」。
/// 三个半透明度尤其不能写死:`surfaceOpacity` / `terminalOpacity` /
/// `backgroundDim` 都是包作者能在 theme.json 的 `effects` 里改的。
pub(super) struct ThemeCard {
    /// **themes/ 下的目录名**,卡片副标题、应用、删除、读资源全用它。
    /// 见 [`ThemeCard::from_listing`]。
    theme_id: String,
    name: String,
    background: Hsla,
    /// 迷你侧栏底色,已含 `surfaceOpacity`(无背景图的包 = 不透明)。
    panel_surface: Hsla,
    /// 迷你终端区底色,已含 `terminalOpacity`(同上)。
    terminal_surface: Hsla,
    accent: Hsla,
    text: Hsla,
    /// 背景图氛围层参数(包里没声明 / 文件不在盘上 = `None`)。
    ///
    /// 与窗口级那一层(`main.rs` 的 `mt_ui::background_art`)**是同一份数据、
    /// 同一个 Element**:cover + `focus` 百分比定位 + 包声明的压暗纱罩。
    art: Option<mt_ui::theme_bridge::BackgroundArt>,
}

impl ThemeCard {
    /// 列表项 → 卡片。
    ///
    /// **身份取 `listing.theme_id`(目录名),不是 `def.id`** —— 原版
    /// `SettingsModal.tsx:1982-1984` 的 `key` / `subtitle` 与
    /// `selectCustom`(`:1834`)、`deletePack`(`:1893`)用的都是
    /// `ThemePackMeta.themeId`,而那个字段是「themes/ 下目录名」
    /// (`themePackManager.ts:75-81`)。
    ///
    /// 拿 `def.id` 当身份的后果实测过:用户把包目录改名成 `ember-new`、
    /// theme.json 里仍写着 `"id": "ember-dusk"`,卡片列得出来,一点「应用」
    /// 就落到 `packs.read("ember-dusk")` 上找不到目录,红条「皮肤应用失败」。
    fn from_listing(listing: &ThemePackListing) -> Self {
        let ThemePackListing { theme_id, def, dir } = listing;
        let applied = resolve_theme_pack(def, Some(dir));
        Self {
            theme_id: theme_id.clone(),
            name: def.name.clone(),
            background: applied.color(ThemeSlot::Background),
            panel_surface: ui::with_alpha(
                applied.color(ThemeSlot::Panel),
                applied.surface_opacity,
            ),
            // 终端底色直接取成品:带图的包这里已经是 `background × terminalOpacity`,
            // 不带图的包是作者声明的 `terminal.background` 原色(不透明)
            terminal_surface: applied.terminal.background,
            accent: applied.color(ThemeSlot::Accent),
            text: applied.color(ThemeSlot::Text),
            // 图找不找得到由 `resolve_theme_pack` 判(它按不到图 = 没有背景图处理,
            // 与真实应用时同一条判据),这里不再自己 `is_file()` 一遍
            art: applied.background.clone(),
        }
    }

    /// 这张卡片缩略图的 key;包里没有背景图就没有。
    fn thumb_key(&self) -> Option<ThemeThumbKey> {
        self.art.as_ref().map(|art| ThemeThumbKey {
            path: Arc::from(art.image.as_path()),
            target: theme_thumb_target(),
        })
    }
}

// ─── 皮肤卡片的缩略图 ─────────────────────────────────────────

/// 卡片宽度上限与内边距(逻辑像素)。卡片样式与缩略图目标尺寸共用这一对,
/// 改一处两边跟着走。
const THEME_CARD_MAX_W: f32 = 300.0;
const THEME_CARD_PADDING: f32 = 12.0;
/// 预览框宽高比(理由见 [`SettingsView::render_theme_card`] 里的注释)。
const THEME_PREVIEW_ASPECT: f32 = 16.0 / 9.0;
/// 缩略图按显示尺寸的几倍生成:2× 覆盖到 200% 缩放都不糊。
const THEME_THUMB_SCALE: f32 = 2.0;

/// 缩略图要盖满的目标框(设备像素):预览框的最大显示尺寸 × [`THEME_THUMB_SCALE`]。
fn theme_thumb_target() -> (u32, u32) {
    let w = (THEME_CARD_MAX_W - 2.0 * THEME_CARD_PADDING) * THEME_THUMB_SCALE;
    let h = w / THEME_PREVIEW_ASPECT;
    (w.ceil() as u32, h.ceil() as u32)
}

/// 缩略图尺寸:等比缩到**刚好盖满**目标框 —— 卡片按 cover 铺,只「放得进」
/// (contain)的话短的那一维会被拉糊。只缩不放(比框还小的图原样);宽高至少 1。
fn thumbnail_size(image: (u32, u32), target: (u32, u32)) -> (u32, u32) {
    let (w, h) = image;
    if w == 0 || h == 0 {
        return image;
    }
    let scale = (f64::from(target.0) / f64::from(w)).max(f64::from(target.1) / f64::from(h));
    if scale >= 1.0 {
        return image;
    }
    let fit = |len: u32| ((f64::from(len) * scale).round() as u32).max(1);
    (fit(w), fit(h))
}

/// [`ThemeThumbnail`] 的键:原图路径 + 目标框。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ThemeThumbKey {
    path: Arc<Path>,
    target: (u32, u32),
}

/// 皮肤卡片的缩略图,挂在 gpui 的资源系统上:后台线程整张解码 → 按
/// [`thumbnail_size`] 缩 → 原图随即丢掉,缓存里只留缩略图。
///
/// 此前卡片直接画原图(与窗口级背景同一个 `ImageAssetLoader` + 路径 key):
/// 两三百像素宽的卡片,每张都按原分辨率常驻 —— 2560×1440 的背景是 15 MB 内存
/// 加同样大的显存,而且谁也不放。改成**独立的资源类型**:缓存条目只归设置页管,
/// 离开外观页 / 关掉设置时放得干净([`release_theme_thumbs`]),也不会把正在当
/// 窗口背景的那份原图连带放掉 —— 窗口背景照旧画原图,一个字节没动。
enum ThemeThumbnail {}

impl gpui::Asset for ThemeThumbnail {
    type Source = ThemeThumbKey;
    /// `None` = 解不出来(文件不在 / 格式不认),卡片只剩纯色底,与窗口背景
    /// 解不出来时同一个观感。
    type Output = Option<Arc<RenderImage>>;

    fn load(
        key: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        // 解码原样借 `ImageAssetLoader` 的,格式覆盖与 svg 栅格化与窗口背景一致。
        // 它返回的 future 连同下面的缩放整个在后台线程跑(gpui 的 `CachedLoad::new`
        // 把 load 出来的 future 丢给 background executor),原图只在这里活一下
        let decode = <ImageAssetLoader as gpui::Asset>::load(Resource::Path(key.path.clone()), cx);
        async move {
            let full = decode.await.ok()?;
            downscale(&full, key.target)
        }
    }
}

/// 整张位图 → 缩略图。只取第一帧 —— 窗口级背景本来也只画第一帧。
fn downscale(full: &RenderImage, target: (u32, u32)) -> Option<Arc<RenderImage>> {
    let size = full.size(0);
    let w = u32::try_from(size.width.0).ok()?;
    let h = u32::try_from(size.height.0).ok()?;
    let (tw, th) = thumbnail_size((w, h), target);
    // RenderImage 的帧是 BGRA,这里借 `Rgba<u8>` 当四通道容器:缩放是逐通道的
    // 线性组合,不解释通道顺序,缩完还是 BGRA(与 frost.rs 同一手法)。借用原图
    // 字节,不先拷一份 15 MB
    let src = image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(w, h, full.as_bytes(0)?)?;
    // Triangle 在缩小时按比例放宽采样核,是真正的区域平均,几倍缩小也不出摩尔纹
    let small = image::imageops::resize(&src, tw, th, image::imageops::FilterType::Triangle);
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(small)])))
}

/// 把这些缩略图从资源缓存与图集里放掉。`window` 的口径同
/// `file_viewer::mermaid::release_mermaid_assets`:渲染途中递当前窗口(它此刻被摘出了
/// `App.windows`),`on_release` 传 `None`。只对要过的 key 调用 —— `fetch_asset`
/// 对没见过的 key 会先发起一次生成。
pub(super) fn release_theme_thumbs(
    keys: &[ThemeThumbKey],
    cx: &mut App,
    mut window: Option<&mut Window>,
) {
    for key in keys {
        if let Some(Some(image)) = cx.fetch_asset::<ThemeThumbnail>(key) {
            cx.drop_image(image, window.as_deref_mut());
        }
        cx.remove_asset::<ThemeThumbnail>(key);
    }
}

impl SettingsView {
    // ── appearance 页 ──

    pub(super) fn render_appearance_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let config = self.store.read(cx).config();
        let custom = config.custom_theme_id.clone();
        let theme = config.theme.clone();
        let follow = config.terminal_follow_theme;

        // 主题段:激活自定义皮肤时三个按钮全不高亮
        let theme_value = choice_value(custom.as_deref(), &theme).to_string();
        let mut theme_group = choice_group();
        for (value, label_key) in [
            ("dark", "appearance.themeDark"),
            ("light", "appearance.themeLight"),
            ("auto", "appearance.themeAuto"),
        ] {
            let selected = theme_value == value;
            theme_group = theme_group.child(
                ui::choice_button(
                    SharedString::from(format!("theme-{value}")),
                    t("settings", label_key),
                    selected,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    // 切主题 = 退出外置皮肤(`set_theme_mode` 内部自己清
                    // `custom_theme_id`,页面侧不必再清一遍)
                    this.store
                        .update(cx, |store, cx| store.set_theme_mode(value, window, cx));
                })),
            );
        }

        page_root()
            .child(section("appearance.language").child(ui::setting_row(
                t("settings", "appearance.languageLabel"),
                None,
                false,
                self.render_language_toggle(cx),
            )))
            .child(
                section("appearance.theme")
                    .child(theme_group)
                    .child(toggle_row(
                        "terminal-follow-theme",
                        "appearance.terminalFollowTheme",
                        "appearance.terminalFollowThemeDesc",
                        follow,
                        false,
                        |this, next, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.set_terminal_follow_theme(next, window, cx)
                            });
                        },
                        cx,
                    )),
            )
            .child(self.render_theme_packs(window, cx))
            .into_any_element()
    }

    /// 语言切换段控件。逐条对照 `src/components/LanguageToggle.tsx`:
    /// 两个选项、各写各自的**母语名**(中文 / English —— endonym 永不翻译)、
    /// 选中项 accent 底色白字,未选中透明底淡字。
    fn render_language_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.store.read(cx).locale();
        let mut seg = div()
            .flex()
            .rounded(px(4.0))
            .overflow_hidden()
            .border_1()
            .border_color(ui::border_default());
        for option in Locale::ALL {
            let active = option == current;
            seg = seg.child(
                div()
                    .id(SharedString::from(format!("lang-{}", option.code())))
                    .px(px(12.0))
                    .py(px(3.0))
                    .text_size(ui::font_px(11.0))
                    .cursor_pointer()
                    .when(active, |el| {
                        el.bg(ui::accent()).text_color(ui::bg_base())
                    })
                    .when(!active, |el| {
                        el.text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.store.update(cx, |store, cx| store.set_locale(option, cx));
                    }))
                    // 永远显示母语名,不随当前语言变
                    .child(option.native_name()),
            );
        }
        seg.into_any_element()
    }

    /// 外置皮肤段(原版 `CustomThemePacksSection`)。
    fn render_theme_packs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let active = self.store.read(cx).config().custom_theme_id.clone();

        // 标题行 + 五个小按钮。`flex_wrap` 允许换行 —— 680px 弹窗里英文文案会贴边
        let actions = div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .child(
                ui::ghost_button("theme-add", t("settings", "themes.addPack")).on_click(
                    cx.listener(|this, _, _window, cx| this.import_theme_dir(cx)),
                ),
            )
            .child(
                ui::ghost_button("theme-zip", t("settings", "themes.importZip")).on_click(
                    cx.listener(|this, _, _window, cx| this.import_theme_zip(cx)),
                ),
            )
            .child(
                ui::ghost_button("theme-gallery", t("settings", "themes.browseGallery")).on_click(
                    |_, _window, cx: &mut App| cx.open_url(THEME_GALLERY_URL),
                ),
            )
            .child(
                ui::ghost_button("theme-open-dir", t("settings", "themes.openDir")).on_click(
                    cx.listener(|_this, _, _window, cx| {
                        let root = crate::theme::theme_packs().root().to_path_buf();
                        // 建目录 + spawn 文件管理器都是阻塞 IO(网络盘 / 杀软下会卡),
                        // 丢后台。失败就地写进主题段的错误行,不走
                        // `fs_ops::open_external` —— 设置是弹窗,toast 在遮罩下看不见
                        cx.spawn(async move |this, cx| {
                            let result = cx
                                .background_executor()
                                .spawn(async move {
                                    let _ = std::fs::create_dir_all(&root);
                                    crate::fs_ops::reveal_in_file_manager(&root)
                                })
                                .await;
                            if let Err(err) = result {
                                eprintln!("[settings] 打开主题目录失败: {err}");
                                let _ = this.update(cx, |this: &mut Self, cx| {
                                    this.theme_error = Some(err.to_string());
                                    cx.notify();
                                });
                            }
                        })
                        .detach();
                    }),
                ),
            )
            .child(
                ui::ghost_button("theme-refresh", t("settings", "themes.refresh")).on_click(
                    cx.listener(|this, _, _window, cx| this.refresh_theme_packs(cx)),
                ),
            );

        let list: AnyElement = if self.theme_cards.is_empty() {
            ui::settings_card()
                .py(px(16.0))
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("settings", "themes.empty")),
                )
                .into_any_element()
        } else {
            // 原版 `grid grid-cols-2 gap-2`;gpui 没有 grid,用可换行的 flex 铺
            let mut grid = div().flex().flex_wrap().gap(px(8.0));
            for (idx, card) in self.theme_cards.iter().enumerate() {
                // 背景图画缩略图(见 [`ThemeThumbnail`])。先记后要:use_asset 一旦
                // 调用,资源系统里就有了这个 key 的任务,放的时候要找得到它
                let thumb = card.thumb_key().and_then(|key| {
                    self.theme_thumbs.insert(key.clone());
                    window.use_asset::<ThemeThumbnail>(&key, cx).flatten()
                });
                grid = grid.child(self.render_theme_card(idx, card, thumb, active.as_deref(), cx));
            }
            grid.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(ui::settings_section_title(t(
                        "settings",
                        "themes.customSection",
                    )))
                    .child(actions),
            )
            .child(list)
            .when_some(self.theme_error.clone(), |el, err| {
                el.child(banner(err, ui::color_error()))
            })
            // notice 与 error 互斥展示(`notice && !error`)
            .when(self.theme_error.is_none(), |el| {
                el.when_some(self.theme_notice.clone(), |el, msg| {
                    el.child(banner(msg, ui::color_success()))
                })
            })
            .into_any_element()
    }

    /// 一张皮肤卡片:缩小版的界面预览 + 名称 + hover 才出现的删除。
    /// `thumb` 是背景图的缩略图(还没生成好 / 包里没有背景图时为 `None`)。
    fn render_theme_card(
        &self,
        idx: usize,
        card: &ThemeCard,
        thumb: Option<Arc<RenderImage>>,
        active_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = active_id == Some(card.theme_id.as_str());
        let theme_id = card.theme_id.clone();
        let name = card.name.clone();

        // 背景图走**与窗口级同一个** `BackgroundArtElement`:cover 铺满 + focus
        // 百分比定位 + 包声明的压暗纱罩。只是位图换成缩略图(`prepared_image`,
        // 与原图同宽高比,落位不变)。
        //
        // ⚠️ 别退回 `img(path).size_full()`:gpui 的 `img()` 默认
        // `ObjectFit::Contain`(整图塞进框、两侧留白)且**恒定居中**,与真实界面的
        // cover + focus 是两种铺法 —— 用户实测「卡片里的图和应用后不是一回事」
        // 就是这么来的。压暗也别再写死 0.35:那只是 `backgroundDim` 的默认值。
        let mut preview = div();
        // 预览框按 **16:9** 走(壁纸的常见比例):此前是定高 96px、宽随卡片 ——
        // 276×96 是 2.9:1 的细条,cover 只截得到焦点附近一横条,与真实窗口
        // (接近 16:10)差着一个量级。`aspect_ratio` 由宽推高,卡片被面板压窄时
        // 比例照旧,不像定高那样越窄越接近正方。
        preview.style().aspect_ratio = Some(THEME_PREVIEW_ASPECT);
        let preview = preview
            .relative()
            .w_full()
            .rounded(px(4.0))
            .overflow_hidden()
            .border_1()
            .border_color(ui::border_subtle())
            .bg(card.background)
            .when_some(card.art.clone(), |el, art| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(mt_ui::background_art(art).prepared_image(thumb)),
                )
            })
            // 迷你侧栏(带包声明的 surfaceOpacity)
            .child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .w(px(48.0))
                    .rounded(px(3.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .bg(card.panel_surface)
                    // 条目数随框变高补到 5 条:16:9 的框比原先的细条高一半,
                    // 只画 3 条会在下半截空出一片,不像「项目列表」
                    .child(mini_bar(32.0, card.accent, 1.0))
                    .child(mini_bar(24.0, card.text, 0.6))
                    .child(mini_bar(28.0, card.text, 0.4))
                    .child(mini_bar(22.0, card.text, 0.32))
                    .child(mini_bar(26.0, card.text, 0.24)),
            )
            // 迷你终端区(带包声明的 terminalOpacity + 提示符)
            .child(
                div()
                    .absolute()
                    .left(px(62.0))
                    .right(px(6.0))
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .rounded(px(3.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .bg(card.terminal_surface)
                    .child(
                        div()
                            .flex()
                            .gap(px(3.0))
                            .text_size(px(10.0))
                            .child(div().text_color(card.accent).child("\u{276f}"))
                            .child(div().text_color(card.text).child("Aa 字")),
                    )
                    // 同上:多两行「输出」把 16:9 的框填满
                    .child(mini_bar(40.0, card.text, 0.5))
                    .child(mini_bar(56.0, card.text, 0.34))
                    .child(mini_bar(30.0, card.text, 0.24)),
            );

        div()
            .id(SharedString::from(format!("theme-card-{idx}")))
            .group(SharedString::from(format!("theme-card-group-{idx}")))
            // 300 是上限不是定值:面板宽被视口钳到很窄时(内容列 < 300),
            // 定值宽的卡片会横着捅出去被裁掉 —— 与「内容列不许越过面板」同一条
            .w_full()
            .max_w(px(THEME_CARD_MAX_W))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(THEME_CARD_PADDING))
            .rounded(px(6.0))
            .border_1()
            .cursor_pointer()
            .when(active, |el| {
                el.border_color(ui::accent()).bg(ui::accent_subtle())
            })
            .when(!active, |el| {
                el.border_color(ui::border_default()).bg(ui::bg_base())
            })
            .child(preview)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(if active {
                                        ui::accent()
                                    } else {
                                        ui::text_primary()
                                    })
                                    .truncate()
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .truncate()
                                    .child(theme_id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("theme-del-{idx}")))
                            .px(px(4.0))
                            .flex_none()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .cursor_pointer()
                            .hover(|el| el.text_color(ui::color_error()))
                            .child("\u{2715}")
                            .on_click(cx.listener({
                                let theme_id = theme_id.clone();
                                let name = name.clone();
                                move |this, _, window, cx| {
                                    // 卡片本身也有 on_click(选中),不拦住会连带选中
                                    cx.stop_propagation();
                                    this.confirm_delete_pack(
                                        theme_id.clone(),
                                        name.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                // **先装上主题、成功了才写配置**:装不上 `set_theme_pack` 返回 false
                // 且不落盘(内存里已回落内置)
                let ok = this.store.update(cx, |store, cx| {
                    store.set_theme_pack(Some(theme_id.clone()), window, cx)
                });
                if !ok {
                    this.theme_error = Some(tr!(
                        "settings",
                        "themes.applyFailed",
                        detail = theme_id.clone()
                    ));
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn confirm_delete_pack(
        &mut self,
        theme_id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        let store = self.store.clone();
        Confirm::new(
            t("settings", "themes.customSection"),
            tr!("settings", "themes.deleteConfirm", name = name),
        )
        .ok_text(t("settings", "common.delete"))
        .open(
            move |window, cx| {
                // **先退出该主题再删目录**:反过来的话 notify 的目录句柄还开着,
                // 被删目录在 Windows 上处于 delete-pending,紧接着重导入同名主题
                // 会撞 ERROR_ACCESS_DENIED(原版 :1886-1888 记的坑)
                let was_active =
                    store.read(cx).config().custom_theme_id.as_deref() == Some(theme_id.as_str());
                if was_active {
                    store.update(cx, |store, cx| {
                        store.set_theme_pack(None, window, cx);
                    });
                }
                let packs = crate::theme::theme_packs();
                let id = theme_id.clone();
                view.update(cx, |this: &mut SettingsView, cx| {
                    this._job = Some(cx.spawn(async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move { packs.delete(&id).map_err(|e| format!("{e:#}")) })
                            .await;
                        let _ = this.update(cx, |this: &mut SettingsView, cx| {
                            this.refresh_theme_packs(cx);
                            if let Err(err) = result {
                                this.theme_error = Some(err);
                            }
                            cx.notify();
                        });
                    }));
                });
            },
            window,
            cx,
        );
    }

    fn import_theme_dir(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("settings", "themes.importDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.run_theme_job(
                    move |packs| packs.import_dir(&dir).map_err(|e| format!("{e:#}")),
                    None,
                    cx,
                );
            });
        }));
    }

    fn import_theme_zip(&mut self, cx: &mut Context<Self>) {
        // gpui 的选择框**没有扩展名过滤**(`PathPromptOptions` 只有四个字段),
        // 选错文件由 `import_zip` 自己报错
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "themes.importZipDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(zip) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.run_theme_job(
                    move |packs| packs.import_zip(&zip).map_err(|e| format!("{e:#}")),
                    None,
                    cx,
                );
            });
        }));
    }

    // ── 外置皮肤 ──

    /// 卡片缩略图对账:不在外观页 → 全放;在外观页 → 放掉已不在卡片列表里的
    /// (删掉 / 刷新后没了的皮肤)。由 [`SettingsView::render`] 每帧开头调一次 ——
    /// 换页、刷新列表都会 notify,放的时机因此总在下一帧的渲染里,正好拿得到
    /// 当前窗口;关掉设置那一下由 `on_release` 全放。
    ///
    /// 渲染途中放是安全的:所有 render / prepaint 都先于本帧任何 paint,这些
    /// 缩略图本帧不会再被画;上一帧的场景画完本帧就被替换,不会再呈现。
    pub(super) fn release_stale_theme_thumbs(&mut self, window: &mut Window, cx: &mut App) {
        if self.theme_thumbs.is_empty() {
            return;
        }
        let live: HashSet<ThemeThumbKey> = if self.page == SettingsPage::Appearance {
            self.theme_cards
                .iter()
                .filter_map(ThemeCard::thumb_key)
                .collect()
        } else {
            HashSet::new()
        };
        let stale: Vec<ThemeThumbKey> = self
            .theme_thumbs
            .extract_if(|key| !live.contains(key))
            .collect();
        release_theme_thumbs(&stale, cx, Some(window));
    }

    pub(super) fn refresh_theme_packs(&mut self, cx: &mut Context<Self>) {
        self.theme_error = None;
        self.theme_notice = None;
        self.theme_cards = crate::theme::list_packs()
            .iter()
            .map(ThemeCard::from_listing)
            .collect();
        cx.notify();
    }

    /// 导入皮肤(目录 / zip):两条都要写盘,统一丢后台。
    fn run_theme_job(
        &mut self,
        job: impl FnOnce(mt_config::ThemePacks) -> Result<String, String> + Send + 'static,
        notice: Option<fn(&str) -> String>,
        cx: &mut Context<Self>,
    ) {
        let packs = crate::theme::theme_packs();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job(packs) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.refresh_theme_packs(cx);
                match result {
                    Ok(id) => this.theme_notice = notice.map(|f| f(&id)),
                    Err(err) => this.theme_error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    // ── font 页 ──

    pub(super) fn render_font_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let ui_size = config.ui_font_size as i32;
        let term_size = config.terminal_font_size as i32;
        let ligatures = config.terminal_ligatures;

        let store_ui = self.store.clone();
        let store_term = self.store.clone();

        page_root()
            .child(
                section("font.fontSize")
                    .child(ui::font_size_slider(
                        "ui-font-size",
                        t("settings", "font.uiFontSize"),
                        ui_size,
                        10,
                        20,
                        move |value, _window, cx| {
                            store_ui.update(cx, |store, cx| {
                                store.set_ui_font_size(value as f64, cx);
                            });
                        },
                    ))
                    .child(ui::font_size_slider(
                        "terminal-font-size",
                        t("settings", "font.terminalFontSize"),
                        term_size,
                        10,
                        24,
                        move |value, _window, cx| {
                            store_term.update(cx, |store, cx| {
                                store.set_terminal_font_size(value as f64, cx);
                            });
                        },
                    ))
                    .child(ui::hint(t("settings", "font.fontSizeFooter"))),
            )
            .child(
                section("font.font")
                    .child(font_family_input(
                        t("settings", "font.uiFont"),
                        &self.txt_ui_font,
                    ))
                    .child(font_family_input(
                        t("settings", "font.terminalFont"),
                        &self.txt_terminal_font,
                    ))
                    .child(ui::hint(format!(
                        "{}'JetBrainsMono Nerd Font', monospace{}",
                        t("settings", "font.fontFamilyFooterPrefix"),
                        t("settings", "font.fontFamilyFooterSuffix"),
                    )))
                    .child(ui::hint(t("settings", "font.nerdIconsBuiltin"))),
            )
            .child(
                section("font.ligatures")
                    // 描述是拼出来的(中间嵌一串 `== => != ->` 样例),用不了
                    // `toggle_row` 的单 key 形状,只能手写一行
                    .child(ui::setting_row(
                        t("settings", "font.ligaturesTitle"),
                        Some(
                            ui::desc_text(format!(
                                "{}== => != ->{}",
                                t("settings", "font.ligaturesDescPrefix"),
                                t("settings", "font.ligaturesDescSuffix"),
                            ))
                            .into_any_element(),
                        ),
                        false,
                        ui::toggle("font-ligatures", ligatures).on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.set_terminal_ligatures(!ligatures, cx);
                                });
                            },
                        )),
                    ))
                    .child(ui::hint(t("settings", "font.ligaturesUnavailable"))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// 回归测试(用户真机 v0.13.x GPUI 版):皮肤卡片的身份必须是**目录名**。
    ///
    /// `themes/ember-new/theme.json` 里写着 `"id": "ember-dusk"` —— 卡片此前取
    /// `def.id`,于是点「应用」落到 `packs.read("ember-dusk")` 上找不到目录,
    /// 红条报「皮肤应用失败: ember-dusk」。副标题同样要显示目录名
    /// (原版 `SettingsModal.tsx:1984` 的 `subtitle={pack.themeId}`)。
    #[test]
    fn 皮肤卡片的身份取目录名() {
        let json = r##"{
          "id": "ember-dusk", "name": "Ember Dusk", "appearance": "dark",
          "colors": {
            "background": "#120d1c", "panel": "#1c1329", "panelAlt": "#251937",
            "accent": "#ff9a62", "text": "#ede4f2", "muted": "#9a8caf", "line": "#3a2d52"
          },
          "image": "background.png"
        }"##;
        let dir = PathBuf::from("/themes/ember-new");
        let listing = ThemePackListing {
            // 列表项的 id 由 mt-config 按目录名给出
            theme_id: "ember-new".to_string(),
            def: mt_ui::theme_bridge::parse_theme_pack("ember-new", json).unwrap(),
            dir: dir.clone(),
        };

        let card = ThemeCard::from_listing(&listing);
        assert_eq!(card.theme_id, "ember-new", "应用/删除/副标题都按目录名");
        assert_ne!(card.theme_id, listing.def.id, "别再回到 theme.json 的 id");
        assert_eq!(card.name, "Ember Dusk", "显示名仍取 theme.json 的 name");
        // 图不在盘上 → 没有氛围层可画(判据与真实应用时同一处:resolve_theme_pack)
        assert!(card.art.is_none());
        // 没有背景图的包不做半透明:面板与终端底色都是实色,与真实界面一致
        assert_eq!(card.panel_surface.a, 1.0);
        assert_eq!(card.terminal_surface.a, 1.0);
    }

    /// 回归测试(用户报:「示例皮肤的图片效果和真实效果存在差异」):
    /// 卡片预览的三个半透明度必须取**包声明的 effects**,不是写死的默认值,
    /// 背景图也必须带着 `focus` 走氛围层(cover + 百分比定位),不是裸 `img()`。
    #[test]
    fn 卡片取包声明的_effects_与_focus() {
        let dir = std::env::temp_dir().join("mt-theme-card-effects");
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("background.png");
        // 只判 `is_file()`,不解码 —— 解码是渲染期 gpui 资产系统的事
        std::fs::write(&image, b"stub").unwrap();

        let json = r##"{
          "id": "ember-dusk", "name": "Ember Dusk", "appearance": "dark",
          "colors": {
            "background": "#120d1c", "panel": "#1c1329", "panelAlt": "#251937",
            "accent": "#ff9a62", "text": "#ede4f2", "muted": "#9a8caf", "line": "#3a2d52"
          },
          "image": "background.png",
          "art": { "focusX": 0.2, "focusY": 0.9 },
          "effects": { "surfaceOpacity": 0.4, "terminalOpacity": 0.25, "backgroundDim": 0.8 }
        }"##;
        let listing = ThemePackListing {
            theme_id: "ember-dusk".to_string(),
            def: mt_ui::theme_bridge::parse_theme_pack("ember-dusk", json).unwrap(),
            dir: dir.clone(),
        };

        let card = ThemeCard::from_listing(&listing);
        let art = card.art.expect("图在盘上就该有氛围层");
        assert_eq!(art.image, image);
        assert_eq!(art.focus, (0.2, 0.9), "焦点要带到预览里(默认值是 0.5/0.5)");
        assert!((art.dim.a - 0.8).abs() < 1e-6, "压暗取 backgroundDim,不是 0.35");
        assert!(
            (card.panel_surface.a - 0.4).abs() < 1e-6,
            "侧栏取 surfaceOpacity,不是 0.72"
        );
        assert!(
            (card.terminal_surface.a - 0.25).abs() < 1e-6,
            "终端区取 terminalOpacity,不是 0.6"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 缩略图目标框_是预览框最大显示尺寸的两倍() {
        // 卡片 300 宽、左右各 12 内边距 → 预览框至多 276 宽、16:9 → 155.25 高
        assert_eq!(theme_thumb_target(), (552, 311));
    }

    #[test]
    fn 缩略图尺寸_等比缩到刚好盖满目标框() {
        let target = (552, 311);
        // 16:9 的 2560×1440:两维都不小于目标(cover 铺不糊),宽高比不变
        let (w, h) = thumbnail_size((2560, 1440), target);
        assert!(w >= target.0 && h >= target.1, "{w}x{h} 盖不满目标框");
        assert!(w <= target.0 + 1 && h <= target.1 + 1, "{w}x{h} 缩得不够");
        assert!((w as f32 / h as f32 - 16.0 / 9.0).abs() < 0.01);
        // 16:10:高那一维有富余,宽刚好
        assert_eq!(thumbnail_size((2560, 1600), target), (552, 345));
        // 竖图:宽刚好,高按比例留着(cover 会裁掉上下)
        assert_eq!(thumbnail_size((1000, 4000), target), (552, 2208));
        // 4K 省下的量:3840×2160 → 553×311,内存 33 MB → 0.7 MB
        let (w, h) = thumbnail_size((3840, 2160), target);
        assert!(u64::from(w) * u64::from(h) * 4 < 700 * 1024);
    }

    #[test]
    fn 缩略图尺寸_只缩不放() {
        let target = (552, 311);
        assert_eq!(thumbnail_size((400, 300), target), (400, 300));
        // 一维已经比目标小:放大那一维才盖得满,宁可糊一点也不放大
        assert_eq!(thumbnail_size((10000, 10), target), (10000, 10));
        assert_eq!(thumbnail_size((0, 0), target), (0, 0));
        assert_eq!(thumbnail_size((0, 900), target), (0, 900));
    }

    #[test]
    fn 缩略图_缩完尺寸对_颜色与通道顺序不变() {
        // 64×36 纯色 BGRA(蓝 10 绿 20 红 30),目标 16×9
        let (w, h) = (64u32, 36u32);
        let bytes: Vec<u8> = [10u8, 20, 30, 255].repeat((w * h) as usize);
        let frame = image::Frame::new(image::RgbaImage::from_raw(w, h, bytes).unwrap());
        let full = RenderImage::new(vec![frame]);

        let thumb = downscale(&full, (16, 9)).expect("能缩");
        let size = thumb.size(0);
        assert_eq!((size.width.0, size.height.0), (16, 9));
        // 纯色是均值的不动点;通道不解释,BGRA 进 BGRA 出
        for px in thumb.as_bytes(0).unwrap().as_chunks::<4>().0 {
            assert_eq!(px, &[10, 20, 30, 255]);
        }

        // 比目标还小的图原样尺寸
        let small = downscale(&full, (640, 360)).expect("能缩");
        assert_eq!((small.size(0).width.0, small.size(0).height.0), (64, 36));
    }

    #[test]
    fn 缩略图_key_只在有背景图时才有() {
        let dir = PathBuf::from("/themes/plain");
        let json = r##"{
          "id": "plain", "name": "Plain", "appearance": "dark",
          "colors": {
            "background": "#120d1c", "panel": "#1c1329", "panelAlt": "#251937",
            "accent": "#ff9a62", "text": "#ede4f2", "muted": "#9a8caf", "line": "#3a2d52"
          }
        }"##;
        let listing = ThemePackListing {
            theme_id: "plain".to_string(),
            def: mt_ui::theme_bridge::parse_theme_pack("plain", json).unwrap(),
            dir,
        };
        assert!(ThemeCard::from_listing(&listing).thumb_key().is_none());
    }
}
