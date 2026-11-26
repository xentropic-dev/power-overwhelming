use crate::persistence::state_directory;
use anyhow::{Context, Result, ensure};
use application::dag::BoxIcons;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use std::{
    fs::{self, OpenOptions},
    io::{Cursor, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Stored icons are at most this many pixels on a side, enough for the strip at 3x scale.
const ICON_SIZE: u32 = 64;
const MAX_SOURCE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 8192;

/// The normalized PNG the desktop shows for a box, whether or not it exists yet.
pub fn icon_path(box_id: &str) -> Result<PathBuf> {
    Ok(icon_directory()?.join(icon_name(box_id)?))
}

fn icon_directory() -> Result<PathBuf> {
    Ok(state_directory()?.join("box-icons"))
}

fn icon_name(box_id: &str) -> Result<String> {
    ensure!(
        !box_id.is_empty()
            && box_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "Invalid box ID {box_id:?}"
    );
    Ok(format!("{box_id}.png"))
}

/// Icons stored as normalized PNGs, so the desktop never reads the caller's file.
pub(crate) struct IconFiles {
    directory: PathBuf,
}

impl IconFiles {
    pub(crate) fn open() -> Result<Self> {
        Ok(Self {
            directory: icon_directory()?,
        })
    }
}

impl BoxIcons for IconFiles {
    fn set(&self, box_id: &str, source: &Path) -> Result<()> {
        let target = self.directory.join(icon_name(box_id)?);
        let png = normalize(source)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)?;
        let temporary = target.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&png)?;
            file.flush()?;
            fs::rename(&temporary, &target)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("Save the box icon")
    }

    fn clear(&self, box_id: &str) -> Result<()> {
        match fs::remove_file(self.directory.join(icon_name(box_id)?)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).context("Remove the box icon")
            }
            _ => Ok(()),
        }
    }
}

fn normalize(source: &Path) -> Result<Vec<u8>> {
    let size = fs::metadata(source)
        .with_context(|| format!("Icon is unavailable at {}", source.display()))?
        .len();
    ensure!(
        size <= MAX_SOURCE_BYTES,
        "Icon files must be at most {} MB",
        MAX_SOURCE_BYTES / 1024 / 1024
    );
    let mut reader = ImageReader::open(source)?.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    reader.limits(limits);
    let image = reader
        .decode()
        .with_context(|| format!("{} is not a supported image", source.display()))?;
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(fit(image).to_rgba8())
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(png)
}

/// Shrinks large images smoothly and enlarges small ones by whole pixels, so pixel art stays crisp.
fn fit(image: DynamicImage) -> DynamicImage {
    let side = image.width().max(image.height());
    if side > ICON_SIZE {
        image.resize(ICON_SIZE, ICON_SIZE, FilterType::Lanczos3)
    } else if side * 2 <= ICON_SIZE {
        let scale = ICON_SIZE / side;
        image.resize(
            image.width() * scale,
            image.height() * scale,
            FilterType::Nearest,
        )
    } else {
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, RgbaImage};

    const BOX: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    fn store() -> (tempfile::TempDir, IconFiles) {
        let directory = tempfile::tempdir().unwrap();
        let icons = IconFiles {
            directory: directory.path().join("box-icons"),
        };
        (directory, icons)
    }

    fn stored(icons: &IconFiles) -> DynamicImage {
        image::open(icons.directory.join(format!("{BOX}.png"))).unwrap()
    }

    #[test]
    fn large_images_shrink_to_fit_and_reset_removes_them() -> Result<()> {
        let (directory, icons) = store();
        let source = directory.path().join("wide.jpg");
        DynamicImage::ImageRgb8(image::RgbImage::new(400, 200)).save(&source)?;
        icons.set(BOX, &source)?;
        assert_eq!(stored(&icons).dimensions(), (64, 32));
        icons.clear(BOX)?;
        assert!(!icons.directory.join(format!("{BOX}.png")).exists());
        icons.clear(BOX)?;
        Ok(())
    }

    #[test]
    fn pixel_art_scales_by_whole_pixels() -> Result<()> {
        let (directory, icons) = store();
        let source = directory.path().join("sprite.png");
        let mut sprite = RgbaImage::new(16, 16);
        sprite.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        sprite.save(&source)?;
        icons.set(BOX, &source)?;
        let icon = stored(&icons);
        assert_eq!(icon.dimensions(), (64, 64));
        assert_eq!(icon.get_pixel(3, 3), image::Rgba([255, 0, 0, 255]));
        assert_eq!(icon.get_pixel(4, 4), image::Rgba([0, 0, 0, 0]));
        Ok(())
    }

    #[test]
    fn rejects_non_images_and_unsafe_ids() -> Result<()> {
        let (directory, icons) = store();
        let source = directory.path().join("notes.png");
        fs::write(&source, "not an image")?;
        assert!(icons.set(BOX, &source).is_err());
        assert!(!icons.directory.join(format!("{BOX}.png")).exists());
        assert!(icon_name("../boxes").is_err());
        assert!(icon_name("").is_err());
        Ok(())
    }
}
