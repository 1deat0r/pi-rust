//! Image component — port of `packages/tui/src/components/image.ts`.
//!
//! Renders base64-encoded images through the detected terminal image
//! protocol (Kitty / iTerm2) with the upstream text fallback when the
//! terminal has no image support.

use crate::terminal_image::{
    allocate_image_id, get_capabilities, get_cell_dimensions, get_image_dimensions, image_fallback,
    render_image, ImageDimensions, ImageProtocol, ImageRenderOptions,
};
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
    cached_cell_dimensions: std::sync::Mutex<Option<(u32, u32)>>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("mime", &self.mime_type)
            .finish()
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
            cached_cell_dimensions: std::sync::Mutex::new(None),
        }
    }

    /// Override the decoded dimensions when the caller already has trusted
    /// metadata, matching upstream `Image`'s optional constructor dimensions.
    /// This is useful for formats whose bytes were already decoded elsewhere
    /// and keeps terminal cell placement deterministic without reparsing them.
    pub fn with_dimensions(mut self, dimensions: ImageDimensions) -> Self {
        self.dimensions = (dimensions.width_px, dimensions.height_px);
        self
    }

    /// The Kitty image ID used by this image (if any).
    pub fn get_image_id(&self) -> Option<u32> {
        *self
            .image_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn render_image(&self, width: usize) -> Vec<String> {
        let max_width = std::cmp::max(
            1,
            std::cmp::min(
                width.saturating_sub(2),
                self.options.max_width_cells.unwrap_or(60),
            ),
        );
        let cell = get_cell_dimensions();
        let default_max_height =
            std::cmp::max(1, (max_width * cell.0 as usize).div_ceil(cell.1 as usize));
        let max_height = self
            .options
            .max_height_cells
            .unwrap_or(default_max_height)
            .max(1);

        let caps = get_capabilities();
        let mut lines: Vec<String> = Vec::new();

        if let Some(protocol) = caps.images {
            let mut effective_id = *self
                .image_id
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if protocol == ImageProtocol::Kitty && effective_id.is_none() {
                effective_id = Some(allocate_image_id());
                *self
                    .image_id
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = effective_id;
            }
            let result = render_image(
                &self.base64_data,
                ImageDimensions {
                    width_px: self.dimensions.0,
                    height_px: self.dimensions.1,
                },
                ImageRenderOptions {
                    max_width_cells: Some(max_width),
                    max_height_cells: Some(max_height),
                    image_id: effective_id,
                    move_cursor: Some(false),
                    ..Default::default()
                },
            );

            if let Some(result) = result {
                if result.image_id.is_some() {
                    *self
                        .image_id
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = result.image_id;
                }
                if protocol == ImageProtocol::Kitty {
                    // C=1 prevents Kitty from moving the cursor. Return the
                    // occupied rows so the TUI accounts for the image height.
                    lines.push(result.sequence);
                    for _ in 0..result.rows.saturating_sub(1) {
                        lines.push(String::new());
                    }
                } else {
                    // iTerm2 draws on the last occupied row; move up before
                    // drawing so cursor accounting stays inside the region.
                    for _ in 0..result.rows.saturating_sub(1) {
                        lines.push(String::new());
                    }
                    let row_offset = result.rows.saturating_sub(1);
                    let move_up = if row_offset > 0 {
                        format!("\x1b[{row_offset}A")
                    } else {
                        String::new()
                    };
                    lines.push(format!("{move_up}{}", result.sequence));
                }
            } else {
                let fallback = image_fallback(
                    &self.mime_type,
                    Some(self.dimensions),
                    self.options.filename.as_deref(),
                );
                lines.push(truncate_to_width(
                    &(self.theme.fallback_color)(&fallback),
                    width,
                    "",
                ));
            }
        } else {
            let fallback = image_fallback(
                &self.mime_type,
                Some(self.dimensions),
                self.options.filename.as_deref(),
            );
            lines.push(truncate_to_width(
                &(self.theme.fallback_color)(&fallback),
                width,
                "",
            ));
        }

        lines
    }
}

impl Component for Image {
    fn render(&self, width: usize) -> Vec<String> {
        {
            let cached = self
                .cached_lines
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let w = *self
                .cached_width
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let cell_dimensions = *self
                .cached_cell_dimensions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let (Some(lines), Some(w), Some(cached_cell_dimensions)) =
                (cached, w, cell_dimensions)
            {
                if w == width
                    && cached_cell_dimensions == crate::terminal_image::get_cell_dimensions()
                {
                    return lines;
                }
            }
        }
        let lines = self.render_image(width);
        *self
            .cached_lines
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(lines.clone());
        *self
            .cached_width
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(width);
        *self
            .cached_cell_dimensions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(crate::terminal_image::get_cell_dimensions());
        lines
    }

    fn invalidate(&mut self) {
        *self
            .cached_lines
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        *self
            .cached_width
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        *self
            .cached_cell_dimensions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::terminal_image::{get_capabilities, set_capabilities, TerminalCapabilities};

    #[test]
    fn explicit_dimensions_override_fallback_metadata() {
        let previous = get_capabilities();
        set_capabilities(TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks: false,
        });

        let image = Image::new(
            "not-real-base64",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(str::to_owned),
            },
            ImageOptions::default(),
        )
        .with_dimensions(ImageDimensions {
            width_px: 3,
            height_px: 4,
        });

        let rendered = image.render(80);
        assert!(rendered[0].contains("3x4"));
        set_capabilities(previous);
    }
}
