use image::{ImageReader, codecs::jpeg::JpegEncoder};
use std::io::Cursor;

/// Target max width for served/stored book covers (px).
pub const COVER_MAX_WIDTH: u32 = 300;

/// Target max height for served/stored book covers (px).
pub const COVER_MAX_HEIGHT: u32 = 450;

/// Default JPEG quality for cover thumbnails (0-100).
pub const COVER_JPEG_QUALITY: u8 = 85;

/// Soft size cap for an encoded cover in bytes. If the first pass exceeds
/// this, we re-encode at a lower quality step before giving up.
pub const COVER_SIZE_CAP_BYTES: usize = 50 * 1024;

/// Hard cap on the raw input we accept. Prevents pathological files
/// from pinning memory during decode.
pub const COVER_MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;

/// Quality steps tried in order when the output exceeds the soft size cap.
const QUALITY_STEPS: [u8; 3] = [COVER_JPEG_QUALITY, 75, 65];

/// Decode an arbitrary image, resize it to fit within 300x450 while keeping
/// aspect ratio, and re-encode as JPEG. If the first encode exceeds the soft
/// size cap, retries at lower quality steps before returning the smallest
/// result obtained.
///
/// This is CPU-bound; callers running inside an async context should invoke
/// it from `tokio::task::spawn_blocking`.
pub fn resize_to_jpeg_thumbnail(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() > COVER_MAX_INPUT_BYTES {
        return Err(format!(
            "input too large: {} bytes (max {})",
            input.len(),
            COVER_MAX_INPUT_BYTES
        ));
    }

    let reader = ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .map_err(|e| format!("guess format: {e}"))?;
    let img = reader.decode().map_err(|e| format!("decode: {e}"))?;

    let thumb = img.thumbnail(COVER_MAX_WIDTH, COVER_MAX_HEIGHT);
    let rgb = thumb.to_rgb8();
    let (w, h) = rgb.dimensions();

    let mut best: Option<Vec<u8>> = None;
    for quality in QUALITY_STEPS {
        let mut buf: Vec<u8> = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
        encoder
            .encode(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| format!("encode q={quality}: {e}"))?;

        let fits_cap = buf.len() <= COVER_SIZE_CAP_BYTES;
        let is_smaller = best.as_ref().is_none_or(|b| buf.len() < b.len());

        if fits_cap {
            return Ok(buf);
        }
        if is_smaller {
            best = Some(buf);
        }
    }

    best.ok_or_else(|| "no encode attempt succeeded".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn encode_png(img: &DynamicImage) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    fn encode_jpeg_quality(img: &DynamicImage, quality: u8) -> Vec<u8> {
        let rgb = img.to_rgb8();
        let (w, h) = rgb.dimensions();
        let mut buf = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
        encoder
            .encode(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    /// Build a deterministic RGB image resembling a real book cover:
    /// smooth gradients with low-frequency variation. Compresses to JPEG at
    /// rates comparable to real-world photos.
    fn cover_like_image(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        let wf = w as f32;
        let hf = h as f32;
        for (x, y, p) in img.enumerate_pixels_mut() {
            let u = x as f32 / wf;
            let v = y as f32 / hf;
            let r = (128.0 + 80.0 * (u * std::f32::consts::PI).sin()) as u8;
            let g = (128.0 + 80.0 * (v * std::f32::consts::PI * 2.0).cos()) as u8;
            let b = (128.0 + 60.0 * ((u + v) * std::f32::consts::PI).sin()) as u8;
            *p = image::Rgb([r, g, b]);
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn resizes_large_png_down_to_target_box() {
        let src = cover_like_image(1200, 1800);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).expect("resize");

        // Output is JPEG
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF]));

        let decoded = image::load_from_memory(&out).unwrap();
        assert!(
            decoded.width() <= COVER_MAX_WIDTH && decoded.height() <= COVER_MAX_HEIGHT,
            "dimensions {}x{} must fit in {}x{}",
            decoded.width(),
            decoded.height(),
            COVER_MAX_WIDTH,
            COVER_MAX_HEIGHT,
        );
        // At least one axis reaches the target box (aspect preserved).
        assert!(
            decoded.width() == COVER_MAX_WIDTH || decoded.height() == COVER_MAX_HEIGHT,
            "expected one axis to hit the target box",
        );
    }

    #[test]
    fn preserves_aspect_ratio_for_square_input() {
        let src = cover_like_image(800, 800);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();

        assert_eq!(decoded.width(), decoded.height(), "square stays square");
        assert!(decoded.width() <= COVER_MAX_WIDTH);
    }

    #[test]
    fn stays_under_soft_cap_on_typical_input() {
        let src = cover_like_image(600, 900);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        assert!(
            out.len() <= COVER_SIZE_CAP_BYTES,
            "output {} bytes exceeds soft cap {}",
            out.len(),
            COVER_SIZE_CAP_BYTES,
        );
    }

    #[test]
    fn rejects_input_over_hard_cap() {
        let oversized = vec![0u8; COVER_MAX_INPUT_BYTES + 1];
        let err = resize_to_jpeg_thumbnail(&oversized).unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_invalid_image_bytes() {
        let garbage = b"this is not an image";
        assert!(resize_to_jpeg_thumbnail(garbage).is_err());
    }

    #[test]
    fn accepts_jpeg_input_and_re_encodes() {
        // Simulate a user-uploaded JPEG that is already reasonable size.
        let src = cover_like_image(400, 600);
        let jpeg_in = encode_jpeg_quality(&src, 90);

        let out = resize_to_jpeg_thumbnail(&jpeg_in).unwrap();
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(out.len() <= COVER_SIZE_CAP_BYTES);
    }
}
