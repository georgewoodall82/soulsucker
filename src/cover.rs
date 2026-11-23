//! The album-folder cover: a baseline 500x500 JPEG named `cover.jpg`.
//!
//! Players and file managers (Rockbox included) pick `cover.jpg` up from the
//! album folder. The source art can be a larger JPEG or a PNG, so it is decoded,
//! scaled to fit 500x500 and re-encoded. The encoder writes a baseline
//! (non-progressive) JPEG, which every embedded player can decode.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageReader};

pub const FOLDER_COVER_NAME: &str = "cover.jpg";
/// Edge length of the longest side, in pixels.
pub const FOLDER_COVER_SIZE: u32 = 500;
const JPEG_QUALITY: u8 = 90;
/// Refuse absurd images instead of allocating gigabytes for them.
const MAX_SOURCE_PIXELS: u64 = 100_000_000;

/// Scale `data` (a JPEG or PNG) to fit 500x500 and encode it as a JPEG.
///
/// Album art is square, so that is the normal result: exactly 500x500. A
/// non-square image keeps its aspect ratio (no stretching, no cropping) and
/// fits inside the box.
pub fn folder_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .context("could not identify the cover image")?;
    let (width, height) = reader
        .into_dimensions()
        .context("could not read the cover image size")?;
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(anyhow!("the cover image is too large ({width}x{height})"));
    }
    let image = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .context("could not identify the cover image")?
        .decode()
        .context("could not decode the cover image")?;
    encode_jpeg(&fit(image))
}

fn fit(image: DynamicImage) -> DynamicImage {
    if image.width() == FOLDER_COVER_SIZE && image.height() == FOLDER_COVER_SIZE {
        return image;
    }
    image.resize(FOLDER_COVER_SIZE, FOLDER_COVER_SIZE, FilterType::Lanczos3)
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>> {
    // JPEG has no alpha channel; flatten to RGB.
    let rgb = image.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .context("could not encode the cover as JPEG")?;
    Ok(out)
}

/// What happened to the folder cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderCoverResult {
    Written,
    /// A `cover.jpg` is already there and was left alone.
    Kept,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Save `jpeg` as `dir/cover.jpg`. An existing file is only replaced when
/// `overwrite` is set. The write goes through a temporary file, so a crash
/// never leaves a truncated cover behind.
pub fn write_folder_cover(dir: &Path, jpeg: &[u8], overwrite: bool) -> Result<FolderCoverResult> {
    let target = dir.join(FOLDER_COVER_NAME);
    if !overwrite && target.exists() {
        return Ok(FolderCoverResult::Kept);
    }
    let temp = dir.join(format!(
        ".soulsucker-cover-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> Result<()> {
        // `create_new` refuses to follow a pre-planted file or symlink.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("could not create {}", temp.display()))?;
        file.write_all(jpeg)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("could not write {}", temp.display()))?;
        fs::rename(&temp, &target).with_context(|| format!("could not save {}", target.display()))
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map(|()| FolderCoverResult::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn decoded(jpeg: &[u8]) -> DynamicImage {
        image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).unwrap()
    }

    #[test]
    fn large_square_art_becomes_a_500_square_jpeg() {
        let jpeg = folder_jpeg(&png(900, 900)).unwrap();
        assert_eq!(&jpeg[..3], b"\xff\xd8\xff");
        let image = decoded(&jpeg);
        assert_eq!((image.width(), image.height()), (500, 500));
    }

    #[test]
    fn a_jpeg_source_is_accepted_and_baseline() {
        let jpeg = folder_jpeg(&png(600, 600)).unwrap();
        // Round trip: a JPEG in, a 500x500 JPEG out.
        let again = folder_jpeg(&jpeg).unwrap();
        assert_eq!(decoded(&again).width(), 500);
        // No progressive-DCT marker (SOF2): Rockbox-class decoders need
        // baseline.
        assert!(!again.windows(2).any(|pair| pair == [0xff, 0xc2]));
    }

    #[test]
    fn non_square_art_keeps_its_aspect_ratio() {
        let image = decoded(&folder_jpeg(&png(1000, 800)).unwrap());
        assert_eq!((image.width(), image.height()), (500, 400));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(folder_jpeg(b"not an image").is_err());
        assert!(folder_jpeg(b"").is_err());
    }

    #[test]
    fn the_folder_cover_is_written_atomically_and_not_overwritten() {
        let dir =
            std::env::temp_dir().join(format!("soulsucker-cover-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let result = write_folder_cover(&dir, b"first", false).unwrap();
        assert_eq!(result, FolderCoverResult::Written);
        assert_eq!(fs::read(dir.join(FOLDER_COVER_NAME)).unwrap(), b"first");

        let result = write_folder_cover(&dir, b"second", false).unwrap();
        assert_eq!(result, FolderCoverResult::Kept);
        assert_eq!(fs::read(dir.join(FOLDER_COVER_NAME)).unwrap(), b"first");

        let result = write_folder_cover(&dir, b"third", true).unwrap();
        assert_eq!(result, FolderCoverResult::Written);
        assert_eq!(fs::read(dir.join(FOLDER_COVER_NAME)).unwrap(), b"third");

        // No temporary files are left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != FOLDER_COVER_NAME)
            .collect();
        assert!(leftovers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
