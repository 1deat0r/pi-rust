//! Image MIME detection + base64 encoding — port of
//! `packages/agent/src/harness/tools/image.ts`.
//!
//! Determines the mime type a model can attach from raw file bytes and
//! processes image payloads for provider limits. This ports the upstream
//! `harness/tools/image.ts` detector/encoder plus coding-agent's
//! `utils/image-process.ts` normalization and resize policy.

use std::io::Cursor;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// Upstream's default maximum dimensions for provider-facing images.
pub const DEFAULT_MAX_WIDTH: u32 = 2_000;
pub const DEFAULT_MAX_HEIGHT: u32 = 2_000;
/// Upstream keeps a 0.5MB headroom below Anthropic's 5MB base64 limit.
pub const DEFAULT_MAX_BASE64_BYTES: usize = 4_500 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct ProcessImageOptions {
    pub auto_resize_images: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_base64_bytes: usize,
    pub jpeg_quality: u8,
}

impl Default for ProcessImageOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            max_width: DEFAULT_MAX_WIDTH,
            max_height: DEFAULT_MAX_HEIGHT,
            max_base64_bytes: DEFAULT_MAX_BASE64_BYTES,
            jpeg_quality: 80,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

/// Detect the model-attachable image mime type from raw bytes. Returns
/// `None` for unsupported or invalid images (mirrors upstream:
/// JPEG with a trailing byte 0xf7 at index 3 is rejected, animated PNG is
/// rejected, malformed BMP is rejected).
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with_bytes(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with_bytes(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(buffer, 0, "GIF") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// Base64-encode bytes using the upstream manual algorithm (the
/// `undefined` padding maps to `=`; partial trailing bytes are handled with
/// zero-fill exactly like the reference).
pub fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut index = 0usize;
    while index < bytes.len() {
        let first = bytes[index];
        let second = bytes.get(index + 1).copied();
        let third = bytes.get(index + 2).copied();
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(
            ALPHABET[(((first & 0x03) << 4) | ((second.unwrap_or(0)) >> 4)) as usize] as char,
        );
        match second {
            Some(second) => {
                output.push(
                    ALPHABET[(((second & 0x0f) << 2) | ((third.unwrap_or(0)) >> 6)) as usize]
                        as char,
                );
            }
            None => output.push('='),
        }
        match third {
            Some(third) => output.push(ALPHABET[(third & 0x3f) as usize] as char),
            None => output.push('='),
        }
        index += 3;
    }
    output
}

/// Normalize and, when enabled, resize an image using the same observable
/// policy as coding-agent's `processImage`.
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    options: ProcessImageOptions,
) -> Result<ProcessedImage, String> {
    let (normalized_bytes, normalized_mime, converted_from) = normalize_image(bytes, mime_type)?;
    let conversion_hint = converted_from
        .as_deref()
        .filter(|from| *from != normalized_mime)
        .map(|from| format!("[Image converted from {from} to {normalized_mime}.]"));

    if !options.auto_resize_images {
        let mut hints = Vec::new();
        if let Some(hint) = conversion_hint {
            hints.push(hint);
        }
        // Upstream's non-resizing path passes through the normalized bytes
        // without decoding them. Keep dimensions as best-effort metadata so
        // a detector-accepted but decoder-unsupported payload still follows
        // that pass-through behavior.
        let (width, height) =
            image_dimensions(&normalized_bytes, &normalized_mime).unwrap_or((0, 0));
        return Ok(ProcessedImage {
            data: encode_base64(&normalized_bytes),
            mime_type: normalized_mime,
            hints,
            original_width: width,
            original_height: height,
            width,
            height,
            was_resized: converted_from.is_some(),
        });
    }

    let image = decode_image(&normalized_bytes, &normalized_mime)?;
    let (original_width, original_height) = (image.width(), image.height());
    let input_base64_size = normalized_bytes.len().div_ceil(3) * 4;

    if original_width <= options.max_width
        && original_height <= options.max_height
        && input_base64_size < options.max_base64_bytes
        && converted_from.is_none()
    {
        return Ok(ProcessedImage {
            data: encode_base64(&normalized_bytes),
            mime_type: normalized_mime,
            hints: Vec::new(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut width, mut height) = fit_dimensions(
        original_width,
        original_height,
        options.max_width,
        options.max_height,
    );
    let quality_steps = unique_quality_steps(options.jpeg_quality);

    loop {
        let resized = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        let candidates = encode_candidates(&resized, &quality_steps)?;
        for (data, mime_type) in candidates {
            if data.len() < options.max_base64_bytes {
                let mut hints = Vec::new();
                if let Some(hint) = conversion_hint.as_deref() {
                    hints.push(hint.to_string());
                }
                if width != original_width || height != original_height {
                    let scale = original_width as f64 / width as f64;
                    hints.push(format!(
                        "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {scale:.2} to map to original image.]"
                    ));
                }
                return Ok(ProcessedImage {
                    data,
                    mime_type,
                    hints,
                    original_width,
                    original_height,
                    width,
                    height,
                    was_resized: true,
                });
            }
        }

        if width == 1 && height == 1 {
            return Err(
                "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_string(),
            );
        }
        let next_width = if width == 1 {
            1
        } else {
            (width as f64 * 0.75).floor().max(1.0) as u32
        };
        let next_height = if height == 1 {
            1
        } else {
            (height as f64 * 0.75).floor().max(1.0) as u32
        };
        if next_width == width && next_height == height {
            return Err(
                "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_string(),
            );
        }
        width = next_width;
        height = next_height;
    }
}

fn normalize_image(
    bytes: &[u8],
    mime_type: &str,
) -> Result<(Vec<u8>, String, Option<String>), String> {
    let normalized_mime = match mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/bmp" => None,
        _ => None,
    };
    if let Some(normalized_mime) = normalized_mime {
        return Ok((bytes.to_vec(), normalized_mime.to_string(), None));
    }

    let image = decode_image(bytes, mime_type)?;
    let png = encode_png(&image)?;
    Ok((png, "image/png".to_string(), Some(mime_type.to_string())))
}

fn decode_image(bytes: &[u8], mime_type: &str) -> Result<image::DynamicImage, String> {
    let format = match mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" | "image/jpg" => image::ImageFormat::Jpeg,
        "image/gif" => image::ImageFormat::Gif,
        "image/webp" => image::ImageFormat::WebP,
        "image/bmp" => image::ImageFormat::Bmp,
        other => return Err(format!("unsupported image format {other}")),
    };
    image::load_from_memory_with_format(bytes, format)
        .map_err(|error| format!("could not decode image: {error}"))
}

fn image_dimensions(bytes: &[u8], mime_type: &str) -> Result<(u32, u32), String> {
    let image = decode_image(bytes, mime_type)?;
    Ok((image.width(), image.height()))
}

fn encode_png(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| format!("could not encode image as PNG: {error}"))?;
    Ok(output.into_inner())
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
    encoder
        .encode_image(&image.to_rgb8())
        .map_err(|error| format!("could not encode image as JPEG: {error}"))?;
    Ok(output.into_inner())
}

fn encode_candidates(
    image: &image::DynamicImage,
    qualities: &[u8],
) -> Result<Vec<(String, String)>, String> {
    let mut candidates = vec![(encode_base64(&encode_png(image)?), "image/png".to_string())];
    for quality in qualities {
        candidates.push((
            encode_base64(&encode_jpeg(image, *quality)?),
            "image/jpeg".to_string(),
        ));
    }
    Ok(candidates)
}

fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let mut width = width;
    let mut height = height;
    if width > max_width {
        height = ((height as f64 * max_width as f64 / width as f64).round() as u32).max(1);
        width = max_width.max(1);
    }
    if height > max_height {
        width = ((width as f64 * max_height as f64 / height as f64).round() as u32).max(1);
        height = max_height.max(1);
    }
    (width.max(1), height.max(1))
}

fn unique_quality_steps(primary: u8) -> Vec<u8> {
    [primary, 85, 70, 55, 40]
        .into_iter()
        .filter(|quality| *quality <= 100)
        .fold(Vec::new(), |mut values, quality| {
            if !values.contains(&quality) {
                values.push(quality);
            }
            values
        })
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length as usize + 4;
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(buffer, 2);
    let pixel_data_offset = read_u32_le(buffer, 10);
    let dib_header_size = read_u32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_u16_le(buffer, 22), read_u16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_u16_le(buffer, 26), read_u16_le(buffer, 28))
    } else {
        return false;
    };
    color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u32 {
    (buffer.get(offset).copied().unwrap_or(0) as u32)
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u32) << 8)
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    (buffer.get(offset).copied().unwrap_or(0) as u32) * 0x1000000
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u32) << 16)
        + ((buffer.get(offset + 2).copied().unwrap_or(0) as u32) << 8)
        + (buffer.get(offset + 3).copied().unwrap_or(0) as u32)
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    (buffer.get(offset).copied().unwrap_or(0) as u32)
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u32) << 8)
        + ((buffer.get(offset + 2).copied().unwrap_or(0) as u32) << 16)
        + (buffer.get(offset + 3).copied().unwrap_or(0) as u32) * 0x1000000
}

fn starts_with_bytes(buffer: &[u8], bytes: &[u8]) -> bool {
    buffer.len() >= bytes.len() && bytes.iter().zip(buffer.iter()).all(|(a, b)| a == b)
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    if buffer.len() < offset + text.len() {
        return false;
    }
    text.as_bytes()
        .iter()
        .enumerate()
        .all(|(i, b)| buffer[offset + i] == *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_png() -> Vec<u8> {
        // valid PNG signature + IHDR (length 13)
        let mut b = PNG_SIGNATURE.to_vec();
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&[0; 8]);
        b
    }

    fn animated_png() -> Vec<u8> {
        let mut b = PNG_SIGNATURE.to_vec();
        // acTL chunk first (length 8)
        b.extend_from_slice(&8u32.to_be_bytes());
        b.extend_from_slice(b"acTL");
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&[0u8; 4]); // CRC
        b
    }

    #[test]
    fn detects_jpeg() {
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        // 0xf7 trailing byte rejected
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]),
            None
        );
        // too short
        assert_eq!(detect_supported_image_mime_type(&[0xff, 0xd8]), None);
    }

    #[test]
    fn detects_png_and_rejects_animated() {
        let png = minimal_png();
        assert_eq!(detect_supported_image_mime_type(&png), Some("image/png"));
        let apng = animated_png();
        assert_eq!(detect_supported_image_mime_type(&apng), None);
    }

    #[test]
    fn detects_gif_webp_bmp() {
        assert_eq!(
            detect_supported_image_mime_type(b"GIF89a"),
            Some("image/gif")
        );
        assert_eq!(
            detect_supported_image_mime_type(b"RIFF1234WEBPVP8 "),
            Some("image/webp")
        );
        // valid BMP: declare size 54, pixel data at 54, DIB 40, planes 1, bpp 24
        let mut bmp = vec![0u8; 54 + 4];
        bmp[0] = b'B';
        bmp[1] = b'M';
        bmp[2..6].copy_from_slice(&(54u32 + 4).to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        assert_eq!(detect_supported_image_mime_type(&bmp), Some("image/bmp"));
        // garbage
        assert_eq!(detect_supported_image_mime_type(b"not an image"), None);
    }

    fn tiny_bmp() -> Vec<u8> {
        let mut buffer = vec![0u8; 58];
        buffer[0..2].copy_from_slice(b"BM");
        let buffer_len = buffer.len() as u32;
        buffer[2..6].copy_from_slice(&buffer_len.to_le_bytes());
        buffer[10..14].copy_from_slice(&54u32.to_le_bytes());
        buffer[14..18].copy_from_slice(&40u32.to_le_bytes());
        buffer[18..22].copy_from_slice(&1i32.to_le_bytes());
        buffer[22..26].copy_from_slice(&1i32.to_le_bytes());
        buffer[26..28].copy_from_slice(&1u16.to_le_bytes());
        buffer[28..30].copy_from_slice(&24u16.to_le_bytes());
        buffer[34..38].copy_from_slice(&4u32.to_le_bytes());
        buffer[56] = 0xff;
        buffer
    }

    #[test]
    fn processes_bmp_into_png_with_conversion_hint() {
        let result = process_image(
            &tiny_bmp(),
            "image/bmp",
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        )
        .expect("tiny BMP should convert");
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.original_width, 1);
        assert_eq!(result.original_height, 1);
        assert!(result
            .hints
            .contains(&"[Image converted from image/bmp to image/png.]".to_string()));
        let png = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, result.data)
            .expect("processed image is base64");
        assert!(png.starts_with(&PNG_SIGNATURE));
    }

    #[test]
    fn resizes_images_and_reports_coordinate_mapping() {
        let mut cursor = Cursor::new(Vec::new());
        let source = image::DynamicImage::new_rgb8(100, 50);
        source
            .write_to(&mut cursor, image::ImageFormat::Png)
            .expect("encode source image");
        let result = process_image(
            &cursor.into_inner(),
            "image/png",
            ProcessImageOptions {
                max_width: 50,
                max_height: 50,
                ..Default::default()
            },
        )
        .expect("image should resize");
        assert!(result.was_resized);
        assert_eq!((result.original_width, result.original_height), (100, 50));
        assert_eq!((result.width, result.height), (50, 25));
        assert!(result
            .hints
            .iter()
            .any(|hint| hint.contains("Multiply coordinates by 2.00")));
    }

    #[test]
    fn base64_encodes_like_standard() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        // against the standard crate encoding
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            encode_base64(&data),
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data)
        );
    }
}
