use image::{
    DynamicImage, ImageDecoder, ImageReader, Rgb, RgbImage, codecs::jpeg::JpegEncoder,
    metadata::Orientation,
};
use std::io::Cursor;

/// Target width for served/stored book covers (px). The output is always
/// exactly this wide, padded if the source aspect ratio is not 2:3.
pub const COVER_MAX_WIDTH: u32 = 300;

/// Target height for served/stored book covers (px). The output is always
/// exactly this tall, padded if the source aspect ratio is not 2:3.
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
/// aspect ratio, pad to exactly 300x450 with a solid white or black canvas
/// (chosen by mean luminance of the resized image), then re-encode as JPEG.
///
/// The fixed output aspect ratio is what lets every peer render the same
/// cover identically: a non-2:3 source would otherwise be cropped differently
/// by each client's image widget. Padding with a uniform colour keeps the
/// JPEG size down because flat regions compress extremely well.
///
/// If the first encode exceeds the soft size cap, retries at lower quality
/// steps before returning the smallest result obtained.
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
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("into_decoder: {e}"))?;
    // iPhones (and many cameras) store sensor-native pixels and put the
    // logical rotation in an EXIF Orientation tag. If we skip this step the
    // re-encoded thumbnail looks rotated 90° to peers, with pad-coloured
    // bands above/below where the rotated content used to fit.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| format!("decode: {e}"))?;
    img.apply_orientation(orientation);

    let thumb = img.thumbnail(COVER_MAX_WIDTH, COVER_MAX_HEIGHT);
    let resized = thumb.to_rgb8();
    let padded = pad_to_target(&resized, COVER_MAX_WIDTH, COVER_MAX_HEIGHT);
    let (w, h) = padded.dimensions();

    let mut best: Option<Vec<u8>> = None;
    for quality in QUALITY_STEPS {
        let mut buf: Vec<u8> = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
        encoder
            .encode(padded.as_raw(), w, h, image::ExtendedColorType::Rgb8)
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

/// Pads `src` onto a `target_w` x `target_h` canvas, centred. The pad colour
/// is solid white or black, picked by the mean luminance of the source so
/// the padding blends with the dominant tone of the cover.
fn pad_to_target(src: &RgbImage, target_w: u32, target_h: u32) -> RgbImage {
    let (w, h) = src.dimensions();
    if w == target_w && h == target_h {
        return src.clone();
    }

    let pad_colour = pick_pad_colour(src);
    let mut canvas = RgbImage::from_pixel(target_w, target_h, pad_colour);

    // Integer division centres the content; any 1-pixel remainder sits on
    // the bottom/right which is imperceptible at this scale.
    let offset_x = (target_w.saturating_sub(w)) / 2;
    let offset_y = (target_h.saturating_sub(h)) / 2;

    for (x, y, pixel) in src.enumerate_pixels() {
        canvas.put_pixel(offset_x + x, offset_y + y, *pixel);
    }
    canvas
}

/// Picks a padding colour (pure white or pure black) based on the mean
/// luminance of the source. ITU-R BT.601 weighting matches how the human
/// eye perceives brightness, so the pad tone blends with the dominant
/// impression of the cover rather than its raw pixel average.
fn pick_pad_colour(src: &RgbImage) -> Rgb<u8> {
    let (w, h) = src.dimensions();
    let pixel_count = (w as u64) * (h as u64);
    if pixel_count == 0 {
        return Rgb([255, 255, 255]);
    }

    let mut sum: u64 = 0;
    for p in src.pixels() {
        // Scale weights by 1000 to keep integer math; max per-pixel contribution
        // is 255 * 1000 = 255_000 which stays well inside u64 even for 300*450.
        let y = 299 * (p[0] as u64) + 587 * (p[1] as u64) + 114 * (p[2] as u64);
        sum += y;
    }
    let mean = sum / (pixel_count * 1000);

    if mean > 128 {
        Rgb([255, 255, 255])
    } else {
        Rgb([0, 0, 0])
    }
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

    fn solid_color_image(w: u32, h: u32, color: [u8; 3]) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = image::Rgb(color);
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn resizes_large_2_3_png_to_exact_target() {
        let src = cover_like_image(1200, 1800);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).expect("resize");

        // Output is JPEG
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF]));

        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);
    }

    #[test]
    fn pads_square_input_to_2_3_box() {
        let src = cover_like_image(300, 300);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();

        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);
    }

    #[test]
    fn pads_landscape_input_to_2_3_box() {
        let src = cover_like_image(450, 300);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();

        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);
    }

    #[test]
    fn pads_narrow_portrait_to_2_3_box() {
        let src = cover_like_image(300, 400);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();

        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);
    }

    #[test]
    fn uses_white_padding_for_light_sources() {
        // Solid light gray (luminance ~220) must trigger white padding.
        let src = solid_color_image(300, 300, [220, 220, 220]);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap().to_rgb8();

        // Source 300x300 sits centered in 300x450 with 75px pad above/below.
        // Sample a pixel well inside the top pad band.
        let pad_pixel = decoded.get_pixel(150, 5);
        assert!(
            pad_pixel[0] > 200 && pad_pixel[1] > 200 && pad_pixel[2] > 200,
            "expected near-white padding for light source, got {:?}",
            pad_pixel
        );
    }

    #[test]
    fn uses_black_padding_for_dark_sources() {
        let src = solid_color_image(300, 300, [30, 30, 30]);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        let decoded = image::load_from_memory(&out).unwrap().to_rgb8();

        let pad_pixel = decoded.get_pixel(150, 5);
        assert!(
            pad_pixel[0] < 50 && pad_pixel[1] < 50 && pad_pixel[2] < 50,
            "expected near-black padding for dark source, got {:?}",
            pad_pixel
        );
    }

    /// Manual sanity check for the padding pipeline. Reads `/tmp/src.jpg`
    /// (or any format supported by the `image` crate) and writes the
    /// padded + re-encoded JPEG to `/tmp/cover_out.jpg` so the developer
    /// can inspect the result visually.
    ///
    /// Run with:
    ///   cp <some photo> /tmp/src.jpg
    ///   cargo test --lib -- --ignored manual_padding_check --nocapture
    ///   open /tmp/cover_out.jpg
    ///   sips -g pixelWidth -g pixelHeight /tmp/cover_out.jpg
    #[test]
    #[ignore = "manual visual check: writes /tmp/cover_out.jpg"]
    fn manual_padding_check() {
        let input = std::fs::read("/tmp/src.jpg")
            .expect("missing /tmp/src.jpg - copy a non-2:3 photo there first");

        let out = resize_to_jpeg_thumbnail(&input).expect("resize failed");
        std::fs::write("/tmp/cover_out.jpg", &out).expect("write output");

        let decoded = image::load_from_memory(&out).unwrap();
        println!(
            "wrote /tmp/cover_out.jpg: {}x{}, {} bytes",
            decoded.width(),
            decoded.height(),
            out.len()
        );
        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);
    }

    #[test]
    fn padded_output_stays_under_soft_cap() {
        // Padding adds 150px of uniform color to a 300x300 source. Uniform
        // regions compress very well in JPEG so the soft cap still holds.
        let src = cover_like_image(300, 300);
        let png = encode_png(&src);

        let out = resize_to_jpeg_thumbnail(&png).unwrap();
        assert!(
            out.len() <= COVER_SIZE_CAP_BYTES,
            "padded output {} bytes exceeds soft cap {}",
            out.len(),
            COVER_SIZE_CAP_BYTES,
        );
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

    /// Wraps a plain JPEG with a minimal APP1/EXIF segment carrying a single
    /// Orientation tag. Lets us simulate the kind of JPEG iPhones produce
    /// (sensor-native landscape pixels + EXIF orientation tag) without
    /// pulling in an EXIF writer dependency.
    fn jpeg_with_exif_orientation(rgb: &RgbImage, exif_orientation: u8) -> Vec<u8> {
        let (w, h) = rgb.dimensions();
        let mut jpeg = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 90);
        encoder
            .encode(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();

        // EXIF payload: TIFF header (little-endian) + one IFD0 entry.
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // "II", magic 42
        payload.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD0 offset
        payload.extend_from_slice(&[0x01, 0x00]); // 1 entry
        payload.extend_from_slice(&[0x12, 0x01]); // tag 0x0112 = Orientation
        payload.extend_from_slice(&[0x03, 0x00]); // type SHORT
        payload.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // count 1
        payload.extend_from_slice(&[exif_orientation, 0x00, 0x00, 0x00]);
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // next IFD = 0

        let segment_len: u16 = (payload.len() + 2) as u16;
        let mut app1: Vec<u8> = Vec::with_capacity(payload.len() + 4);
        app1.extend_from_slice(&[0xFF, 0xE1]);
        app1.extend_from_slice(&segment_len.to_be_bytes());
        app1.extend_from_slice(&payload);

        // Insert APP1 immediately after SOI (FF D8).
        let mut out = Vec::with_capacity(jpeg.len() + app1.len());
        out.extend_from_slice(&jpeg[..2]);
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn applies_exif_orientation_rotate90() {
        // Reproduces the iPhone "rotated cover with two black bands" bug.
        //
        // Pixels are stored as 600x400 landscape, but the EXIF Orientation
        // tag (value 6 = Rotate90 CW) declares the logical image is 400x600
        // portrait. After applying the orientation the image is exact 2:3
        // and fits 300x450 with NO padding bands.
        //
        // Without the EXIF fix, the decoder yields raw 600x400 pixels:
        // thumbnail() shrinks them to ~300x200, pad_to_target() then adds
        // two pad-coloured bands above and below the content. The assertions
        // below sample the centre column near top and bottom, where those
        // bands would land.
        let rgb = RgbImage::from_pixel(600, 400, image::Rgb([200, 50, 50]));
        let jpeg_in = jpeg_with_exif_orientation(&rgb, 6);

        let out = resize_to_jpeg_thumbnail(&jpeg_in).expect("resize");
        let decoded = image::load_from_memory(&out).unwrap().to_rgb8();

        assert_eq!(decoded.width(), COVER_MAX_WIDTH);
        assert_eq!(decoded.height(), COVER_MAX_HEIGHT);

        let top = decoded.get_pixel(150, 10);
        assert!(
            top[0] > 100 && top[1] < 120 && top[2] < 120,
            "expected reddish pixel near top (no padding band), got {top:?}",
        );
        let bottom = decoded.get_pixel(150, 440);
        assert!(
            bottom[0] > 100 && bottom[1] < 120 && bottom[2] < 120,
            "expected reddish pixel near bottom (no padding band), got {bottom:?}",
        );
    }
}
