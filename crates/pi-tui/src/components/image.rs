//! Image component — port of `packages/tui/src/components/image.ts`.
//!
//! Renders base64-encoded images through the detected terminal image
//! protocol (Kitty / iTerm2) with the upstream text fallback when the
//! terminal has no image support.

use crate::terminal_image::{encode_iterm2, encode_kitty, get_capabilities, get_cell_dimensions, get_image_dimensions, image_fallback, ImageProtocol};
use crate::tui::Component;
use crate::utils::truncate_to_width;

/// Theme for the image fallback text.
pub struct ImageTheme {
    pub fallback_color: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl std::fmt::Debug for ImageTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageTheme").finish()
    }
}

pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    /// Kitty image ID; when provided, reuses this ID.
    pub image_id: Option<u32>,
}

#[derive(Default)]
pub struct ImageOptionsDefaultMarker;
impl Default for ImageOptions {
    fn default() -> Self {
        Self::default_impl()
    }
}

/// The Image component.
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: (u32, u32),
    theme: ImageTheme,
    options: ImageOptions,
    image_id: std::sync::Mutex<Option<u32>>,
    cached_lines: std::sync::Mutex<Option<Vec<String>>>,
    cached_width: std::sync::Mutex<Option<usize>>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image").field("mime", &self.mime_type).finish()
    }
}

impl ImageOptions {
    fn default_impl() -> Self {
        Self {
            max_width_cells: None,
            max_height_cells: None,
            filename: None,
            image_id: None,
        }
    }
}

impl Image {
    pub fn new(
        base64_data: impl Into<String>,
        mime_type: impl Into<String>,
        theme: ImageTheme,
        options: ImageOptions,
    ) -> Self {
        let base64_data = base64_data.into();
        let mime_type = mime_type.into();
        let dimensions = get_image_dimensions(&base64_data, &mime_type).unwrap_or((800, 600));
        let configured_id = options.image_id;
        Self {
            base64_data,
            mime_type,
            dimensions,
            theme,
            options,
            image_id: std::sync::Mutex::new(configured_id),
            cached_lines: std::sync::Mutex::new(None),
            cached_width: std::sync::Mutex::new(None),
        }
    }

    /// The Kitty image ID used by this image (if any).
    pub fn get_image_id(&self) -> Option<u32> {
        *self.image_id.lock().unwrap()
    }

    fn render_image(&self, width: usize) -> Vec<String> {
        let max_width = std::cmp::max(1, std::cmp::min(width.saturating_sub(2), self.options.max_width_cells.unwrap_or(60)));
        let cell = get_cell_dimensions();
        let default_max_height = std::cmp::max(1, (max_width * cell.0 as usize).div_ceil(cell.1 as usize));
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        let caps = get_capabilities();
        let mut lines: Vec<String> = Vec::new();

        if let Some(protocol) = caps.images {
            let mut effective_id = *self.image_id.lock().unwrap();
            let (image_w, image_h) = self.dimensions;
            // Scale to fit cell bounds preserving aspect ratio.
            let width_scale = (max_width as f64 * cell.0 as f64) / image_w as f64;
            let height_scale = (max_height as f64 * cell.1 as f64) / image_h as f64;
            let scale = width_scale.min(height_scale);
            let columns = ((image_w as f64 * scale) / cell.0 as f64).ceil().max(1.0).min(max_width as f64) as usize;
            let rows = ((image_h as f64 * scale) / cell.1 as f64).ceil().max(1.0).min(max_height as f64) as usize;

            if protocol == ImageProtocol::Kitty {
                if effective_id.is_none() {
                    effective_id = Some(allocate_image_id());
                    *self.image_id.lock().unwrap() = effective_id;
                }
                let sequence = encode_kitty(&self.base64_data, columns, rows, effective_id, false);
                lines.push(sequence);
                for _ in 0..rows.saturating_sub(1) {
                    lines.push(String::new());
                }
            } else {
                let sequence = encode_iterm2(&self.base64_data, columns, true);
                for _ in 0..rows.saturating_sub(1) {
                    lines.push(String::new());
                }
                let row_offset = rows.saturating_sub(1);
                let move_up = if row_offset > 0 { format!("\x1b[{row_offset}A") } else { String::new() };
                lines.push(format!("{move_up}{sequence}"));
            }
        } else {
            let fallback = image_fallback(&self.mime_type, Some(self.dimensions), self.options.filename.as_deref());
            lines.push(truncate_to_width(&(self.theme.fallback_color)(&fallback), width, ""));
        }

        lines
    }
}

// Random image id in [1, 0xffffffff].
fn allocate_image_id() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    ((secs ^ (nanos as u64)) as u32).max(1)
}

impl Component for Image {
    fn render(&self, width: usize) -> Vec<String> {
        {
            let cached = self.cached_lines.lock().unwrap().clone();
            let w = *self.cached_width.lock().unwrap();
            if let (Some(lines), Some(w)) = (cached, w) {
                if w == width {
                    return lines;
                }
            }
        }
        let lines = self.render_image(width);
        *self.cached_lines.lock().unwrap() = Some(lines.clone());
        *self.cached_width.lock().unwrap() = Some(width);
        lines
    }

    fn invalidate(&mut self) {
        *self.cached_lines.lock().unwrap() = None;
        *self.cached_width.lock().unwrap() = None;
    }
}
