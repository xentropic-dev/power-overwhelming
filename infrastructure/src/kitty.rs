//! Kitty graphics setup shared by the daemon's terminal and the GUI's mirror.
//!
//! PNG decoding and the 128 MB storage limit follow herdr-app `src/vt.rs`
//! (https://github.com/herdrdev/herdr-app), licensed under Apache-2.0.

use ghostty_vt::{
    Terminal,
    alloc::{Allocator, Bytes},
    kitty::graphics::{DecodePng, DecodedImage, set_png_decoder},
};
use std::cell::Cell;

/// The most image bytes one screen keeps before evicting the oldest images.
pub const IMAGE_STORAGE_LIMIT: u64 = 128 << 20;

/// Turns Kitty graphics on with Power's storage limit and a PNG decoder.
///
/// libghostty-vt leaves PNG decoding to the embedder, and the Rust binding keeps the
/// decoder per thread, so this installs one on the calling thread the first time.
pub fn enable(terminal: &mut Terminal) -> ghostty_vt::error::Result<()> {
    install_png_decoder()?;
    terminal.set_kitty_image_storage_limit(IMAGE_STORAGE_LIMIT)?;
    Ok(())
}

fn install_png_decoder() -> ghostty_vt::error::Result<()> {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.get() {
        return Ok(());
    }
    set_png_decoder(Some(Box::new(Png)))?;
    INSTALLED.set(true);
    Ok(())
}

/// Decodes PNG to 8-bit RGBA, the layout Ghostty stores.
struct Png;

impl DecodePng for Png {
    fn decode_png(&mut self, alloc: &Allocator, data: &[u8]) -> Option<DecodedImage> {
        let (width, height, rgba) = decode_rgba(data)?;
        let mut bytes = Bytes::new_with_alloc(alloc, rgba.len()).ok()?;
        bytes.copy_from_slice(&rgba);
        Some(DecodedImage {
            width,
            height,
            data: bytes,
        })
    }
}

fn decode_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let image = image::load_from_memory_with_format(data, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Some((width, height, image.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ghostty_vt::terminal::{Event, Options};

    /// A 2×1 PNG: one red pixel and one half-transparent blue one.
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        let image = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        image
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn png_decodes_to_rgba() {
        let (w, h, rgba) = decode_rgba(&png()).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, [255, 0, 0, 255, 0, 0, 255, 128]);
        assert!(decode_rgba(b"not a png").is_none());
    }

    #[test]
    fn enabled_terminal_stores_png_images_and_answers_queries() {
        let mut terminal = Terminal::new(Options {
            cols: 10,
            rows: 4,
            ..Options::default()
        })
        .unwrap();
        terminal.resize(10, 4, 8, 16).unwrap();
        enable(&mut terminal).unwrap();
        assert_eq!(
            terminal.kitty_image_storage_limit().unwrap(),
            IMAGE_STORAGE_LIMIT
        );
        let payload = STANDARD.encode(png());
        terminal.vt_write(format!("\x1b_Ga=t,f=100,i=7;{payload}\x1b\\").as_bytes());
        let reply: Vec<u8> = terminal
            .events()
            .filter_map(|event| match event {
                Event::PtyWrite(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(String::from_utf8_lossy(&reply), "\x1b_Gi=7;OK\x1b\\");
        let graphics = terminal.kitty_graphics().unwrap();
        let image = graphics.image(7).unwrap();
        assert_eq!((image.width().unwrap(), image.height().unwrap()), (2, 1));
        assert_eq!(image.data().unwrap(), [255, 0, 0, 255, 0, 0, 255, 128]);
    }
}
