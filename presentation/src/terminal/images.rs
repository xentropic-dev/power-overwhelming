//! Kitty graphics images: the pixels the terminal stores, uploaded once per image
//! generation and drawn in their layer.
//!
//! Adapted from herdr-app `src/vt.rs` (`image_bgra`) and `src/terminal.rs` (`paint_grid`)
//! (https://github.com/herdrdev/herdr-app), licensed under Apache-2.0.

use ghostty_vt::{
    Terminal,
    kitty::graphics::ImageFormat,
    render::{ImageLayer, ImagePlacement},
};
use gpui::{Bounds, Pixels, Point, RenderImage, Size, point, px, size};
use image::{Frame, RgbaImage};
use std::{collections::HashMap, sync::Arc};

/// One cached upload: the image and its pixels' generation. Every crop of an image
/// shares it, so the uploads stay within Ghostty's own storage limit.
type Key = (u32, u64);

/// An image ready to paint: the whole upload at `bounds`, shown only inside `clip`, the
/// placement's rectangle, so a crop costs no upload of its own.
pub(super) struct Placed {
    pub(super) layer: ImageLayer,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) clip: Bounds<Pixels>,
    pub(super) image: Arc<RenderImage>,
}

/// Uploads keyed by image generation, so a retransmitted image replaces its sprite.
#[derive(Default)]
pub(super) struct ImageCache {
    images: HashMap<Key, Arc<RenderImage>>,
}

impl ImageCache {
    /// Uploads what `placements` show and isn't cached yet, from the terminal as it
    /// stands. Synchronized output calls this when its hold begins, so the held frame keeps
    /// pixels that a deletion or retransmission later in the same write would take away.
    pub(super) fn prepare(&mut self, terminal: &Terminal, placements: &[ImagePlacement]) {
        for p in placements {
            let key = (p.image_id, p.image_generation);
            if !self.images.contains_key(&key)
                && let Some(image) = upload(terminal, key)
            {
                self.images.insert(key, image);
            }
        }
    }

    /// The placements to paint, origin at the grid's top-left, plus the uploads no
    /// placement uses anymore, which the window should release.
    ///
    /// `cell` is the logical cell size; Ghostty's pixel values use the device cell given
    /// to `resize`, so they scale by the ratio.
    pub(super) fn place(
        &mut self,
        terminal: &Terminal,
        placements: &[ImagePlacement],
        grid: Point<Pixels>,
        cell: Size<Pixels>,
    ) -> (Vec<Placed>, Vec<Arc<RenderImage>>) {
        let (sx, sy) = scale(terminal, cell);
        let mut placed = Vec::with_capacity(placements.len());
        let mut used = HashMap::with_capacity(placements.len());
        for p in placements {
            let key = (p.image_id, p.image_generation);
            let image = match self.images.remove(&key).or_else(|| used.get(&key).cloned()) {
                Some(image) => image,
                None => match upload(terminal, key) {
                    Some(image) => image,
                    None => continue,
                },
            };
            used.insert(key, image.clone());
            let [x, y, width, height] = geometry(p, cell, sx, sy);
            let clip = Bounds::new(grid + point(px(x), px(y)), size(px(width), px(height)));
            let Some(bounds) = uncropped(p, &image, clip) else {
                continue;
            };
            placed.push(Placed {
                layer: p.layer,
                bounds,
                clip,
                image,
            });
        }
        let evicted = std::mem::replace(&mut self.images, used)
            .into_values()
            .collect();
        (placed, evicted)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.images.len()
    }

    /// Every upload, for the window to release when the view goes away.
    pub(super) fn drain(&mut self) -> Vec<Arc<RenderImage>> {
        self.images.drain().map(|(_, image)| image).collect()
    }
}

/// Where the whole image goes so that its source rectangle lands on `clip`.
fn uncropped(
    p: &ImagePlacement,
    image: &RenderImage,
    clip: Bounds<Pixels>,
) -> Option<Bounds<Pixels>> {
    let full = image.size(0);
    let (iw, ih) = (full.width.0 as u32, full.height.0 as u32);
    let sw = p.source_width.min(iw.saturating_sub(p.source_x));
    let sh = p.source_height.min(ih.saturating_sub(p.source_y));
    if sw == 0 || sh == 0 {
        return None;
    }
    let kx = f32::from(clip.size.width) / sw as f32;
    let ky = f32::from(clip.size.height) / sh as f32;
    Some(Bounds::new(
        clip.origin - point(px(p.source_x as f32 * kx), px(p.source_y as f32 * ky)),
        size(px(iw as f32 * kx), px(ih as f32 * ky)),
    ))
}

/// Logical pixels per device pixel of the terminal's cell, horizontally and vertically.
fn scale(terminal: &Terminal, cell: Size<Pixels>) -> (f32, f32) {
    let per = |pixels: Result<u32, _>, count: Result<u16, _>, logical: Pixels| {
        let device = pixels.unwrap_or(0) as f32 / f32::from(count.unwrap_or(1).max(1));
        if device > 0. {
            f32::from(logical) / device
        } else {
            1.
        }
    };
    (
        per(terminal.width_px(), terminal.cols(), cell.width),
        per(terminal.height_px(), terminal.rows(), cell.height),
    )
}

/// The placement's rectangle from the grid origin, in logical pixels: x, y, width, height.
fn geometry(p: &ImagePlacement, cell: Size<Pixels>, sx: f32, sy: f32) -> [f32; 4] {
    [
        p.viewport_col as f32 * f32::from(cell.width) + p.cell_offset_x as f32 * sx,
        p.viewport_row as f32 * f32::from(cell.height) + p.cell_offset_y as f32 * sy,
        p.pixel_width as f32 * sx,
        p.pixel_height as f32 * sy,
    ]
}

/// The stored image, whole, if it still holds the pixels of `generation`.
fn upload(terminal: &Terminal, (id, generation): Key) -> Option<Arc<RenderImage>> {
    let graphics = terminal.kitty_graphics().ok()?;
    let image = graphics.image(id)?;
    // A retransmission under the same id must not paint under the old generation.
    if image.generation().ok()? != generation {
        return None;
    }
    let (width, height) = (image.width().ok()?, image.height().ok()?);
    let (w, h, bgra) = crop_bgra(
        image.data().ok()?,
        image.format().ok()?,
        width,
        height,
        [0, 0, width, height],
    )?;
    let frame = Frame::new(RgbaImage::from_raw(w, h, bgra)?);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

/// Converts the stored pixels to BGRA (what GPUI uploads), cropped to `source`.
fn crop_bgra(
    pixels: &[u8],
    format: ImageFormat,
    width: u32,
    height: u32,
    [sx, sy, sw, sh]: [u32; 4],
) -> Option<(u32, u32, Vec<u8>)> {
    let bpp = match format {
        ImageFormat::Rgba => 4,
        ImageFormat::Rgb => 3,
        ImageFormat::GrayAlpha => 2,
        ImageFormat::Gray => 1,
        // Ghostty stores PNG decoded; anything else is unexpected.
        _ => return None,
    };
    let (sw, sh) = (
        sw.min(width.saturating_sub(sx)),
        sh.min(height.saturating_sub(sy)),
    );
    if sw == 0 || sh == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(sw as usize * sh as usize * 4);
    for y in sy..sy + sh {
        let start = (y as usize * width as usize + sx as usize) * bpp;
        let row = pixels.get(start..start + sw as usize * bpp)?;
        for p in row.chunks_exact(bpp) {
            let (r, g, b, a) = match bpp {
                4 => (p[0], p[1], p[2], p[3]),
                3 => (p[0], p[1], p[2], 255),
                2 => (p[0], p[0], p[0], p[1]),
                _ => (p[0], p[0], p[0], 255),
            };
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some((sw, sh, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::frame::Frame as TerminalFrame;
    use ghostty_vt::{
        render::{KittyPlacementKind, RenderState},
        terminal::Options,
    };

    #[test]
    fn crops_and_converts_every_stored_format_to_bgra() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(
            crop_bgra(&rgba, ImageFormat::Rgba, 2, 2, [1, 0, 1, 2]),
            Some((1, 2, vec![7, 6, 5, 8, 15, 14, 13, 16]))
        );
        assert_eq!(
            crop_bgra(&[1, 2, 3], ImageFormat::Rgb, 1, 1, [0, 0, 1, 1]),
            Some((1, 1, vec![3, 2, 1, 255]))
        );
        assert_eq!(
            crop_bgra(&[9, 7], ImageFormat::GrayAlpha, 1, 1, [0, 0, 1, 1]),
            Some((1, 1, vec![9, 9, 9, 7]))
        );
        assert_eq!(
            crop_bgra(&[9], ImageFormat::Gray, 1, 1, [0, 0, 4, 4]),
            Some((1, 1, vec![9, 9, 9, 255]))
        );
        assert_eq!(crop_bgra(&[9], ImageFormat::Gray, 1, 1, [1, 0, 1, 1]), None);
        assert_eq!(crop_bgra(&[], ImageFormat::Rgba, 1, 1, [0, 0, 1, 1]), None);
    }

    fn terminal() -> Terminal {
        let mut terminal = Terminal::new(Options {
            cols: 10,
            rows: 5,
            ..Options::default()
        })
        .unwrap();
        terminal.resize(10, 5, 8, 16).unwrap();
        infrastructure::kitty::enable(&mut terminal).unwrap();
        terminal
    }

    /// Transmits a 16×32 RGBA image, two cells by two at 8×16.
    fn transmit(terminal: &mut Terminal, id: u32, byte: u8) {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(vec![byte; 16 * 32 * 4]);
        terminal.vt_write(format!("\x1b_Ga=t,f=32,s=16,v=32,i={id},q=2;{data}\x1b\\").as_bytes());
    }

    fn images(terminal: &Terminal, renderer: &mut RenderState) -> Vec<ImagePlacement> {
        let frame = TerminalFrame::default()
            .update(renderer, terminal)
            .unwrap()
            .unwrap();
        frame.images.to_vec()
    }

    #[test]
    fn direct_placements_sort_by_layer_and_z_and_scale_to_logical_cells() {
        let mut terminal = terminal();
        transmit(&mut terminal, 1, 200);
        transmit(&mut terminal, 2, 100);
        // Below the text at the cursor, then above it one row down.
        terminal.vt_write(b"\x1b_Ga=p,i=2,z=5,C=1,q=2\x1b\\");
        terminal.vt_write(b"\x1b[2;3H\x1b_Ga=p,i=1,z=-1,C=1,q=2\x1b\\");
        let placements = images(&terminal, &mut RenderState::new().unwrap());
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].image_id, 1);
        assert_eq!(placements[0].layer, ImageLayer::BelowText);
        assert_eq!(placements[1].layer, ImageLayer::AboveText);
        assert_eq!(placements[0].kind, KittyPlacementKind::Fixed);
        assert_eq!(
            (placements[0].viewport_col, placements[0].viewport_row),
            (2, 1)
        );

        // A 2× display: logical cells of 4×8 for device cells of 8×16.
        let mut cache = ImageCache::default();
        let cell = size(px(4.), px(8.));
        let (placed, evicted) = cache.place(&terminal, &placements, point(px(10.), px(20.)), cell);
        assert!(evicted.is_empty());
        assert_eq!(
            placed[0].bounds,
            Bounds::new(point(px(18.), px(28.)), size(px(8.), px(16.)))
        );
        let first = placed[0].image.clone();
        // Unchanged placements reuse their upload.
        let (again, evicted) = cache.place(&terminal, &placements, point(px(0.), px(0.)), cell);
        assert!(evicted.is_empty());
        assert!(Arc::ptr_eq(&again[0].image, &first));
        // Retransmitting changes the generation, so the old upload goes.
        transmit(&mut terminal, 1, 50);
        terminal.vt_write(b"\x1b[2;3H\x1b_Ga=p,i=1,z=-1,C=1,q=2\x1b\\");
        let placements = images(&terminal, &mut RenderState::new().unwrap());
        let (placed, evicted) = cache.place(&terminal, &placements, point(px(0.), px(0.)), cell);
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &first));
        assert!(placed.iter().all(|p| !Arc::ptr_eq(&p.image, &first)));
    }

    #[test]
    fn crops_of_one_image_share_a_single_upload() {
        let mut terminal = terminal();
        transmit(&mut terminal, 5, 9);
        // Many distinct, nearly full-size crops, each in its own cell.
        for (n, x) in (0..8).enumerate() {
            terminal.vt_write(
                format!(
                    "\x1b[{};1H\x1b_Ga=p,i=5,p={},x={x},y={x},w={},h={},c=1,r=1,q=2\x1b\\",
                    n % 5 + 1,
                    n + 1,
                    16 - x,
                    32 - x
                )
                .as_bytes(),
            );
        }
        let placements = images(&terminal, &mut RenderState::new().unwrap());
        assert_eq!(placements.len(), 8);
        let mut cache = ImageCache::default();
        let cell = size(px(8.), px(16.));
        let (placed, _) = cache.place(&terminal, &placements, point(px(0.), px(0.)), cell);
        assert_eq!(placed.len(), 8);
        assert!(
            placed
                .iter()
                .all(|p| Arc::ptr_eq(&p.image, &placed[0].image))
        );
        // One upload of the stored image's size, however many crops show it.
        assert_eq!(cache.images.len(), 1);
        let bytes: usize = cache
            .images
            .values()
            .map(|image| image.as_bytes(0).unwrap().len())
            .sum();
        assert_eq!(bytes, 16 * 32 * 4);
        // The crop is a clip: x=3,y=3 shifts the whole image up and left of its cell.
        let crop = placed
            .iter()
            .zip(&placements)
            .find(|(_, p)| p.source_x == 3)
            .unwrap()
            .0;
        assert_eq!(crop.clip.size, size(px(8.), px(16.)));
        assert!(crop.bounds.origin.x < crop.clip.origin.x);
        assert!(crop.bounds.contains(&crop.clip.origin));
        assert_eq!(cache.drain().len(), 1);
        assert!(cache.images.is_empty());
    }

    /// The held frame and cache after one write that places a new image, starts
    /// synchronized output and then does `after` to it.
    fn held_after(after: &[u8]) -> (Terminal, Vec<Placed>) {
        use std::{cell::RefCell, rc::Rc};
        let mut terminal = terminal();
        let captured = crate::terminal::hold::Captured::default();
        let cache = Rc::new(RefCell::new(ImageCache::default()));
        crate::terminal::hold::install(
            &mut terminal,
            captured.clone(),
            Rc::new(std::cell::Cell::new(1.)),
            cache.clone(),
        );
        transmit(&mut terminal, 6, 200);
        let mut chunk = b"\x1b_Ga=p,i=6,q=2\x1b\\\x1b[?2026h".to_vec();
        chunk.extend_from_slice(after);
        terminal.vt_write(&chunk);
        let (_, frame) = captured.borrow_mut().take().unwrap();
        assert_eq!(frame.images.len(), 1);
        let (placed, _) = cache.borrow_mut().place(
            &terminal,
            &frame.images,
            point(px(0.), px(0.)),
            size(px(8.), px(16.)),
        );
        (terminal, placed)
    }

    fn first_pixel(placed: &[Placed]) -> u8 {
        placed[0].image.as_bytes(0).unwrap()[0]
    }

    #[test]
    fn a_hold_keeps_an_image_deleted_later_in_the_same_write() {
        let (_, placed) = held_after(b"\x1b_Ga=d,d=I,i=6,q=2\x1b\\AFTER HOLD");
        assert_eq!(placed.len(), 1);
        assert_eq!(first_pixel(&placed), 200);
    }

    #[test]
    fn a_hold_keeps_the_old_pixels_of_an_image_retransmitted_in_the_same_write() {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 16 * 32 * 4]);
        let (_, placed) =
            held_after(format!("\x1b_Ga=t,f=32,s=16,v=32,i=6,q=2;{data}\x1b\\").as_bytes());
        assert_eq!(placed.len(), 1);
        assert_eq!(first_pixel(&placed), 200);
    }

    #[test]
    fn a_stale_generation_never_uploads_the_replacement() {
        let mut terminal = terminal();
        transmit(&mut terminal, 8, 1);
        terminal.vt_write(b"\x1b_Ga=p,i=8,q=2\x1b\\");
        let old = images(&terminal, &mut RenderState::new().unwrap());
        transmit(&mut terminal, 8, 2);
        let (placed, _) = ImageCache::default().place(
            &terminal,
            &old,
            point(px(0.), px(0.)),
            size(px(8.), px(16.)),
        );
        assert!(placed.is_empty());
    }

    #[test]
    fn live_reattach_snapshot_keeps_images() {
        // `Command::Attach` sends `snapshot::encode` of the daemon's terminal; the GUI
        // decodes it into its mirror. Only a daemon restart, which replays the scrollback
        // text, loses images.
        let mut daemon = terminal();
        transmit(&mut daemon, 9, 42);
        daemon.vt_write(b"\x1b[2;2H\x1b_Ga=p,i=9,q=2\x1b\\");
        let bytes = ghostty_vt::snapshot::encode(&daemon).unwrap();
        let mut mirror = ghostty_vt::snapshot::Decoder::from_slice(&bytes)
            .decode()
            .unwrap();
        infrastructure::kitty::enable(&mut mirror).unwrap();
        let placements = images(&mirror, &mut RenderState::new().unwrap());
        assert_eq!(placements.len(), 1);
        assert_eq!(
            (placements[0].viewport_col, placements[0].viewport_row),
            (1, 1)
        );
        let (placed, _) = ImageCache::default().place(
            &mirror,
            &placements,
            point(px(0.), px(0.)),
            size(px(8.), px(16.)),
        );
        assert_eq!(first_pixel(&placed), 42);
    }

    #[test]
    fn unicode_placeholders_place_their_image_where_the_cells_are() {
        let mut terminal = terminal();
        transmit(&mut terminal, 3, 255);
        // A virtual placement two cells by two, then the placeholder cells for it: the
        // image id rides in the foreground color, rows and columns in diacritics.
        terminal.vt_write(b"\x1b_Ga=p,U=1,i=3,c=2,r=2,q=2\x1b\\");
        terminal.vt_write(
            "\x1b[2;4H\x1b[38;5;3m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\r\n\
             \x1b[4C\u{10EEEE}\u{030D}\u{0305}\u{10EEEE}\u{030D}\u{030D}\x1b[39m"
                .as_bytes(),
        );
        let mut renderer = RenderState::new().unwrap();
        let placements = images(&terminal, &mut renderer);
        assert!(!placements.is_empty());
        assert!(placements.iter().all(|p| p.image_id == 3));
        assert!(
            placements
                .iter()
                .all(|p| p.kind == KittyPlacementKind::Virtual)
        );
        let top = placements.iter().map(|p| p.viewport_row).min().unwrap();
        let left = placements.iter().map(|p| p.viewport_col).min().unwrap();
        assert_eq!((left, top), (3, 1));
        let covered: u32 = placements.iter().map(|p| p.pixel_height).sum();
        assert_eq!(covered, 32);
    }

    #[test]
    fn deleting_an_image_clears_its_placements_from_the_next_frame() {
        let mut terminal = terminal();
        transmit(&mut terminal, 4, 1);
        terminal.vt_write(b"\x1b_Ga=p,i=4,q=2\x1b\\");
        let mut renderer = RenderState::new().unwrap();
        let shown = TerminalFrame::default()
            .update(&mut renderer, &terminal)
            .unwrap()
            .unwrap();
        assert_eq!(shown.images.len(), 1);
        terminal.vt_write(b"\x1b_Ga=d,d=I,i=4,q=2\x1b\\");
        let cleared = shown.update(&mut renderer, &terminal).unwrap().unwrap();
        assert!(cleared.images.is_empty());
    }
}
