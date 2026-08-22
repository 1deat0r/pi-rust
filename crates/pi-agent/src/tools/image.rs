//! Image MIME detection + base64 encoding — port of
//! `packages/agent/src/harness/tools/image.ts`.
//!
//! Determines the mime type a model can attach from raw file bytes (JPEG,
//! PNG — rejecting animated PNG, GIF, WebP, BMP) and encodes bytes to base64
//! with the upstream manual 3-byte-block algorithm (including the exact
//! padding behavior).

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// Detect the model-attachable image mime type from raw bytes. Returns
/// `None` for unsupported or invalid images (mirrors upstream:
/// JPEG with a trailing byte 0xf7 at index 3 is rejected, animated PNG is
/// rejected, malformed BMP is rejected).
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with_bytes(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) { None } else { Some("image/jpeg") };
    }
    if starts_with_bytes(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) { Some("image/png") } else { None };
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
        output.push(ALPHABET[(((first & 0x03) << 4) | ((second.unwrap_or(0)) >> 4)) as usize] as char);
        match second {
            Some(second) => {
                output.push(ALPHABET[(((second & 0x0f) << 2) | ((third.unwrap_or(0)) >> 6)) as usize] as char);
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
    text.as_bytes().iter().enumerate().all(|(i, b)| buffer[offset + i] == *b)
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
        assert_eq!(detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        // 0xf7 trailing byte rejected
        assert_eq!(detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]), None);
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
        assert_eq!(detect_supported_image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(detect_supported_image_mime_type(b"RIFF1234WEBPVP8 "), Some("image/webp"));
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
        assert_eq!(encode_base64(&data), base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data));
    }
}
