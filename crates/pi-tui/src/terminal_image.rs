//! Terminal image capabilities — port of `packages/tui/src/terminal-image.ts`
//! (the parts the markdown renderer and Image component use).

use std::process::{Command, Stdio};
use std::sync::{Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// Image protocol support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

/// Terminal capabilities for images, truecolor, hyperlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

/// Measured terminal cell dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// Pixel dimensions of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// Cell dimensions occupied by a rendered image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: usize,
    pub rows: usize,
}

/// Options shared by Kitty and iTerm2 image rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub preserve_aspect_ratio: Option<bool>,
    pub image_id: Option<u32>,
    /// Kitty's default is to move the cursor after placement.
    pub move_cursor: Option<bool>,
}

/// Optional iTerm2 image-file parameters.
///
/// The legacy [`encode_iterm2`] helper keeps its original Rust signature for
/// existing callers.  This options form exposes the complete upstream
/// encoder surface, including non-inline transfers and the optional filename
/// parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ITerm2ImageOptions {
    pub width: Option<String>,
    pub height: Option<String>,
    pub name: Option<String>,
    pub preserve_aspect_ratio: bool,
    pub inline: bool,
}

impl Default for ITerm2ImageOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            name: None,
            preserve_aspect_ratio: true,
            inline: true,
        }
    }
}

impl ITerm2ImageOptions {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Result of rendering an image through a supported terminal protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    pub sequence: String,
    pub columns: usize,
    pub rows: usize,
    pub image_id: Option<u32>,
}

/// Compatibility name matching the upstream render result terminology.
pub type ImageRenderResult = RenderedImage;

/// Metadata retained for Kitty placement-only redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: usize,
    pub rows: usize,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredKittyImageMetadata {
    metadata: KittyImageMetadata,
    transmission_generation: u64,
}

/// A placement-only Kitty command plus accounting for the source transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: u64,
    pub sequence: String,
    pub replacement_line: String,
}

static CAPS: RwLock<Option<TerminalCapabilities>> = RwLock::new(None);
static CELL_DIMENSIONS: RwLock<(u32, u32)> = RwLock::new((9, 18));
static KITTY_IMAGE_METADATA: Mutex<Vec<RegisteredKittyImageMetadata>> = Mutex::new(Vec::new());
static KITTY_TRANSMISSION_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The environment inputs used by terminal capability detection.
///
/// Keeping the matrix separate from process-global environment reads makes
/// terminal support testable without racing other tests or mutating the
/// caller's environment.  `from_process` is the only place that reads
/// environment variables for the normal runtime path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityEnvironment {
    pub term_program: Option<String>,
    pub terminal_emulator: Option<String>,
    pub term: Option<String>,
    pub color_term: Option<String>,
    pub kitty_window_id: bool,
    pub ghostty_resources_dir: bool,
    pub wezterm_pane: bool,
    pub warp_session_id: bool,
    pub warp_terminal_session_uuid: bool,
    pub iterm_session_id: bool,
    pub wt_session: bool,
    pub tmux: bool,
    pub is_windows: bool,
}

impl CapabilityEnvironment {
    fn from_process() -> Self {
        fn value(name: &str) -> Option<String> {
            std::env::var(name).ok().map(|value| value.to_lowercase())
        }
        Self {
            term_program: value("TERM_PROGRAM"),
            terminal_emulator: value("TERMINAL_EMULATOR"),
            term: value("TERM"),
            color_term: value("COLORTERM"),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            ghostty_resources_dir: std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
            wezterm_pane: std::env::var_os("WEZTERM_PANE").is_some(),
            warp_session_id: std::env::var_os("WARP_SESSION_ID").is_some(),
            warp_terminal_session_uuid: std::env::var_os("WARP_TERMINAL_SESSION_UUID").is_some(),
            iterm_session_id: std::env::var_os("ITERM_SESSION_ID").is_some(),
            wt_session: std::env::var_os("WT_SESSION").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            is_windows: cfg!(windows),
        }
    }
}

/// Detect capabilities from the environment.
pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with_tmux_probe(probe_tmux_hyperlinks)
}

/// Detect capabilities while allowing callers/tests to supply the tmux probe.
pub fn detect_capabilities_with_tmux_probe<F>(tmux_forwards_hyperlink: F) -> TerminalCapabilities
where
    F: FnOnce() -> bool,
{
    detect_capabilities_for_environment(
        &CapabilityEnvironment::from_process(),
        tmux_forwards_hyperlink,
    )
}

/// Detect capabilities for an explicit environment matrix.
///
/// This is public so downstream fixture tests can verify a terminal family
/// without changing process environment variables.  The precedence mirrors
/// the upstream detector: tmux and screen are transport layers and therefore
/// take precedence over the outer terminal's image protocol.
pub fn detect_capabilities_for_environment<F>(
    environment: &CapabilityEnvironment,
    tmux_forwards_hyperlink: F,
) -> TerminalCapabilities
where
    F: FnOnce() -> bool,
{
    let term_program = environment.term_program.as_deref().unwrap_or_default();
    let term_emulator = environment.terminal_emulator.as_deref().unwrap_or_default();
    let term = environment.term.as_deref().unwrap_or_default();
    let color_term = environment.color_term.as_deref().unwrap_or_default();
    let has_true_color = color_term == "truecolor" || color_term == "24bit";

    let tmux = environment.tmux || term.starts_with("tmux");
    if tmux {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color,
            hyperlinks: tmux_forwards_hyperlink(),
        };
    }
    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color,
            hyperlinks: false,
        };
    }
    if environment.kitty_window_id
        || term_program == "kitty"
        || term_program == "ghostty"
        || term.contains("ghostty")
        || environment.ghostty_resources_dir
        || environment.wezterm_pane
        || term_program == "wezterm"
        || term_program == "warpterminal"
        || environment.warp_session_id
        || environment.warp_terminal_session_uuid
    {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }
    if environment.iterm_session_id || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        };
    }
    if environment.wt_session || term_program == "vscode" || term_program == "alacritty" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }
    if term_emulator == "jetbrains-jediterm" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    if environment.is_windows {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    TerminalCapabilities {
        images: None,
        true_color: has_true_color,
        hyperlinks: false,
    }
}

/// Check whether the attached tmux client advertises OSC 8 hyperlink
/// forwarding. tmux strips hyperlinks unless `client_termfeatures` includes
/// `hyperlinks`; failures and timeouts conservatively return `false`.
pub fn probe_tmux_hyperlinks() -> bool {
    let mut child = match Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = match child.wait_with_output() {
                    Ok(output) => output,
                    Err(_) => return false,
                };
                return status.success()
                    && parse_tmux_client_termfeatures(&String::from_utf8_lossy(&output.stdout));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Parse the comma-separated `client_termfeatures` value returned by tmux.
pub fn parse_tmux_client_termfeatures(termfeatures: &str) -> bool {
    termfeatures
        .split(',')
        .map(str::trim)
        .any(|feature| feature == "hyperlinks")
}

pub fn get_capabilities() -> TerminalCapabilities {
    cached_capabilities(&CAPS, detect_capabilities)
}

/// Read the capability cache, detect on a miss, and publish the result.
///
/// The explicit read-guard drop is important: a cache miss must release the
/// read lock before acquiring the write lock, including on the first TTY
/// render in a fresh process.
fn cached_capabilities<F>(
    cache: &RwLock<Option<TerminalCapabilities>>,
    detect: F,
) -> TerminalCapabilities
where
    F: FnOnce() -> TerminalCapabilities,
{
    let cached = cache.read().unwrap_or_else(|e| e.into_inner());
    if let Some(caps) = *cached {
        return caps;
    }
    drop(cached);

    let caps = detect();
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
    caps
}

pub fn set_capabilities(caps: TerminalCapabilities) {
    let mut guard = CAPS.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
}

/// Clear the process capability cache so a later call re-runs detection.
/// This is primarily useful for deterministic fixture tests and parity with
/// the upstream `resetCapabilitiesCache` helper.
pub fn reset_capabilities_cache() {
    let mut guard = CAPS.write().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Allocate a non-zero Kitty image id without relying on a platform RNG API.
/// A monotonic id is sufficient for the TUI's lifetime and deterministic in
/// tests; callers may still provide an explicit id to `encode_kitty`.
pub fn allocate_image_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_IMAGE_ID.store(2, Ordering::Relaxed);
        1
    } else {
        id
    }
}

/// Delete one Kitty image while retaining the terminal's other placements.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Delete all Kitty placements and their uploaded image data.
pub fn delete_all_kitty_images() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// Delete all visible Kitty placements while retaining uploaded image data.
pub fn delete_all_kitty_placements() -> &'static str {
    "\x1b_Ga=d,d=a,q=2\x1b\\"
}

const KITTY_PREFIX: &str = "\x1b_G";

fn kitty_command_controls(line: &str) -> Option<(usize, usize, usize)> {
    let command_start = line.find(KITTY_PREFIX)?;
    let controls_start = command_start + KITTY_PREFIX.len();
    let separator = line[controls_start..].find(';')? + controls_start;
    Some((command_start, controls_start, separator))
}

fn kitty_control_value<'a>(controls: &'a str, key: &str) -> Option<&'a str> {
    controls.split(',').find_map(|control| {
        let (control_key, value) = control.split_once('=')?;
        (control_key == key).then_some(value)
    })
}

fn registered_kitty_metadata(image_id: u32) -> Option<RegisteredKittyImageMetadata> {
    KITTY_IMAGE_METADATA
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|entry| entry.metadata.image_id == image_id)
        .copied()
}

/// Register the dimensions of an image transmission for later Kitty
/// placement-only redraws.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    let generation =
        KITTY_TRANSMISSION_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let mut entries = KITTY_IMAGE_METADATA
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    entries.retain(|entry| entry.metadata.image_id != metadata.image_id);
    entries.push(RegisteredKittyImageMetadata {
        metadata,
        transmission_generation: generation,
    });
    if entries.len() > 1000 {
        entries.remove(0);
    }
}

/// Recover registered image metadata from the first Kitty command on a line.
pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    let (_, controls_start, controls_end) = kitty_command_controls(line)?;
    let controls = &line[controls_start..controls_end];
    let image_id = kitty_control_value(controls, "i")?.parse::<u32>().ok()?;
    registered_kitty_metadata(image_id).map(|entry| entry.metadata)
}

const KITTY_PLACEMENT_CONTROL_KEYS: &[&str] = &[
    "i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H", "V",
];

fn kitty_is_placement_control(control: &str) -> bool {
    control
        .split_once('=')
        .map(|(key, _)| KITTY_PLACEMENT_CONTROL_KEYS.contains(&key))
        .unwrap_or(false)
}

/// Convert a transmitted Kitty image line into a placement-only command and
/// retain the original transfer for accounting/replacement purposes.
pub fn get_kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let (command_start, controls_start, controls_end) = kitty_command_controls(line)?;
    let first_controls = &line[controls_start..controls_end];
    let image_id = kitty_control_value(first_controls, "i")?
        .parse::<u32>()
        .ok()?;
    let metadata = registered_kitty_metadata(image_id)?;

    let mut current_start = command_start;
    let mut current_controls = first_controls;
    let transmission_end = loop {
        let data_start = current_start + KITTY_PREFIX.len();
        let terminator = line[data_start..].find("\x1b\\")? + data_start;
        let end = terminator + 2;
        if !kitty_control_value(current_controls, "m").is_some_and(|value| value == "1") {
            break end;
        }
        current_start = end;
        if !line[current_start..].starts_with(KITTY_PREFIX) {
            return None;
        }
        let next_controls_start = current_start + KITTY_PREFIX.len();
        let next_controls_end = line[next_controls_start..].find(';')? + next_controls_start;
        current_controls = &line[next_controls_start..next_controls_end];
    };

    let controls = first_controls
        .split(',')
        .filter(|control| kitty_is_placement_control(control))
        .collect::<Vec<_>>();
    let sequence = format!("\x1b_Ga=p,q=2,{}\x1b\\", controls.join(","));
    Some(KittyImagePlacement {
        image_id,
        transmission_generation: metadata.transmission_generation,
        transmission_bytes: transmission_end - command_start,
        estimated_decoded_bytes: u64::from(metadata.metadata.width_px)
            .saturating_mul(u64::from(metadata.metadata.height_px))
            .saturating_mul(4),
        sequence: sequence.clone(),
        replacement_line: format!(
            "{}{}{}",
            &line[..command_start],
            sequence,
            &line[transmission_end..]
        ),
    })
}

/// Crop a registered Kitty image line to the visible row range without
/// retransmitting its payload.
pub fn crop_kitty_image_line(line: &str, hidden_rows: usize, visible_rows: usize) -> String {
    let Some(metadata) = get_kitty_image_metadata(line) else {
        return line.to_string();
    };
    let Some((command_start, controls_start, controls_end)) = kitty_command_controls(line) else {
        return line.to_string();
    };
    if hidden_rows >= metadata.rows || visible_rows == 0 {
        return line.to_string();
    }
    let cropped_rows = visible_rows.min(metadata.rows - hidden_rows);
    if hidden_rows == 0 && cropped_rows == metadata.rows {
        return line.to_string();
    }

    let source_y =
        u64::from(metadata.height_px).saturating_mul(hidden_rows as u64) / metadata.rows as u64;
    let source_end = (u64::from(metadata.height_px)
        .saturating_mul((hidden_rows + cropped_rows) as u64)
        .saturating_add(metadata.rows as u64 - 1))
        / metadata.rows as u64;
    let source_height = source_end
        .min(u64::from(metadata.height_px))
        .saturating_sub(source_y)
        .max(1);
    let controls = line[controls_start..controls_end]
        .split(',')
        .filter(|control| {
            !control
                .split_once('=')
                .map(|(key, _)| matches!(key, "y" | "h" | "r"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    let controls = format!(
        "{},y={},h={},r={}",
        controls.join(","),
        source_y,
        source_height,
        cropped_rows
    );
    format!(
        "{}{}{};{}",
        &line[..command_start],
        KITTY_PREFIX,
        controls,
        &line[controls_end + 1..]
    )
}

/// Wrap text in an OSC 8 hyperlink sequence.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether a rendered line contains a kitty/iTerm2 inline image sequence.
pub fn is_image_line(line: &str) -> bool {
    line.starts_with("\x1b_G")
        || line.starts_with("\x1b]1337;File=")
        || line.contains("\x1b_G")
        || line.contains("\x1b]1337;File=")
}

/// Default cell dimensions (used by image sizing).
pub fn get_cell_dimensions() -> (u32, u32) {
    *CELL_DIMENSIONS
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

/// Update measured terminal cell dimensions in pixels. The TUI calls this
/// when it receives the terminal's `CSI 6;height;width t` response.
pub fn set_cell_dimensions(width_px: u32, height_px: u32) {
    if width_px == 0 || height_px == 0 {
        return;
    }
    *CELL_DIMENSIONS
        .write()
        .unwrap_or_else(|error| error.into_inner()) = (width_px, height_px);
}

/// Return measured cell dimensions in the structured form used by image
/// sizing helpers.  The tuple-returning `get_cell_dimensions` remains for
/// compatibility with the existing Rust component API.
pub fn get_cell_dimensions_struct() -> CellDimensions {
    let (width_px, height_px) = get_cell_dimensions();
    CellDimensions {
        width_px,
        height_px,
    }
}

/// Calculate the terminal cells needed to fit an image inside the requested
/// bounds while preserving its aspect ratio.
pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
    cell_dimensions: CellDimensions,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|height| height.max(1));
    let image_width = image_dimensions.width_px.max(1);
    let image_height = image_dimensions.height_px.max(1);
    let cell_width = cell_dimensions.width_px.max(1);
    let cell_height = cell_dimensions.height_px.max(1);

    let width_scale = (max_width as f64 * cell_width as f64) / image_width as f64;
    let height_scale = max_height
        .map(|height| (height as f64 * cell_height as f64) / image_height as f64)
        .unwrap_or(width_scale);
    let scale = width_scale.min(height_scale);
    let scaled_width_px = image_width as f64 * scale;
    let scaled_height_px = image_height as f64 * scale;
    let columns = (scaled_width_px / cell_width as f64).ceil() as usize;
    let rows = (scaled_height_px / cell_height as f64).ceil() as usize;

    ImageCellSize {
        columns: columns.max(1).min(max_width),
        rows: rows.max(1).min(max_height.unwrap_or(usize::MAX)),
    }
}

/// Calculate image rows for a target width using the current cell geometry.
pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: usize,
    cell_dimensions: CellDimensions,
) -> usize {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions).rows
}

/// Parse the terminal response to a `CSI 16 t` cell-size query.
///
/// Terminals report this as `CSI 6 ; height ; width t`. Keep the parser
/// deliberately strict: a malformed response must remain available to the
/// normal key/input path rather than changing image sizing unexpectedly.
fn parse_cell_size_fields(data: &str) -> Option<(u32, u32)> {
    let body = data.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let mut fields = body.split(';');
    let is_decimal =
        |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    let height = fields.next()?;
    let width = fields.next()?;
    if fields.next().is_some() || !is_decimal(height) || !is_decimal(width) {
        return None;
    }
    Some((height.parse().ok()?, width.parse().ok()?))
}

/// Return whether the input is a syntactically complete cell-size response.
///
/// A terminal may legally report zero dimensions while it is transitioning
/// sizes. Upstream consumes that response without updating the cached cell
/// geometry; callers need to distinguish that case from malformed input.
pub fn is_cell_size_response(data: &str) -> bool {
    parse_cell_size_fields(data).is_some()
}

pub fn parse_cell_size_response(data: &str) -> Option<(u32, u32)> {
    let (height, width) = parse_cell_size_fields(data)?;
    if height == 0 || width == 0 {
        return None;
    }
    Some((width, height))
}

/// Decode base64 into bytes (used by image dimension parsing).
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    // Minimal base64 decoder (standard alphabet, no whitespace).
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in data.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Parse PNG dimensions from base64 data (port of `getPngDimensions`).
pub fn get_png_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 24 {
        return None;
    }
    if bytes[0] != 0x89 || bytes[1] != 0x50 || bytes[2] != 0x4e || bytes[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

/// Parse JPEG dimensions from base64 (upstream `getJpegDimensions`).
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
            return Some((width, height));
        }
        if offset + 3 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

/// Parse GIF dimensions from base64 (upstream `getGifDimensions`).
pub fn get_gif_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 10 {
        return None;
    }
    let sig = std::str::from_utf8(&bytes[..6]).ok()?;
    if sig != "GIF87a" && sig != "GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    Some((width, height))
}

/// Parse WebP dimensions from base64 (upstream `getWebpDimensions`).
pub fn get_webp_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 30 {
        return None;
    }
    if &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    match chunk {
        b"VP8 " => {
            if bytes.len() < 30 {
                return None;
            }
            let width = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32;
            let height = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32;
            Some((width, height))
        }
        b"VP8L" => {
            if bytes.len() < 25 {
                return None;
            }
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3fff) + 1;
            let height = ((bits >> 14) & 0x3fff) + 1;
            Some((width, height))
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            let width =
                ((bytes[24] as u32) | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16)) + 1;
            let height =
                ((bytes[27] as u32) | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16)) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

/// Get dimensions from base64 for a mime type.
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<(u32, u32)> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Text fallback when the terminal cannot render inline images.
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<(u32, u32)>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        let display = shorten_home_path(filename);
        let display =
            if get_capabilities().hyperlinks && std::path::Path::new(filename).is_absolute() {
                hyperlink(&display, &path_to_file_url(filename))
            } else {
                display
            };
        parts.push(display);
    }
    parts.push(format!("[{mime_type}]"));
    if let Some((w, h)) = dimensions {
        parts.push(format!("{w}x{h}"));
    }
    format!("[Image: {}]", parts.join(" "))
}

fn shorten_home_path(filename: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return filename.to_string();
    }
    let home_prefix = format!("{home}/");
    let windows_home_prefix = format!("{home}\\");
    if filename == home {
        "~".to_string()
    } else if filename.starts_with(&home_prefix) || filename.starts_with(&windows_home_prefix) {
        format!("~{}", &filename[home.len()..])
    } else {
        filename.to_string()
    }
}

fn path_to_file_url(filename: &str) -> String {
    let normalized = filename.replace('\\', "/");
    let mut url = String::from("file://");
    for byte in normalized.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            url.push(*byte as char);
        } else {
            url.push('%');
            url.push(hex_digit(byte >> 4));
            url.push(hex_digit(byte & 0x0f));
        }
    }
    url
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

pub fn encode_kitty(
    base64_data: &str,
    columns: usize,
    rows: usize,
    image_id: Option<u32>,
    move_cursor: bool,
) -> String {
    const CHUNK_SIZE: usize = 4096;
    let mut params = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if !move_cursor {
        params.push("C=1".to_string());
    }
    if columns > 0 {
        params.push(format!("c={columns}"));
    }
    if rows > 0 {
        params.push(format!("r={rows}"));
    }
    if let Some(id) = image_id {
        params.push(format!("i={id}"));
    }
    let joined = params.join(",");
    if base64_data.len() <= CHUNK_SIZE {
        return format!("\x1b_G{joined};{base64_data}\x1b\\");
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut offset = 0usize;
    let mut is_first = true;
    while offset < base64_data.len() {
        let chunk = &base64_data[offset..(offset + CHUNK_SIZE).min(base64_data.len())];
        let is_last = offset + CHUNK_SIZE >= base64_data.len();
        if is_first {
            chunks.push(format!("\x1b_G{joined},m=1;{chunk}\x1b\\"));
            is_first = false;
        } else if is_last {
            chunks.push(format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push(format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset += CHUNK_SIZE;
    }
    chunks.join("")
}

/// Encode an iTerm2 image using the complete option set.
pub fn encode_iterm2_with_options(base64_data: &str, options: &ITerm2ImageOptions) -> String {
    // iTerm2's `size` is the decoded byte count, not the base64 character
    // count.  The latter is only equal for a few accidental inputs.
    let size = decode_base64(base64_data)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|| base64_data.len());
    let mut params = vec![
        format!("inline={}", usize::from(options.inline)),
        format!("size={size}"),
    ];
    if let Some(width) = &options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = &options.height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = &options.name {
        if !name.is_empty() {
            params.push(format!("name={}", encode_base64(name.as_bytes())));
        }
    }
    if !options.preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={}:{}\x07", params.join(";"), base64_data)
}

/// Encode an iTerm2 image with the historical width/height-auto helper.
pub fn encode_iterm2(base64_data: &str, columns: usize, preserve_aspect_ratio: bool) -> String {
    let mut options = ITerm2ImageOptions::new();
    if columns > 0 {
        options.width = Some(columns.to_string());
    }
    options.height = Some("auto".to_string());
    options.preserve_aspect_ratio = preserve_aspect_ratio;
    encode_iterm2_with_options(base64_data, &options)
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied();
        encoded.push(ALPHABET[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
        if let Some(second) = second {
            encoded.push(
                ALPHABET[((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6) as usize]
                    as char,
            );
        } else {
            encoded.push('=');
        }
        if let Some(third) = chunk.get(2).copied() {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Render an image using the detected terminal protocol.  `None` means the
/// terminal has no inline-image capability; callers should use
/// [`image_fallback`] in that case.
pub fn render_image(
    base64_data: &str,
    image_dimensions: ImageDimensions,
    options: ImageRenderOptions,
) -> Option<RenderedImage> {
    let capabilities = get_capabilities();
    let protocol = capabilities.images?;
    let size = calculate_image_cell_size(
        image_dimensions,
        options.max_width_cells.unwrap_or(80),
        options.max_height_cells,
        get_cell_dimensions_struct(),
    );

    match protocol {
        ImageProtocol::Kitty => {
            if let Some(image_id) = options.image_id {
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: size.columns,
                    rows: size.rows,
                    width_px: image_dimensions.width_px,
                    height_px: image_dimensions.height_px,
                });
            }
            Some(RenderedImage {
                sequence: encode_kitty(
                    base64_data,
                    size.columns,
                    size.rows,
                    options.image_id,
                    options.move_cursor.unwrap_or(true),
                ),
                columns: size.columns,
                rows: size.rows,
                image_id: options.image_id,
            })
        }
        ImageProtocol::ITerm2 => Some(RenderedImage {
            sequence: encode_iterm2(
                base64_data,
                size.columns,
                options.preserve_aspect_ratio.unwrap_or(true),
            ),
            columns: size.columns,
            rows: size.rows,
            image_id: None,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_client_termfeatures() {
        assert!(parse_tmux_client_termfeatures("hyperlinks,RGB,usstyle"));
        assert!(parse_tmux_client_termfeatures(" RGB , hyperlinks "));
        assert!(!parse_tmux_client_termfeatures("RGB,usstyle"));
        assert!(!parse_tmux_client_termfeatures(""));
    }

    #[test]
    fn parses_cell_size_response_height_then_width() {
        assert_eq!(parse_cell_size_response("\x1b[6;18;9t"), Some((9, 18)));
        assert!(is_cell_size_response("\x1b[6;0;9t"));
        assert_eq!(parse_cell_size_response("\x1b[6;18;9"), None);
        assert_eq!(parse_cell_size_response("\x1b[6;0;9t"), None);
        assert_eq!(parse_cell_size_response("\x1b[6;18;9;1t"), None);
        assert!(!is_cell_size_response("\x1b[6;18;9;1t"));
    }

    #[test]
    fn cell_dimensions_are_updated_only_with_positive_values() {
        let original = get_cell_dimensions();
        set_cell_dimensions(11, 22);
        assert_eq!(get_cell_dimensions(), (11, 22));
        set_cell_dimensions(0, 0);
        assert_eq!(get_cell_dimensions(), (11, 22));
        set_cell_dimensions(original.0, original.1);
    }

    #[test]
    fn uncached_capability_detection_releases_read_lock_before_write() {
        let cache = RwLock::new(None);
        let expected = TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };

        let actual = cached_capabilities(&cache, || expected);
        assert!(actual.true_color);
        assert!(cache
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .is_some());
    }

    #[test]
    fn capability_matrix_covers_named_terminal_families_and_fallback() {
        let cases = [
            (
                "Kitty",
                CapabilityEnvironment {
                    kitty_window_id: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Ghostty",
                CapabilityEnvironment {
                    term_program: Some("ghostty".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "WezTerm",
                CapabilityEnvironment {
                    wezterm_pane: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Warp",
                CapabilityEnvironment {
                    term_program: Some("warpterminal".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "iTerm2",
                CapabilityEnvironment {
                    iterm_session_id: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::ITerm2),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "VS Code",
                CapabilityEnvironment {
                    term_program: Some("vscode".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Alacritty",
                CapabilityEnvironment {
                    term_program: Some("alacritty".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "JetBrains",
                CapabilityEnvironment {
                    terminal_emulator: Some("jetbrains-jediterm".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
            (
                "screen",
                CapabilityEnvironment {
                    term: Some("screen-256color".into()),
                    color_term: Some("truecolor".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
            (
                "Windows Terminal",
                CapabilityEnvironment {
                    wt_session: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "unknown with truecolor",
                CapabilityEnvironment {
                    color_term: Some("24bit".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
        ];

        for (name, environment, expected) in cases {
            let actual = detect_capabilities_for_environment(&environment, || false);
            assert_eq!(actual, expected, "capability mismatch for {name}");
        }
    }

    #[test]
    fn tmux_transport_wins_over_outer_terminal_and_probe_controls_links() {
        let environment = CapabilityEnvironment {
            term_program: Some("kitty".into()),
            term: Some("tmux-256color".into()),
            tmux: true,
            ..Default::default()
        };
        assert_eq!(
            detect_capabilities_for_environment(&environment, || true),
            TerminalCapabilities {
                images: None,
                true_color: false,
                hyperlinks: true,
            }
        );
        assert!(!detect_capabilities_for_environment(&environment, || false).hyperlinks);
    }

    #[test]
    fn iterm2_size_uses_decoded_payload_bytes() {
        let encoded = encode_iterm2("SGVsbG8=", 0, true);
        assert!(encoded.contains("size=5"));
        assert!(!encoded.contains("size=8"));
        assert!(encoded.contains(";height=auto:SGVsbG8="));
    }

    #[test]
    fn iterm2_options_encode_all_upstream_file_parameters() {
        let encoded = encode_iterm2_with_options(
            "AAAA",
            &ITerm2ImageOptions {
                width: Some("2".to_string()),
                height: Some("3".to_string()),
                name: Some("a b".to_string()),
                preserve_aspect_ratio: false,
                inline: false,
            },
        );
        assert_eq!(
            encoded,
            "\x1b]1337;File=inline=0;size=3;width=2;height=3;name=YSBi;preserveAspectRatio=0:AAAA\x07"
        );
    }

    #[test]
    fn iterm2_options_default_to_inline_and_preserve_aspect_ratio() {
        let encoded = encode_iterm2_with_options("AAAA", &ITerm2ImageOptions::default());
        assert!(encoded.starts_with("\x1b]1337;File=inline=1;size=3:"));
        assert!(!encoded.contains("preserveAspectRatio=0"));
    }

    #[test]
    fn image_fallback_shortens_and_links_absolute_paths_only_when_supported() {
        let lock = std::sync::Mutex::new(());
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        set_capabilities(TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks: true,
        });
        let linked = image_fallback("image/png", Some((10, 20)), Some("/tmp/a b#c.png"));
        assert!(linked.contains("\x1b]8;;file:///tmp/a%20b%23c.png\x1b\\"));
        assert!(linked.contains("/tmp/a b#c.png"));
        assert_eq!(delete_all_kitty_placements(), "\x1b_Ga=d,d=a,q=2\x1b\\");
        reset_capabilities_cache();
    }

    #[test]
    fn image_sizing_and_kitty_placement_metadata_are_reusable() {
        let original_cell = get_cell_dimensions();
        set_cell_dimensions(10, 10);
        set_capabilities(TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });

        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 20,
                height_px: 20,
            },
            2,
            None,
            CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        );
        assert_eq!(
            size,
            ImageCellSize {
                columns: 2,
                rows: 2
            }
        );
        assert_eq!(
            calculate_image_rows(
                ImageDimensions {
                    width_px: 20,
                    height_px: 20,
                },
                2,
                CellDimensions {
                    width_px: 10,
                    height_px: 10,
                },
            ),
            2
        );

        let rendered = render_image(
            "SGVsbG8=",
            ImageDimensions {
                width_px: 20,
                height_px: 20,
            },
            ImageRenderOptions {
                max_width_cells: Some(2),
                image_id: Some(4242),
                move_cursor: Some(false),
                ..Default::default()
            },
        )
        .expect("kitty support");
        assert_eq!((rendered.columns, rendered.rows), (2, 2));
        assert!(rendered.sequence.contains(",C=1,"));
        assert_eq!(
            get_kitty_image_metadata(&rendered.sequence),
            Some(KittyImageMetadata {
                image_id: 4242,
                columns: 2,
                rows: 2,
                width_px: 20,
                height_px: 20,
            })
        );

        let placement = get_kitty_image_placement(&rendered.sequence).expect("placement");
        assert_eq!(placement.image_id, 4242);
        assert!(placement.transmission_bytes > 0);
        assert!(placement.sequence.contains("a=p,q=2"));
        assert_eq!(
            crop_kitty_image_line(&rendered.sequence, 1, 1),
            "\x1b_Ga=T,f=100,q=2,C=1,c=2,i=4242,y=10,h=10,r=1;SGVsbG8=\x1b\\"
        );

        set_cell_dimensions(original_cell.0, original_cell.1);
        reset_capabilities_cache();
    }

    #[test]
    fn kitty_placement_replaces_all_transmission_chunks_and_keeps_text() {
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: 4243,
            columns: 3,
            rows: 3,
            width_px: 100,
            height_px: 100,
        });
        let transmission = encode_kitty(&"A".repeat(8192), 3, 3, Some(4243), false);
        let line = format!("left {transmission} right");
        let cropped = crop_kitty_image_line(&line, 2, 1);
        let placement = get_kitty_image_placement(&cropped).expect("placement");

        assert_eq!(
            placement.transmission_bytes,
            cropped.len() - "left ".len() - " right".len()
        );
        assert_eq!(placement.estimated_decoded_bytes, 100 * 100 * 4);
        assert_eq!(
            placement.sequence,
            "\x1b_Ga=p,q=2,C=1,c=3,i=4243,y=66,h=34,r=1\x1b\\"
        );
        assert_eq!(
            placement.replacement_line,
            format!("left {} right", placement.sequence)
        );
        assert!(!placement.replacement_line.contains("AAAA"));
    }
}
