//! 终端里的图片(Kitty 图形协议):把 [`mt_terminal::kitty`] 给的摆放画出来,并管位图在
//! gpui 这边的生命周期。默认关闭,关着时这里一行都不跑。
//!
//! # 纹理什么时候放
//!
//! `RenderImage` 一旦画过,图集里就有一份纹理,只有 `drop_image` 摘得掉。这里按位图的
//! uid 缓存,摘的时机两个:
//!
//! - **画的时候**([`paint_images`]):位图已经不在账本里(被替换、被删、被淘汰),或者
//!   屏外的总量超了 [`OFFSCREEN_BUDGET`]。这是绘制之内,不踩「不重画就再呈现」那颗雷
//!   (见 mt-app 的 `file_viewer::images::evict_viewer_image`);
//! - **视图释放时**([`release_all`]):绘制之外,所以要把窗口弄脏。
//!
//! 缓存挂在 `TerminalView` 上而不是元素状态上:切走的页签不进元素树,元素状态随之丢了,
//! 手上的 `Arc<RenderImage>` 一丢纹理就再也摘不掉。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Bounds, Corners, Pixels, Point, RenderImage, Size, Window, point, size};
use mt_terminal::kitty::{Kitty, KittyBitmap, VisibleImage};

/// 暂时没画的位图最多留多少字节在 gpui 这边。它们还在账本里,再画到时重新上传就是。
const OFFSCREEN_BUDGET: u64 = 64 * 1024 * 1024;

struct CachedImage {
    image: Arc<RenderImage>,
    bytes: u64,
    /// 最后一次画到是第几次 paint。
    last_painted: u64,
}

/// 一个终端的 `RenderImage` 缓存。见模块注释。
#[derive(Default)]
pub struct TerminalImageCache {
    entries: HashMap<u64, CachedImage>,
    paints: u64,
    /// 上次核对账本时的代数。
    epoch: Option<u64>,
}

impl TerminalImageCache {
    /// 这张位图的 `RenderImage`,没有就建一个。
    fn image_for(&mut self, bitmap: &KittyBitmap) -> Option<Arc<RenderImage>> {
        let paints = self.paints;
        if let Some(cached) = self.entries.get_mut(&bitmap.uid()) {
            cached.last_painted = paints;
            return Some(cached.image.clone());
        }
        let (width, height) = bitmap.size();
        // RenderImage 的帧是 BGRA,这里借 `Rgba<u8>` 当四通道容器(与 mt-app 的
        // `frost.rs` 同一手法),字节原样放进去
        let buffer = image::RgbaImage::from_raw(width, height, bitmap.bgra().to_vec())?;
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        self.entries.insert(
            bitmap.uid(),
            CachedImage {
                image: image.clone(),
                bytes: bitmap.bgra().len() as u64,
                last_painted: paints,
            },
        );
        Some(image)
    }

    /// 摘出账本里已经没有的。
    fn take_dead(&mut self, live: &HashSet<u64>) -> Vec<Arc<RenderImage>> {
        let dead: Vec<u64> = self
            .entries
            .keys()
            .filter(|uid| !live.contains(uid))
            .copied()
            .collect();
        dead.into_iter()
            .filter_map(|uid| self.entries.remove(&uid))
            .map(|cached| cached.image)
            .collect()
    }

    /// 屏外的总量超了就摘,最久没画的先走。这一次 paint 画到的不动。
    fn take_over_budget(&mut self) -> Vec<Arc<RenderImage>> {
        let paints = self.paints;
        let offscreen: u64 = self
            .entries
            .values()
            .filter(|cached| cached.last_painted != paints)
            .map(|cached| cached.bytes)
            .sum();
        if offscreen <= OFFSCREEN_BUDGET {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, u64, u64)> = self
            .entries
            .iter()
            .filter(|(_, cached)| cached.last_painted != paints)
            .map(|(uid, cached)| (cached.last_painted, *uid, cached.bytes))
            .collect();
        candidates.sort_unstable();
        let mut over = offscreen - OFFSCREEN_BUDGET;
        let mut taken = Vec::new();
        for (_, uid, bytes) in candidates {
            if over == 0 {
                break;
            }
            over = over.saturating_sub(bytes);
            if let Some(cached) = self.entries.remove(&uid) {
                taken.push(cached.image);
            }
        }
        taken
    }
}

/// 图片这一层的坐标系。
pub(super) struct ImageLayer {
    /// 网格原点(元素左上角)。
    pub origin: Point<Pixels>,
    pub cell: Size<Pixels>,
    pub display_offset: usize,
    /// 画到哪儿为止:元素范围。部分滚出视口、超出右边的都在这里裁掉。
    pub clip: Bounds<Pixels>,
}

/// 摆放落在屏幕上的矩形,与「整张原图铺开」的矩形。
///
/// 协议里的裁切是「原图的这一块铺满 c×r」,所以按比例反推整张图的矩形:
/// `Window::paint_image` 画的是两者的交集,贴图坐标由它按比例算。
fn image_rects(image: &VisibleImage, layer: &ImageLayer) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let x = layer.origin.x + layer.cell.width * image.column as f32;
    let row = image.line + layer.display_offset as i32;
    let y = layer.origin.y + layer.cell.height * row as f32;
    let dest = Bounds::new(
        point(x, y),
        size(
            layer.cell.width * image.columns as f32,
            layer.cell.height * image.rows as f32,
        ),
    );
    let (source_w, source_h) = image.bitmap.source_size();
    let scale_x = dest.size.width / image.source.width as f32;
    let scale_y = dest.size.height / image.source.height as f32;
    let full = Bounds::new(
        point(
            x - scale_x * image.source.x as f32,
            y - scale_y * image.source.y as f32,
        ),
        size(scale_x * source_w as f32, scale_y * source_h as f32),
    );
    (dest, full)
}

/// 画这一帧的图,顺手放掉不再需要的纹理。
pub(super) fn paint_images(
    kitty: &Kitty,
    images: &[VisibleImage],
    layer: &ImageLayer,
    cache: &RefCell<TerminalImageCache>,
    window: &mut Window,
    cx: &mut App,
) {
    let mut cache = cache.borrow_mut();
    if images.is_empty() && cache.entries.is_empty() {
        return;
    }
    cache.paints += 1;
    for image in images {
        let Some(render) = cache.image_for(&image.bitmap) else {
            continue;
        };
        let (dest, full) = image_rects(image, layer);
        let visible = dest.intersect(&layer.clip);
        _ = window.paint_image(visible, full, Corners::default(), render, 0, false);
    }

    let epoch = kitty.epoch();
    let mut stale = Vec::new();
    if cache.epoch != Some(epoch) {
        cache.epoch = Some(epoch);
        stale = cache.take_dead(&kitty.live_bitmap_uids());
    }
    stale.extend(cache.take_over_budget());
    drop(cache);
    // `window` 此刻不在 `App.windows` 里,要递进去(`App::drop_image` 的口径)
    for image in stale {
        cx.drop_image(image, Some(window));
    }
}

/// 视图释放:纹理全部摘掉。绘制之外摘,要把窗口弄脏(见模块注释)。
pub(super) fn release_all(cache: &RefCell<TerminalImageCache>, cx: &mut App) {
    let entries = std::mem::take(&mut cache.borrow_mut().entries);
    if entries.is_empty() {
        return;
    }
    for cached in entries.into_values() {
        cx.drop_image(cached.image, None);
    }
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;
    use mt_terminal::kitty::SourceRect;
    use mt_terminal::{TermSize, TerminalEmulator};

    /// 经公开接口造一张位图:走一遍真的 PNG 传输。
    fn bitmap(width: u32, height: u32) -> Arc<KittyBitmap> {
        use base64::Engine as _;
        let e = TerminalEmulator::new(TermSize::new(20, 5)).with_kitty_graphics(true);
        let pixels = vec![128u8; (width * height * 4) as usize];
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            &pixels,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("编码");
        let payload = base64::engine::general_purpose::STANDARD.encode(&png);
        e.advance(format!("\x1b_Ga=T,f=100,q=2,c=2,r=2,i=1;{payload}\x1b\\").as_bytes());
        e.with_term(|term| e.kitty().unwrap().visible_images(term))
            .remove(0)
            .bitmap
    }

    fn visible(bitmap: Arc<KittyBitmap>, source: SourceRect) -> VisibleImage {
        VisibleImage {
            bitmap,
            line: 2,
            column: 3,
            columns: 4,
            rows: 2,
            source,
        }
    }

    fn layer() -> ImageLayer {
        ImageLayer {
            origin: point(px(100.0), px(50.0)),
            cell: size(px(10.0), px(20.0)),
            display_offset: 0,
            clip: Bounds::new(point(px(100.0), px(50.0)), size(px(200.0), px(100.0))),
        }
    }

    fn whole(width: u32, height: u32) -> SourceRect {
        SourceRect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// 没裁切:按格子定位,整张图的矩形就是摆放矩形。
    #[test]
    fn 按格子定位() {
        let image = visible(bitmap(8, 8), whole(8, 8));
        let (dest, full) = image_rects(&image, &layer());
        assert_eq!(dest.origin, point(px(130.0), px(90.0)));
        assert_eq!(dest.size, size(px(40.0), px(40.0)));
        assert_eq!(full, dest);
    }

    /// 回看偏移:用户往回翻 3 行,同一条 grid 行往下挪 3 行。
    #[test]
    fn 回看偏移换算到屏幕行() {
        let mut image = visible(bitmap(8, 8), whole(8, 8));
        image.line = -2;
        let layer = ImageLayer {
            display_offset: 3,
            ..layer()
        };
        let (dest, _) = image_rects(&image, &layer);
        assert_eq!(dest.origin.y, px(50.0 + 20.0));
    }

    /// 裁切:原图的下半截铺满摆放矩形,整张图的矩形往上探出去一个摆放高度。
    #[test]
    fn 裁切时反推整张图的矩形() {
        let image = visible(
            bitmap(8, 8),
            SourceRect {
                x: 0,
                y: 4,
                width: 8,
                height: 4,
            },
        );
        let (dest, full) = image_rects(&image, &layer());
        assert_eq!(dest.size, size(px(40.0), px(40.0)));
        assert_eq!(full.origin, point(px(130.0), px(90.0 - 40.0)));
        assert_eq!(full.size, size(px(40.0), px(80.0)));
    }

    /// 超出元素右边、下边,以及从视口上方探下来:交集就是要画的部分。
    #[test]
    fn 裁到元素范围() {
        let mut image = visible(bitmap(8, 8), whole(8, 8));
        image.column = 18;
        let layer = layer();
        let (dest, _) = image_rects(&image, &layer);
        let shown = dest.intersect(&layer.clip);
        assert_eq!(shown.size.width, px(20.0), "只露出 2 列");

        image.column = 0;
        image.line = -1;
        let (dest, _) = image_rects(&image, &layer);
        let shown = dest.intersect(&layer.clip);
        assert_eq!(shown.origin.y, px(50.0));
        assert_eq!(shown.size.height, px(20.0), "只露出下面 1 行");

        image.line = 4;
        let (dest, _) = image_rects(&image, &layer);
        let shown = dest.intersect(&layer.clip);
        assert_eq!(shown.size.height, px(20.0), "只露出上面 1 行");
    }

    /// 同一张位图只建一次 `RenderImage`,字节序原样。
    #[test]
    fn 同一张位图只建一次() {
        let mut cache = TerminalImageCache::default();
        let first = bitmap(4, 4);
        let a = cache.image_for(&first).expect("建得出来");
        let b = cache.image_for(&first).expect("建得出来");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.as_bytes(0), Some(first.bgra()));
        assert_eq!(a.size(0).width.0, 4);

        let other = bitmap(4, 4);
        let c = cache.image_for(&other).expect("建得出来");
        assert!(!Arc::ptr_eq(&a, &c), "uid 不同就是另一张");
    }

    #[test]
    fn 账本里没有的摘出来() {
        let mut cache = TerminalImageCache::default();
        let keep = bitmap(2, 2);
        let gone = bitmap(2, 2);
        cache.image_for(&keep);
        cache.image_for(&gone);
        let dead = cache.take_dead(&HashSet::from([keep.uid()]));
        assert_eq!(dead.len(), 1);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key(&keep.uid()));
    }

    /// 屏外超预算:最久没画的先走,这一次 paint 画到的不动。
    #[test]
    fn 屏外超预算时摘最旧的() {
        let mut cache = TerminalImageCache::default();
        let (a, b, c) = (bitmap(2, 2), bitmap(2, 2), bitmap(2, 2));
        cache.paints = 1;
        cache.image_for(&a);
        cache.paints = 2;
        cache.image_for(&b);
        cache.paints = 3;
        cache.image_for(&c);
        assert!(cache.take_over_budget().is_empty(), "没超");

        // 把两张屏外的记成超预算的大小
        cache.entries.get_mut(&a.uid()).unwrap().bytes = OFFSCREEN_BUDGET;
        cache.entries.get_mut(&b.uid()).unwrap().bytes = 16;
        let taken = cache.take_over_budget();
        assert_eq!(taken.len(), 1);
        assert!(!cache.entries.contains_key(&a.uid()), "最久没画的走");
        assert!(cache.entries.contains_key(&b.uid()));
        assert!(cache.entries.contains_key(&c.uid()), "这一次画到的不动");
    }
}
