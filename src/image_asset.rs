use std::{
    fs,
    io::{BufReader, Cursor},
    path::Path,
    sync::Arc,
    time::Duration,
};

use eframe::egui;
use image::{
    AnimationDecoder, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::gif::GifDecoder,
};
use thiserror::Error;

pub const DEFAULT_EYE_BYTES: &[u8] = include_bytes!("../assets/default-eye.png");
const MAX_FILE_SIZE: u64 = 32 * 1024 * 1024;
const MAX_SVG_FILE_SIZE: usize = 2 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4_096;
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_GIF_FRAMES: usize = 300;
const MAX_GIF_DURATION: Duration = Duration::from_secs(5 * 60);
const MIN_FRAME_DURATION: Duration = Duration::from_millis(50);
const MAX_FRAME_DURATION: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ImageAssetError {
    #[error("image file is larger than 32 MiB")]
    TooLarge,
    #[error("SVG document is larger than 2 MiB")]
    SvgTooLarge,
    #[error("image dimensions exceed 4096 x 4096")]
    DimensionsTooLarge,
    #[error("decoded image data exceeds 64 MiB")]
    DecodedDataTooLarge,
    #[error("GIF contains more than 300 frames")]
    TooManyFrames,
    #[error("GIF animation cycle is longer than 5 minutes")]
    AnimationTooLong,
    #[error("GIF contains no frames")]
    EmptyAnimation,
    #[error("GIF frame dimensions do not match its canvas")]
    FrameDimensionsMismatch,
    #[error("failed to parse SVG image: {0}")]
    SvgParse(String),
    #[error("failed to allocate the SVG raster image")]
    SvgRasterAllocation,
    #[error("failed to read image: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to decode PNG, JPEG, WebP, or GIF image: {0}")]
    Decode(#[from] image::ImageError),
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub rgba: Arc<[u8]>,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub color_image: egui::ColorImage,
    pub aspect_ratio: f32,
    pub size: [u32; 2],
    frames: Vec<DecodedFrame>,
    cycle_duration: Duration,
}

impl DecodedImage {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    pub fn frame_or_first(&self, index: usize) -> &DecodedFrame {
        self.frames.get(index).unwrap_or(&self.frames[0])
    }

    pub fn frame_at(&self, elapsed: Duration) -> (usize, Duration) {
        if !self.is_animated() || self.cycle_duration.is_zero() {
            return (0, Duration::MAX);
        }
        let mut position = elapsed.as_millis() % self.cycle_duration.as_millis();
        for (index, frame) in self.frames.iter().enumerate() {
            let duration = frame.duration.as_millis();
            if position < duration {
                return (index, Duration::from_millis((duration - position) as u64));
            }
            position -= duration;
        }
        (0, self.frames[0].duration)
    }
}

pub fn load_default() -> Result<DecodedImage, ImageAssetError> {
    decode(DEFAULT_EYE_BYTES)
}

pub fn load_file(path: &Path) -> Result<DecodedImage, ImageAssetError> {
    let metadata = fs::metadata(path)?;
    let is_svg = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    if is_svg && metadata.len() > MAX_SVG_FILE_SIZE as u64 {
        return Err(ImageAssetError::SvgTooLarge);
    }
    if metadata.len() > MAX_FILE_SIZE {
        return Err(ImageAssetError::TooLarge);
    }
    let bytes = fs::read(path)?;
    if is_svg {
        decode_svg(&bytes)
    } else {
        decode(&bytes)
    }
}

fn decode(bytes: &[u8]) -> Result<DecodedImage, ImageAssetError> {
    if image::guess_format(bytes)? == ImageFormat::Gif {
        return decode_gif(bytes);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(decoding_limits());
    let image = reader.decode()?;
    validate_dimensions(image.width(), image.height())?;
    let rgba = image.into_rgba8();
    decoded_image_from_frames(
        [rgba.width(), rgba.height()],
        vec![DecodedFrame {
            rgba: rgba.into_raw().into(),
            duration: Duration::ZERO,
        }],
    )
}

fn decode_gif(bytes: &[u8]) -> Result<DecodedImage, ImageAssetError> {
    let mut decoder = GifDecoder::new(BufReader::new(Cursor::new(bytes)))?;
    decoder.set_limits(decoding_limits())?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    let mut frames = Vec::new();
    let mut decoded_bytes = 0_usize;
    let mut cycle_duration = Duration::ZERO;
    for frame in decoder.into_frames() {
        if frames.len() == MAX_GIF_FRAMES {
            return Err(ImageAssetError::TooManyFrames);
        }
        let frame = frame?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let millis = u64::from(numerator)
            .div_ceil(u64::from(denominator.max(1)))
            .max(MIN_FRAME_DURATION.as_millis() as u64)
            .min(MAX_FRAME_DURATION.as_millis() as u64);
        let duration = Duration::from_millis(millis);
        cycle_duration = cycle_duration.saturating_add(duration);
        if cycle_duration > MAX_GIF_DURATION {
            return Err(ImageAssetError::AnimationTooLong);
        }
        let buffer = frame.into_buffer();
        if buffer.width() != width || buffer.height() != height {
            return Err(ImageAssetError::FrameDimensionsMismatch);
        }
        let rgba = buffer.into_raw();
        decoded_bytes = decoded_bytes.saturating_add(rgba.len());
        if decoded_bytes > MAX_DECODED_BYTES {
            return Err(ImageAssetError::DecodedDataTooLarge);
        }
        frames.push(DecodedFrame {
            rgba: rgba.into(),
            duration,
        });
    }
    if frames.is_empty() {
        return Err(ImageAssetError::EmptyAnimation);
    }
    decoded_image_from_frames([width, height], frames)
}

fn decode_svg(bytes: &[u8]) -> Result<DecodedImage, ImageAssetError> {
    if bytes.len() > MAX_SVG_FILE_SIZE {
        return Err(ImageAssetError::SvgTooLarge);
    }
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data_nested(bytes, &options)
        .map_err(|error| ImageAssetError::SvgParse(error.to_string()))?;
    let size = tree.size();
    let width = size.width().ceil() as u32;
    let height = size.height().ceil() as u32;
    validate_dimensions(width, height)?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(ImageAssetError::SvgRasterAllocation)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    decoded_image_from_frames(
        [width, height],
        vec![DecodedFrame {
            rgba: rgba.into(),
            duration: Duration::ZERO,
        }],
    )
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), ImageAssetError> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(ImageAssetError::DimensionsTooLarge);
    }
    let decoded_bytes = u64::from(width) * u64::from(height) * 4;
    if decoded_bytes > MAX_DECODED_BYTES as u64 {
        return Err(ImageAssetError::DecodedDataTooLarge);
    }
    Ok(())
}

fn decoding_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES as u64);
    limits
}

fn decoded_image_from_frames(
    size: [u32; 2],
    frames: Vec<DecodedFrame>,
) -> Result<DecodedImage, ImageAssetError> {
    let first = frames
        .first()
        .ok_or_else(|| ImageAssetError::EmptyAnimation)?;
    let cycle_duration = frames
        .iter()
        .fold(Duration::ZERO, |total, frame| total + frame.duration);
    let aspect_ratio = size[0] as f32 / size[1].max(1) as f32;
    Ok(DecodedImage {
        color_image: egui::ColorImage::from_rgba_unmultiplied(
            [size[0] as usize, size[1] as usize],
            &first.rgba,
        ),
        aspect_ratio,
        size,
        frames,
        cycle_duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};

    #[test]
    fn bundled_eye_is_a_decodable_rgba_image() {
        let image = load_default().expect("bundled eye should decode");
        assert!(image.color_image.width() > 0);
        assert!(image.color_image.height() > 0);
        assert!(image.aspect_ratio > 1.0);
        assert!(image.color_image.pixels.iter().any(|pixel| pixel.a() == 0));
        assert!(image.color_image.pixels.iter().any(|pixel| pixel.a() > 0));
    }

    #[test]
    fn animated_gif_preserves_frames_and_timing() {
        let bytes = encode_gif(vec![
            gif_frame([255, 0, 0, 255], 50),
            gif_frame([0, 255, 0, 255], 60),
        ]);
        let image = decode(&bytes).expect("GIF should decode");

        assert!(image.is_animated());
        assert_eq!(image.frame_or_first(0).rgba.as_ref(), [255, 0, 0, 255]);
        assert_eq!(image.frame_or_first(1).rgba.as_ref(), [0, 255, 0, 255]);
        assert_eq!(image.frame_at(Duration::from_millis(49)).0, 0);
        assert_eq!(image.frame_at(Duration::from_millis(50)).0, 1);
        assert_eq!(image.frame_at(Duration::from_millis(110)).0, 0);
    }

    #[test]
    fn static_image_timing_and_frame_fallback_are_stable() {
        let image = decoded_image_from_frames(
            [1, 1],
            vec![DecodedFrame {
                rgba: Arc::from([10, 20, 30, 255]),
                duration: Duration::ZERO,
            }],
        )
        .expect("static image should be created");

        assert!(!image.is_animated());
        assert_eq!(image.frame_at(Duration::from_secs(10)), (0, Duration::MAX));
        assert_eq!(image.frame_or_first(99).rgba.as_ref(), [10, 20, 30, 255]);
    }

    #[test]
    fn gif_frame_delays_are_clamped_to_safe_bounds() {
        let bytes = encode_gif(vec![
            gif_frame([255, 0, 0, 255], 1),
            gif_frame([0, 255, 0, 255], 61_000),
        ]);
        let image = decode(&bytes).expect("GIF should decode");

        assert_eq!(image.frame_or_first(0).duration, MIN_FRAME_DURATION);
        assert_eq!(image.frame_or_first(1).duration, MAX_FRAME_DURATION);
        assert_eq!(image.frame_at(Duration::ZERO), (0, MIN_FRAME_DURATION));
        assert_eq!(image.frame_at(MIN_FRAME_DURATION), (1, MAX_FRAME_DURATION));
    }

    #[test]
    fn gif_cycle_duration_is_bounded() {
        let frames = (0..6).map(|_| gif_frame([0, 0, 0, 0], 60_000)).collect();
        let error = decode(&encode_gif(frames)).expect_err("long GIF should be rejected");

        assert!(matches!(error, ImageAssetError::AnimationTooLong));
    }

    #[test]
    fn gif_frame_count_is_bounded() {
        let frames = (0..=MAX_GIF_FRAMES)
            .map(|_| gif_frame([0, 0, 0, 0], 20))
            .collect();
        let error = decode(&encode_gif(frames)).expect_err("too many frames should be rejected");

        assert!(matches!(error, ImageAssetError::TooManyFrames));
    }

    #[test]
    fn svg_is_rasterized_with_alpha() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="1">
            <rect width="1" height="1" fill="#ff0000"/>
        </svg>"##;
        let image = decode_svg(svg).expect("SVG should rasterize");

        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.frame_or_first(0).rgba[0..4], [255, 0, 0, 255]);
        assert_eq!(image.frame_or_first(0).rgba[4..8], [0, 0, 0, 0]);
    }

    #[test]
    fn svg_external_images_are_not_loaded() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let image_path = directory.path().join("external.png");
        RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 255]))
            .save(&image_path)
            .expect("external test image should save");
        let href = image_path.to_string_lossy().replace('\\', "/");
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="{href}" width="1" height="1"/></svg>"#
        );
        let image = decode_svg(svg.as_bytes()).expect("SVG should parse without external image");

        assert_eq!(image.frame_or_first(0).rgba.as_ref(), [0, 0, 0, 0]);
    }

    #[test]
    fn svg_document_size_is_bounded() {
        let oversized = vec![b' '; MAX_SVG_FILE_SIZE + 1];
        let error = decode_svg(&oversized).expect_err("oversized SVG should be rejected");

        assert!(matches!(error, ImageAssetError::SvgTooLarge));
    }

    #[test]
    fn invalid_and_oversized_svg_dimensions_are_reported() {
        let parse_error = decode_svg(b"not an svg").expect_err("invalid SVG should be rejected");
        assert!(matches!(parse_error, ImageAssetError::SvgParse(_)));

        let oversized = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4097" height="1"/>"#;
        let size_error = decode_svg(oversized).expect_err("wide SVG should be rejected");
        assert!(matches!(size_error, ImageAssetError::DimensionsTooLarge));
    }

    #[test]
    fn image_dimension_boundaries_are_enforced() {
        assert!(validate_dimensions(1, 1).is_ok());
        assert!(validate_dimensions(MAX_DIMENSION, MAX_DIMENSION).is_ok());
        assert!(matches!(
            validate_dimensions(0, 1),
            Err(ImageAssetError::DimensionsTooLarge)
        ));
        assert!(matches!(
            validate_dimensions(MAX_DIMENSION + 1, 1),
            Err(ImageAssetError::DimensionsTooLarge)
        ));
    }

    #[test]
    fn unpremultiplication_handles_transparent_partial_and_opaque_pixels() {
        let mut rgba = [1, 2, 3, 0, 64, 32, 16, 128, 9, 8, 7, 255];

        unpremultiply_rgba(&mut rgba);

        assert_eq!(rgba, [0, 0, 0, 0, 128, 64, 32, 128, 9, 8, 7, 255]);
    }

    #[test]
    fn file_size_is_checked_before_decoding() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("large.png");
        let file = fs::File::create(&path).expect("large test file should be created");
        file.set_len(MAX_FILE_SIZE + 1)
            .expect("large test file should be resized");

        let error = load_file(&path).expect_err("large image should be rejected");

        assert!(matches!(error, ImageAssetError::TooLarge));
    }

    fn gif_frame(color: [u8; 4], duration_ms: u32) -> Frame {
        Frame::from_parts(
            RgbaImage::from_pixel(1, 1, Rgba(color)),
            0,
            0,
            Delay::from_numer_denom_ms(duration_ms, 1),
        )
    }

    fn encode_gif(frames: Vec<Frame>) -> Vec<u8> {
        let mut bytes = Vec::new();
        GifEncoder::new(&mut bytes)
            .encode_frames(frames)
            .expect("test GIF should encode");
        bytes
    }
}
