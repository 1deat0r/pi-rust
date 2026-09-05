//! Image MIME detection + base64 encoding — port of
//! `packages/agent/src/harness/tools/image.ts`.
//!
//! Determines the mime type a model can attach from raw file bytes and
//! processes image payloads for provider limits. This ports the upstream
//! `harness/tools/image.ts` detector/encoder plus coding-agent's
//! `utils/image-process.ts` normalization and resize policy.

use std::io::Cursor;

use pi_ai::types::ContentBlock;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const IMAGE_CONVERSION_FAILURE: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";
const IMAGE_RESIZE_FAILURE: &str =
    "[Image omitted: could not be resized below the inline image size limit.]";

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
/// `None` for unsupported or obviously malformed signatures (mirrors the
/// upstream content sniffer: JPEG with a trailing byte 0xf7 at index 3 is
/// rejected, animated PNG is rejected, malformed BMP headers are rejected).
/// The sniffer deliberately does not fully decode every format; the real
/// decoder in [`process_image`] owns that validation boundary.
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
    let mut output = String::with_capacity(base64_encoded_len(bytes.len()));
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

/// Convert an encoded image to PNG after applying any EXIF orientation.
///
/// This is the Rust equivalent of coding-agent's
/// `convertImageBytesToPng`: decoding is content-based, so a stale MIME label
/// does not prevent conversion. `None` means the bytes are not a format the
/// configured image-crate features can decode or the PNG encoder failed.
pub fn convert_image_bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = decode_image(bytes, "").ok()?;
    let image = apply_exif_orientation(image, bytes);
    encode_png(&image).ok()
}

/// Convert base64 image data to PNG for terminal protocols that require PNG.
/// An exact `image/png` MIME type follows upstream and is returned without
/// decoding or rewriting the payload.
pub fn convert_to_png(base64_data: &str, mime_type: &str) -> Option<(String, String)> {
    if mime_type == "image/png" {
        return Some((base64_data.to_string(), mime_type.to_string()));
    }
    let bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data).ok()?;
    let png = convert_image_bytes_to_png(&bytes)?;
    Some((encode_base64(&png), "image/png".to_string()))
}

/// Normalize and, when enabled, resize an image using the same observable
/// policy as coding-agent's `processImage`.
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    options: ProcessImageOptions,
) -> Result<ProcessedImage, String> {
    let (normalized_bytes, normalized_mime, converted_from) =
        normalize_image(bytes, mime_type).map_err(|_| IMAGE_CONVERSION_FAILURE.to_string())?;
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
            // Conversion is normalization, not a resize. This is observable
            // through the upstream dimension-note policy.
            was_resized: false,
        });
    }

    if options.max_width == 0 || options.max_height == 0 {
        return Err(IMAGE_RESIZE_FAILURE.to_string());
    }

    let image = decode_image(&normalized_bytes, &normalized_mime)
        .map(|image| apply_exif_orientation(image, &normalized_bytes))
        .map_err(|_| IMAGE_RESIZE_FAILURE.to_string())?;
    let (original_width, original_height) = (image.width(), image.height());
    let input_base64_size = base64_encoded_len(normalized_bytes.len());

    if original_width <= options.max_width
        && original_height <= options.max_height
        && input_base64_size < options.max_base64_bytes
    {
        let hints = conversion_hint.into_iter().collect();
        return Ok(ProcessedImage {
            data: encode_base64(&normalized_bytes),
            mime_type: normalized_mime,
            hints,
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
        let candidates = encode_candidates(&resized, &quality_steps)
            .map_err(|_| IMAGE_RESIZE_FAILURE.to_string())?;
        for (data, mime_type) in candidates {
            if data.len() < options.max_base64_bytes {
                let mut hints = Vec::new();
                if let Some(hint) = conversion_hint.as_deref() {
                    hints.push(hint.to_string());
                }
                // Upstream formats this note whenever the resize path was
                // taken, including a byte-limit-only re-encode at 1.00x.
                let scale = original_width as f64 / width as f64;
                hints.push(format!(
                    "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {scale:.2} to map to original image.]"
                ));
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
            return Err(IMAGE_RESIZE_FAILURE.to_string());
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
            return Err(IMAGE_RESIZE_FAILURE.to_string());
        }
        width = next_width;
        height = next_height;
    }
}

/// Normalize every image in a finalized tool result after extension hooks
/// have had a chance to replace or add content. Images that cannot be decoded
/// or normalized are retained unchanged, matching coding-agent's
/// `normalizeToolResultImages` best-effort contract. Conversion/resize hints
/// are inserted immediately after the image that produced them.
pub fn normalize_tool_result_images(
    content: &[ContentBlock],
    options: ProcessImageOptions,
) -> Vec<ContentBlock> {
    let mut normalized = Vec::with_capacity(content.len());
    for block in content {
        let ContentBlock::Image { data, mime_type } = block else {
            normalized.push(block.clone());
            continue;
        };
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
        else {
            normalized.push(block.clone());
            continue;
        };
        let Ok(processed) = process_image(&bytes, mime_type, options) else {
            normalized.push(block.clone());
            continue;
        };
        normalized.push(ContentBlock::Image {
            data: processed.data,
            mime_type: processed.mime_type,
        });
        if !processed.hints.is_empty() {
            normalized.push(ContentBlock::text(processed.hints.join("\n")));
        }
    }
    normalized
}

fn normalize_image(
    bytes: &[u8],
    mime_type: &str,
) -> Result<(Vec<u8>, String, Option<String>), String> {
    let base_mime = base_mime_type(mime_type);
    let normalized_mime = match base_mime.as_str() {
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

    // Photon decodes by the bytes for conversion, so a valid image received
    // with a generic or stale MIME label is still normalized. The public
    // conversion helper uses the same content-based image-crate fallback.
    let png = convert_image_bytes_to_png(bytes)
        .ok_or_else(|| "could not convert image bytes to PNG".to_string())?;
    Ok((png, "image/png".to_string(), Some(base_mime)))
}

fn decode_image(bytes: &[u8], mime_type: &str) -> Result<image::DynamicImage, String> {
    let base_mime = base_mime_type(mime_type);
    let declared_format = image_format_for_mime(&base_mime);
    let guessed_format = image::guess_format(bytes).ok();
    let mut formats = Vec::with_capacity(2);
    if let Some(format) = declared_format {
        formats.push(format);
    }
    if let Some(format) = guessed_format {
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    if formats.is_empty() {
        let error = image::guess_format(bytes)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no supported content signature".to_string());
        return Err(format!("unsupported image format {base_mime}: {error}"));
    }

    let mut last_error = None;
    for format in formats {
        // The image crate's WebP decoder currently rejects the otherwise
        // valid ancillary EXIF chunk that Pi uses for orientation. Strip only
        // that metadata for decoding; pass-through results still retain the
        // original bytes and the orientation parser still reads the original
        // payload. Trying the declared format first and the sniffed format
        // second matches Photon's content-based conversion behavior even when
        // a caller supplies a stale MIME label.
        let sanitized_webp = (format == image::ImageFormat::WebP)
            .then(|| strip_webp_exif_chunks(bytes))
            .flatten();
        let decode_bytes = sanitized_webp.as_deref().unwrap_or(bytes);
        match image::load_from_memory_with_format(decode_bytes, format) {
            Ok(image) => return Ok(image),
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    Err(format!(
        "could not decode image: {}",
        last_error.unwrap_or_else(|| "unknown decoder error".to_string())
    ))
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
}

fn image_format_for_mime(mime_type: &str) -> Option<image::ImageFormat> {
    match mime_type {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(image::ImageFormat::Jpeg),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        "image/bmp" => Some(image::ImageFormat::Bmp),
        _ => None,
    }
}

fn strip_webp_exif_chunks(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }

    let mut offset = 12usize;
    let mut removed = false;
    let mut chunks = Vec::with_capacity(bytes.len());
    while offset + 8 <= bytes.len() {
        let size = usize::try_from(u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]))
        .ok()?;
        let data_start = offset.checked_add(8)?;
        let data_end = data_start.checked_add(size)?;
        let next_offset = data_end.checked_add(size % 2)?;
        if next_offset > bytes.len() {
            return None;
        }

        if &bytes[offset..offset + 4] == b"EXIF" {
            removed = true;
        } else {
            chunks.extend_from_slice(&bytes[offset..next_offset]);
        }
        offset = next_offset;
    }
    if !removed {
        return None;
    }

    let riff_size = u32::try_from(4usize.checked_add(chunks.len())?).ok()?;
    let mut output = Vec::with_capacity(12 + chunks.len());
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&riff_size.to_le_bytes());
    output.extend_from_slice(b"WEBP");
    output.extend_from_slice(&chunks);
    Some(output)
}

/// Read the EXIF orientation tag from JPEG APP1 or WebP EXIF metadata.
///
/// Image decoders generally preserve the encoded pixel matrix and leave
/// orientation to the caller.  Pi applies the tag before measuring or
/// resizing an image, so the model receives dimensions in the same visual
/// coordinate system as the user.  Malformed or absent EXIF is deliberately
/// treated as orientation 1.
fn exif_orientation(bytes: &[u8]) -> u16 {
    let tiff_start = if bytes.starts_with(&[0xff, 0xd8]) {
        find_jpeg_tiff_offset(bytes)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        find_webp_tiff_offset(bytes)
    } else {
        None
    };
    let Some(tiff_start) = tiff_start else {
        return 1;
    };
    read_tiff_orientation(bytes, tiff_start).unwrap_or(1)
}

fn find_jpeg_tiff_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 2 || bytes[..2] != [0xff, 0xd8] {
        return None;
    }
    let mut offset = 2usize;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        // Standalone markers do not carry a segment length. SOS/EOI ends the
        // metadata walk; EXIF may only occur before those markers.
        if marker == 0xd8 || marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = usize::from(u16::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
        ]));
        let segment_end = offset.checked_add(length)?;
        if length < 2 || segment_end > bytes.len() {
            return None;
        }
        let data_start = offset + 2;
        if marker == 0xe1
            && bytes.get(data_start..data_start.saturating_add(6)) == Some(b"Exif\0\0")
        {
            return Some(data_start + 6);
        }
        offset = segment_end;
    }
    None
}

fn find_webp_tiff_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let size = usize::try_from(u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]))
        .ok()?;
        let data_start = offset.checked_add(8)?;
        let data_end = data_start.checked_add(size)?;
        if data_end > bytes.len() {
            return None;
        }
        if chunk_id == b"EXIF" {
            return if size >= 6 && &bytes[data_start..data_start + 6] == b"Exif\0\0" {
                Some(data_start + 6)
            } else {
                Some(data_start)
            };
        }
        offset = data_end.checked_add(size % 2)?;
    }
    None
}

fn read_tiff_orientation(bytes: &[u8], tiff_start: usize) -> Option<u16> {
    let order_end = tiff_start.checked_add(2)?;
    let order = bytes.get(tiff_start..order_end)?;
    let little_endian = order == b"II";
    if !little_endian && order != b"MM" {
        return None;
    }
    let read_u16 = |offset: usize| -> Option<u16> {
        let value = bytes.get(offset..offset.checked_add(2)?)?;
        Some(if little_endian {
            u16::from_le_bytes([value[0], value[1]])
        } else {
            u16::from_be_bytes([value[0], value[1]])
        })
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        let value = bytes.get(offset..offset.checked_add(4)?)?;
        Some(if little_endian {
            u32::from_le_bytes([value[0], value[1], value[2], value[3]])
        } else {
            u32::from_be_bytes([value[0], value[1], value[2], value[3]])
        })
    };
    if read_u16(tiff_start.checked_add(2)?)? != 42 {
        return None;
    }
    let ifd_offset = usize::try_from(read_u32(tiff_start.checked_add(4)?)?).ok()?;
    let ifd_start = tiff_start.checked_add(ifd_offset)?;
    let entries = usize::from(read_u16(ifd_start)?);
    for index in 0..entries {
        let entry_offset = 2usize.checked_add(index.checked_mul(12)?)?;
        let entry = ifd_start.checked_add(entry_offset)?;
        if read_u16(entry)? != 0x0112 {
            continue;
        }
        // Orientation is a SHORT (type 3), count 1.  Reading the inline
        // value is sufficient and avoids trusting an arbitrary offset.
        if read_u16(entry.checked_add(2)?)? != 3 || read_u32(entry.checked_add(4)?)? != 1 {
            return None;
        }
        let orientation = read_u16(entry.checked_add(8)?)?;
        return (1..=8).contains(&orientation).then_some(orientation);
    }
    None
}

fn apply_exif_orientation(image: image::DynamicImage, bytes: &[u8]) -> image::DynamicImage {
    match exif_orientation(bytes) {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn image_dimensions(bytes: &[u8], mime_type: &str) -> Result<(u32, u32), String> {
    let image = apply_exif_orientation(decode_image(bytes, mime_type)?, bytes);
    Ok((image.width(), image.height()))
}

fn base64_encoded_len(byte_len: usize) -> usize {
    byte_len.div_ceil(3).saturating_mul(4)
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
        width = max_width;
    }
    if height > max_height {
        width = ((width as f64 * max_height as f64 / height as f64).round() as u32).max(1);
        height = max_height;
    }
    (width.max(1), height.max(1))
}

fn unique_quality_steps(primary: u8) -> Vec<u8> {
    [primary, 85, 70, 55, 40]
        .into_iter()
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
    let minimum_pixel_data_offset = 14u32.saturating_add(dib_header_size);
    if pixel_data_offset < minimum_pixel_data_offset {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageBuffer, Rgb, RgbImage};

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
    fn normalizes_tool_result_images_in_place_with_adjacent_hints() {
        let original = vec![
            ContentBlock::text("before"),
            ContentBlock::Image {
                data: encode_base64(&tiny_bmp()),
                mime_type: "image/bmp".to_string(),
            },
            ContentBlock::text("after"),
        ];
        let normalized = normalize_tool_result_images(
            &original,
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        );
        assert_eq!(normalized.len(), 4);
        assert!(matches!(
            &normalized[0],
            ContentBlock::Text { text, .. } if text == "before"
        ));
        assert!(matches!(
            &normalized[1],
            ContentBlock::Image { mime_type, data }
                if mime_type == "image/png"
                    && base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        data
                    )
                    .is_ok_and(|bytes| bytes.starts_with(&PNG_SIGNATURE))
        ));
        assert!(matches!(
            &normalized[2],
            ContentBlock::Text { text, .. }
                if text == "[Image converted from image/bmp to image/png.]"
        ));
        assert!(matches!(
            &normalized[3],
            ContentBlock::Text { text, .. } if text == "after"
        ));
    }

    #[test]
    fn failed_tool_result_image_normalization_retains_the_original_block() {
        let original = vec![ContentBlock::Image {
            data: "not-base64".to_string(),
            mime_type: "image/png".to_string(),
        }];
        assert_eq!(
            normalize_tool_result_images(&original, ProcessImageOptions::default()),
            original
        );

        let undecodable = vec![ContentBlock::Image {
            data: encode_base64(b"not-an-image"),
            mime_type: "image/png".to_string(),
        }];
        assert_eq!(
            normalize_tool_result_images(&undecodable, ProcessImageOptions::default()),
            undecodable
        );
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

    fn encode_test_image(image: &image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, format)
            .expect("test image should encode");
        cursor.into_inner()
    }

    fn pattern_image() -> image::DynamicImage {
        let values = [[10u8, 20, 30], [40, 50, 60]];
        let mut image = RgbImage::new(3, 2);
        for (y, row) in values.iter().enumerate() {
            for (x, value) in row.iter().enumerate() {
                image.put_pixel(
                    x as u32,
                    y as u32,
                    Rgb([*value, value.saturating_add(1), value.saturating_add(2)]),
                );
            }
        }
        image::DynamicImage::ImageRgb8(image)
    }

    fn noisy_image(width: u32, height: u32) -> image::DynamicImage {
        let mut state = 0x9e37_79b9u32;
        let mut image = ImageBuffer::new(width, height);
        for pixel in image.pixels_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *pixel = Rgb([
                (state & 0xff) as u8,
                ((state >> 8) & 0xff) as u8,
                ((state >> 16) & 0xff) as u8,
            ]);
        }
        image::DynamicImage::ImageRgb8(image)
    }

    fn exif_tiff_with_endianness(orientation: u16, little_endian: bool) -> Vec<u8> {
        let mut tiff = Vec::with_capacity(26);
        let order = if little_endian { b"II" } else { b"MM" };
        tiff.extend_from_slice(order);
        if little_endian {
            tiff.extend_from_slice(&42u16.to_le_bytes());
            tiff.extend_from_slice(&8u32.to_le_bytes());
            tiff.extend_from_slice(&1u16.to_le_bytes());
            tiff.extend_from_slice(&0x0112u16.to_le_bytes());
            tiff.extend_from_slice(&3u16.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
            tiff.extend_from_slice(&0u32.to_le_bytes());
        } else {
            tiff.extend_from_slice(&42u16.to_be_bytes());
            tiff.extend_from_slice(&8u32.to_be_bytes());
            tiff.extend_from_slice(&1u16.to_be_bytes());
            tiff.extend_from_slice(&0x0112u16.to_be_bytes());
            tiff.extend_from_slice(&3u16.to_be_bytes());
            tiff.extend_from_slice(&1u32.to_be_bytes());
            tiff.extend_from_slice(&orientation.to_be_bytes());
            tiff.extend_from_slice(&0u16.to_be_bytes());
            tiff.extend_from_slice(&0u32.to_be_bytes());
        }
        tiff
    }

    fn exif_tiff(orientation: u16) -> Vec<u8> {
        exif_tiff_with_endianness(orientation, true)
    }

    fn jpeg_with_exif_payload(payload: &[u8]) -> Vec<u8> {
        let jpeg = encode_test_image(&pattern_image(), image::ImageFormat::Jpeg);
        let segment_length = u16::try_from(payload.len() + 2).expect("test EXIF fits APP1");
        let mut output = Vec::with_capacity(jpeg.len() + payload.len() + 4);
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&segment_length.to_be_bytes());
        output.extend_from_slice(payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn jpeg_with_orientation(orientation: u16) -> Vec<u8> {
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&exif_tiff(orientation));
        jpeg_with_exif_payload(&payload)
    }

    fn webp_with_orientation(orientation: u16) -> Vec<u8> {
        let webp = encode_test_image(&pattern_image(), image::ImageFormat::WebP);
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&exif_tiff(orientation));
        let chunk_size = payload.len() + (payload.len() % 2);
        let added_riff_size = 8 + chunk_size;
        let old_riff_size = u32::from_le_bytes(webp[4..8].try_into().expect("RIFF size"));

        let mut output = Vec::with_capacity(webp.len() + added_riff_size);
        output.extend_from_slice(b"RIFF");
        output.extend_from_slice(
            &old_riff_size
                .checked_add(u32::try_from(added_riff_size).expect("test chunk fits RIFF"))
                .expect("RIFF size does not overflow")
                .to_le_bytes(),
        );
        output.extend_from_slice(b"WEBP");
        output.extend_from_slice(b"EXIF");
        output.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("test EXIF fits chunk")
                .to_le_bytes(),
        );
        output.extend_from_slice(&payload);
        if payload.len() % 2 != 0 {
            output.push(0);
        }
        output.extend_from_slice(&webp[12..]);
        output
    }

    fn first_channels(image: &image::DynamicImage) -> (u32, u32, Vec<u8>) {
        (
            image.width(),
            image.height(),
            image.to_rgb8().pixels().map(|pixel| pixel[0]).collect(),
        )
    }

    #[test]
    fn processes_real_png_jpeg_and_webp_bytes_without_reencoding_when_safe() {
        let source = pattern_image();
        let cases = [
            (image::ImageFormat::Png, "image/png"),
            (image::ImageFormat::Jpeg, "image/jpeg"),
            (image::ImageFormat::WebP, "image/webp"),
        ];

        for (format, mime_type) in cases {
            let bytes = encode_test_image(&source, format);
            assert_eq!(
                detect_supported_image_mime_type(&bytes),
                Some(mime_type),
                "content sniffer for {mime_type}"
            );
            let result = process_image(
                &bytes,
                mime_type,
                ProcessImageOptions {
                    max_width: 10,
                    max_height: 10,
                    max_base64_bytes: base64_encoded_len(bytes.len()) + 1,
                    ..Default::default()
                },
            )
            .expect("safe encoded image should pass through");
            assert_eq!(result.data, encode_base64(&bytes));
            assert_eq!(result.mime_type, mime_type);
            assert_eq!((result.original_width, result.original_height), (3, 2));
            assert_eq!((result.width, result.height), (3, 2));
            assert!(!result.was_resized);
            assert!(result.hints.is_empty());

            let no_resize_result = process_image(
                &bytes,
                mime_type,
                ProcessImageOptions {
                    auto_resize_images: false,
                    ..Default::default()
                },
            )
            .expect("no-resize mode should pass through supported bytes");
            assert_eq!(no_resize_result.data, encode_base64(&bytes));
            assert_eq!(no_resize_result.mime_type, mime_type);
            assert!(!no_resize_result.was_resized);
            assert!(no_resize_result.hints.is_empty());
        }
    }

    #[test]
    fn convert_to_png_matches_terminal_conversion_contract() {
        let png = encode_test_image(&pattern_image(), image::ImageFormat::Png);
        let png_base64 = encode_base64(&png);
        assert_eq!(
            convert_to_png(&png_base64, "image/png"),
            Some((png_base64.clone(), "image/png".to_string()))
        );

        let jpeg = jpeg_with_orientation(6);
        let converted = convert_to_png(&encode_base64(&jpeg), "image/jpeg")
            .expect("valid JPEG should convert to PNG");
        assert_eq!(converted.1, "image/png");
        let converted_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, converted.0)
                .expect("converted data should be base64");
        assert_eq!(
            decode_image(&converted_bytes, "image/png")
                .expect("converted data should be PNG")
                .dimensions(),
            (2, 3)
        );
        assert!(convert_to_png("not-base64", "image/jpeg").is_none());
        assert!(convert_image_bytes_to_png(b"not-an-image").is_none());
    }

    #[test]
    fn processes_real_bmp_bytes_as_png_in_both_resize_modes() {
        let bytes = encode_test_image(&pattern_image(), image::ImageFormat::Bmp);
        assert_eq!(detect_supported_image_mime_type(&bytes), Some("image/bmp"));

        let without_resize = process_image(
            &bytes,
            "image/bmp",
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        )
        .expect("valid BMP should convert");
        let with_resize = process_image(&bytes, "image/bmp", ProcessImageOptions::default())
            .expect("small valid BMP should normalize");

        assert_eq!(without_resize.mime_type, "image/png");
        assert_eq!(with_resize.mime_type, "image/png");
        assert_eq!(without_resize.data, with_resize.data);
        assert!(!with_resize.was_resized);
        assert_eq!(
            (with_resize.original_width, with_resize.original_height),
            (3, 2)
        );
        assert_eq!(
            with_resize.hints,
            vec!["[Image converted from image/bmp to image/png.]".to_string()]
        );
        let converted = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            without_resize.data,
        )
        .expect("converted PNG should be base64");
        assert_eq!(
            decode_image(&converted, "image/png")
                .expect("converted PNG should decode")
                .dimensions(),
            (3, 2)
        );
    }

    #[test]
    fn unsupported_mime_uses_encoded_content_and_normalizes_hint() {
        let bytes = jpeg_with_orientation(6);
        let result = process_image(
            &bytes,
            "application/octet-stream; charset=binary",
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        )
        .expect("encoded JPEG should be convertible despite generic MIME");

        assert_eq!(result.mime_type, "image/png");
        assert_eq!(
            result.hints,
            vec!["[Image converted from application/octet-stream to image/png.]".to_string()]
        );
        assert_eq!((result.original_width, result.original_height), (2, 3));
        let png = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, result.data)
            .expect("normalized PNG should be base64");
        assert_eq!(
            decode_image(&png, "image/png")
                .expect("normalized PNG should decode")
                .dimensions(),
            (2, 3)
        );
    }

    #[test]
    fn stale_declared_mime_does_not_prevent_content_based_conversion() {
        let bytes = encode_test_image(&pattern_image(), image::ImageFormat::Png);
        let result = process_image(
            &bytes,
            "image/bmp",
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        )
        .expect("conversion should sniff the actual encoded format");

        assert_eq!(result.mime_type, "image/png");
        assert_eq!(
            result.hints,
            vec!["[Image converted from image/bmp to image/png.]".to_string()]
        );
        assert_eq!((result.original_width, result.original_height), (3, 2));
    }

    #[test]
    fn auto_resize_uses_content_when_declared_supported_mime_is_stale() {
        let bytes = encode_test_image(&pattern_image(), image::ImageFormat::Png);
        let result = process_image(
            &bytes,
            "image/jpeg",
            ProcessImageOptions {
                max_width: 10,
                max_height: 10,
                max_base64_bytes: base64_encoded_len(bytes.len()) + 1,
                ..Default::default()
            },
        )
        .expect("resize path should decode the actual content");

        assert_eq!(result.data, encode_base64(&bytes));
        assert_eq!(result.mime_type, "image/jpeg");
        assert_eq!((result.original_width, result.original_height), (3, 2));
        assert!(!result.was_resized);
    }

    #[test]
    fn keeps_caller_bytes_unchanged_when_resizing() {
        let source = encode_test_image(&noisy_image(80, 60), image::ImageFormat::Png);
        let original = source.clone();
        let result = process_image(
            &source,
            "image/png",
            ProcessImageOptions {
                max_width: 20,
                max_height: 20,
                ..Default::default()
            },
        )
        .expect("oversized image should resize");

        assert_eq!(source, original);
        assert!(result.was_resized);
        assert!(result.data.len() < DEFAULT_MAX_BASE64_BYTES);
        assert_eq!((result.width, result.height), (20, 15));
    }

    #[test]
    fn byte_limit_only_resize_emits_the_upstream_unit_scale_note() {
        let source = encode_test_image(&noisy_image(64, 64), image::ImageFormat::Png);
        let decoded = decode_image(&source, "image/png").expect("generated PNG should decode");
        let input_size = base64_encoded_len(source.len());
        let candidate = encode_candidates(&decoded, &unique_quality_steps(100))
            .expect("generated image should encode")
            .into_iter()
            .find(|(data, _)| data.len() < input_size)
            .expect("no encoded candidate was smaller than the source");
        let max_base64_bytes = candidate.0.len() + 1;
        assert!(input_size >= max_base64_bytes);

        let result = process_image(
            &source,
            "image/png",
            ProcessImageOptions {
                max_width: 64,
                max_height: 64,
                max_base64_bytes,
                jpeg_quality: 100,
                ..Default::default()
            },
        )
        .expect("same-dimension candidate should satisfy the byte limit");

        assert!(result.was_resized);
        assert_eq!((result.original_width, result.original_height), (64, 64));
        assert_eq!((result.width, result.height), (64, 64));
        assert!(result.hints.iter().any(|hint| {
            hint == "[Image: original 64x64, displayed at 64x64. Multiply coordinates by 1.00 to map to original image.]"
        }));
        assert!(result.data.len() < max_base64_bytes);
    }

    #[test]
    fn jpeg_quality_fallback_uses_the_first_quality_that_fits() {
        let source_image = noisy_image(64, 64);
        let source = encode_test_image(&source_image, image::ImageFormat::Png);
        let decoded = decode_image(&source, "image/png").expect("generated PNG should decode");
        let candidates = encode_candidates(&decoded, &unique_quality_steps(100))
            .expect("generated image should encode");
        let (expected_data, expected_mime, max_base64_bytes) = candidates
            .iter()
            .enumerate()
            .skip(2)
            .find_map(|(index, (data, mime_type))| {
                let limit = data.len() + 1;
                let all_previous_rejected = candidates[..index]
                    .iter()
                    .all(|(previous, _)| previous.len() >= limit);
                (all_previous_rejected && mime_type == "image/jpeg")
                    .then(|| (data.clone(), mime_type.clone(), limit))
            })
            .expect("generated image did not provide a decreasing JPEG quality sequence");

        let result = process_image(
            &source,
            "image/png",
            ProcessImageOptions {
                max_width: 64,
                max_height: 64,
                max_base64_bytes,
                jpeg_quality: 100,
                ..Default::default()
            },
        )
        .expect("a lower JPEG quality should satisfy the limit");

        assert_eq!(result.data, expected_data);
        assert_eq!(result.mime_type, expected_mime);
        assert!(result.data.len() < max_base64_bytes);
    }

    #[test]
    fn applies_all_jpeg_exif_orientations() {
        let source = pattern_image();
        let expected = [
            (3, 2, vec![10, 20, 30, 40, 50, 60]),
            (3, 2, vec![30, 20, 10, 60, 50, 40]),
            (3, 2, vec![60, 50, 40, 30, 20, 10]),
            (3, 2, vec![40, 50, 60, 10, 20, 30]),
            (2, 3, vec![10, 40, 20, 50, 30, 60]),
            (2, 3, vec![40, 10, 50, 20, 60, 30]),
            (2, 3, vec![60, 30, 50, 20, 40, 10]),
            (2, 3, vec![30, 60, 20, 50, 10, 40]),
        ];

        for (orientation, (width, height, channels)) in expected.into_iter().enumerate() {
            let bytes = jpeg_with_orientation((orientation + 1) as u16);
            assert_eq!(exif_orientation(&bytes), (orientation + 1) as u16);
            let oriented = apply_exif_orientation(source.clone(), &bytes);
            assert_eq!(first_channels(&oriented), (width, height, channels));
        }
    }

    #[test]
    fn reads_webp_exif_orientation_and_keeps_visual_dimensions() {
        let bytes = webp_with_orientation(6);
        assert_eq!(detect_supported_image_mime_type(&bytes), Some("image/webp"));
        assert_eq!(exif_orientation(&bytes), 6);
        let decoded = decode_image(&bytes, "image/webp").expect("EXIF WebP should decode");
        assert_eq!(decoded.dimensions(), (3, 2));

        let result = process_image(
            &bytes,
            "image/webp",
            ProcessImageOptions {
                max_width: 3,
                max_height: 3,
                max_base64_bytes: base64_encoded_len(bytes.len()) + 1,
                ..Default::default()
            },
        )
        .expect("safe WebP should pass through");
        assert_eq!((result.original_width, result.original_height), (2, 3));
        assert_eq!((result.width, result.height), (2, 3));
        assert!(!result.was_resized);
        assert_eq!(result.data, encode_base64(&bytes));

        for orientation in 1..=8 {
            assert_eq!(
                exif_orientation(&webp_with_orientation(orientation)),
                orientation
            );
        }
    }

    #[test]
    fn applies_jpeg_exif_before_resize_dimension_calculation() {
        let bytes = jpeg_with_orientation(6);
        let result = process_image(
            &bytes,
            "image/jpeg",
            ProcessImageOptions {
                max_width: 1,
                max_height: 3,
                ..Default::default()
            },
        )
        .expect("oriented JPEG should resize");

        assert_eq!((result.original_width, result.original_height), (2, 3));
        assert_eq!((result.width, result.height), (1, 2));
        assert!(result.was_resized);
        let output =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, result.data)
                .expect("resized JPEG output should be base64");
        assert_eq!(
            decode_image(&output, "image/png")
                .or_else(|_| decode_image(&output, "image/jpeg"))
                .expect("resized output should decode")
                .dimensions(),
            (1, 2)
        );
    }

    #[test]
    fn malformed_exif_falls_back_to_normal_orientation_without_panicking() {
        let mut bad_payload = b"Exif\0\0".to_vec();
        bad_payload.extend_from_slice(b"II\x2a\0\xff");
        let jpeg = jpeg_with_exif_payload(&bad_payload);
        assert_eq!(exif_orientation(&jpeg), 1);
        assert_eq!(
            first_channels(&apply_exif_orientation(pattern_image(), &jpeg)),
            first_channels(&pattern_image())
        );

        for bytes in [
            Vec::new(),
            vec![0xff, 0xd8, 0xff, 0xe1, 0xff],
            b"RIFF\0\0\0\0WEBP EXIF".to_vec(),
        ] {
            let result = std::panic::catch_unwind(|| exif_orientation(&bytes));
            assert!(result.is_ok(), "malformed metadata must not panic");
            assert_eq!(result.expect("checked above"), 1);
        }
    }

    #[test]
    fn malformed_image_headers_are_rejected_without_panicking() {
        let mut malformed_bmp = vec![0u8; 30];
        malformed_bmp[0..2].copy_from_slice(b"BM");
        malformed_bmp[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(detect_supported_image_mime_type(&malformed_bmp), None);

        let mut malformed_png = PNG_SIGNATURE.to_vec();
        malformed_png.extend_from_slice(&u32::MAX.to_be_bytes());
        malformed_png.extend_from_slice(b"zzzz");
        assert_eq!(detect_supported_image_mime_type(&malformed_png), None);
    }

    #[test]
    fn reads_big_endian_jpeg_exif_orientation() {
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&exif_tiff_with_endianness(8, false));
        let bytes = jpeg_with_exif_payload(&payload);
        assert_eq!(exif_orientation(&bytes), 8);
        assert_eq!(
            first_channels(&apply_exif_orientation(pattern_image(), &bytes)),
            (2, 3, vec![30, 60, 20, 50, 10, 40])
        );
    }

    #[test]
    fn malformed_signature_corpus_is_total() {
        for length in 0..=512 {
            let mut bytes = vec![0u8; length];
            if length >= 4 {
                bytes[..4].copy_from_slice(b"RIFF");
            }
            if length >= 12 {
                bytes[8..12].copy_from_slice(b"WEBP");
            }
            assert!(
                std::panic::catch_unwind(|| detect_supported_image_mime_type(&bytes)).is_ok(),
                "image detector panicked for {length}-byte input"
            );
            assert!(
                std::panic::catch_unwind(|| exif_orientation(&bytes)).is_ok(),
                "EXIF parser panicked for {length}-byte input"
            );
        }
    }

    #[test]
    fn invalid_and_unsupported_inputs_follow_process_failure_contract() {
        let invalid = b"not an image";
        let pass_through = process_image(
            invalid,
            "image/png",
            ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            },
        )
        .expect("upstream disables decoding when auto-resize is off");
        assert_eq!(pass_through.data, encode_base64(invalid));
        assert_eq!(pass_through.mime_type, "image/png");
        assert_eq!((pass_through.width, pass_through.height), (0, 0));

        assert_eq!(
            process_image(invalid, "image/png", ProcessImageOptions::default()),
            Err(IMAGE_RESIZE_FAILURE.to_string())
        );
        assert_eq!(
            process_image(
                invalid,
                "image/bmp",
                ProcessImageOptions {
                    auto_resize_images: false,
                    ..Default::default()
                }
            ),
            Err(IMAGE_CONVERSION_FAILURE.to_string())
        );
        assert_eq!(
            process_image(
                &encode_test_image(&pattern_image(), image::ImageFormat::Png),
                "image/png",
                ProcessImageOptions {
                    max_width: 0,
                    ..Default::default()
                }
            ),
            Err(IMAGE_RESIZE_FAILURE.to_string())
        );
    }

    #[test]
    fn zero_byte_limit_cannot_produce_an_attachment() {
        let source = encode_test_image(&pattern_image(), image::ImageFormat::Png);
        assert_eq!(
            process_image(
                &source,
                "image/png",
                ProcessImageOptions {
                    max_base64_bytes: 0,
                    ..Default::default()
                }
            ),
            Err(IMAGE_RESIZE_FAILURE.to_string())
        );
    }
}
