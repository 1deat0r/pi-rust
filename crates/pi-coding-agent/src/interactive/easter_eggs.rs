//! Rust-native equivalents of the upstream interactive hidden components.
//!
//! The TypeScript implementations use `setInterval` to advance their state
//! and ask the TUI for a render. The Rust TUI already owns the render loop, so
//! these components keep the same frame clock and state machine but consume
//! due frames from `render`. This deliberately avoids a worker thread: there
//! is nothing to leak when a scene is replaced or the terminal exits. The
//! optional redraw callback is a small seam for callers/tests that need to
//! observe the same invalidation events as the upstream UI callback.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pi_tui::tui::{Component, SharedComponent};
use pi_tui::utils::{truncate_to_width, visible_width};

use super::tui_theme as theme;

/// Callback invoked once for every consumed animation frame.
pub type RedrawCallback = Arc<dyn Fn() + Send + Sync + 'static>;

const ARMIN_WIDTH: usize = 31;
const ARMIN_HEIGHT: usize = 36;
const ARMIN_BYTES_PER_ROW: usize = ARMIN_WIDTH.div_ceil(8);
const ARMIN_DISPLAY_HEIGHT: usize = ARMIN_HEIGHT.div_ceil(2);
const ARMIN_FPS: u64 = 30;
const ARMIN_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / ARMIN_FPS);
// The finite rain cleanup path is the longest effect for this exact XBM. Its
// worst valid schedule is 212 callbacks: the initial drop can begin 35 rows
// above the viewport, each of the remaining visible targets can reset up to
// five rows above the viewport, and the final empty-column cleanup needs one
// extra completion callback.
const ARMIN_MAX_FRAMES: u64 = 212;
const ARMIN_MAX_ANIMATION: Duration =
    Duration::from_nanos((1_000_000_000 / ARMIN_FPS) * ARMIN_MAX_FRAMES);

// Upstream XBM data: 1 is background, 0 is foreground, and bits are LSB
// first.
const ARMIN_BITS: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f,
    0xff, 0xb7, 0xff, 0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f,
    0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3, 0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f,
    0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7, 0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70,
    0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53, 0xc1, 0xff, 0xef, 0x57,
    0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50, 0x6c,
    0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78,
    0xff, 0x07, 0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78,
    0xff, 0xff, 0xdf, 0x78, 0xff, 0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArminEffect {
    Typewriter,
    Scanline,
    Rain,
    Fade,
    Crt,
    Glitch,
    Dissolve,
}

const ARMIN_EFFECTS: [ArminEffect; 7] = [
    ArminEffect::Typewriter,
    ArminEffect::Scanline,
    ArminEffect::Rain,
    ArminEffect::Fade,
    ArminEffect::Crt,
    ArminEffect::Glitch,
    ArminEffect::Dissolve,
];

impl ArminEffect {
    const fn fps(self) -> u64 {
        match self {
            Self::Glitch => 60,
            _ => 30,
        }
    }

    const fn frame_interval(self) -> Duration {
        if matches!(self, Self::Glitch) {
            Duration::from_nanos(1_000_000_000 / self.fps())
        } else {
            ARMIN_FRAME_INTERVAL
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RainDrop {
    y: i32,
    settled: usize,
}

#[derive(Debug)]
enum ArminEffectState {
    Typewriter {
        pos: usize,
    },
    Scanline {
        row: usize,
    },
    Rain {
        drops: Vec<RainDrop>,
    },
    Fade {
        positions: Vec<(usize, usize)>,
        idx: usize,
    },
    Crt {
        expansion: usize,
    },
    Glitch {
        phase: usize,
        glitch_frames: usize,
    },
    Dissolve {
        positions: Vec<(usize, usize)>,
        idx: usize,
    },
}

/// Small deterministic PRNG used only for the same visual randomization
/// points as upstream's `Math.random()`. It is intentionally not a security
/// primitive. A supplied seed makes every state transition reproducible in
/// tests without sleeping or relying on a process-global RNG.
#[derive(Clone, Debug)]
struct VisualRng {
    state: u64,
}

impl VisualRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*: compact, deterministic, and sufficient for pixels and
        // effect selection. The nonzero state is guaranteed by `new`.
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(2_685_821_657_736_338_717)
    }

    fn unit(&mut self) -> f64 {
        // Keep the result in [0, 1), like Math.random().
        ((self.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }

    fn index(&mut self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            (self.unit() * len as f64).floor() as usize
        }
    }
}

fn production_seed() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos() as u64 ^ now.as_secs().rotate_left(17)
}

fn armin_pixel(x: usize, y: usize) -> bool {
    if x >= ARMIN_WIDTH || y >= ARMIN_HEIGHT {
        return false;
    }
    let byte_index = y * ARMIN_BYTES_PER_ROW + x / 8;
    let bit_index = x % 8;
    ((ARMIN_BITS[byte_index] >> bit_index) & 1) == 0
}

fn armin_char(x: usize, row: usize) -> char {
    match (armin_pixel(x, row * 2), armin_pixel(x, row * 2 + 1)) {
        (true, true) => '█',
        (true, false) => '▀',
        (false, true) => '▄',
        (false, false) => ' ',
    }
}

fn armin_grid() -> Vec<Vec<char>> {
    (0..ARMIN_DISPLAY_HEIGHT)
        .map(|row| (0..ARMIN_WIDTH).map(|x| armin_char(x, row)).collect())
        .collect()
}

fn empty_armin_grid() -> Vec<Vec<char>> {
    vec![vec![' '; ARMIN_WIDTH]; ARMIN_DISPLAY_HEIGHT]
}

fn shuffled_positions(rng: &mut VisualRng) -> Vec<(usize, usize)> {
    let mut positions = (0..ARMIN_DISPLAY_HEIGHT)
        .flat_map(|row| (0..ARMIN_WIDTH).map(move |x| (row, x)))
        .collect::<Vec<_>>();
    for index in (1..positions.len()).rev() {
        let other = rng.index(index + 1);
        positions.swap(index, other);
    }
    positions
}

fn rotate_row(row: &[char], offset: isize) -> Vec<char> {
    let width = row.len();
    if width == 0 {
        return Vec::new();
    }
    (0..width)
        .map(|index| {
            let source = (index as isize + offset).rem_euclid(width as isize) as usize;
            row[source]
        })
        .collect()
}

fn random_armin_effect(rng: &mut VisualRng) -> ArminEffect {
    ARMIN_EFFECTS[rng.index(ARMIN_EFFECTS.len())]
}

fn initial_armin_state(
    effect: ArminEffect,
    rng: &mut VisualRng,
) -> (Vec<Vec<char>>, ArminEffectState) {
    match effect {
        ArminEffect::Typewriter => (empty_armin_grid(), ArminEffectState::Typewriter { pos: 0 }),
        ArminEffect::Scanline => (empty_armin_grid(), ArminEffectState::Scanline { row: 0 }),
        ArminEffect::Rain => {
            let drops = (0..ARMIN_WIDTH)
                .map(|_| RainDrop {
                    y: -(rng.index(ARMIN_DISPLAY_HEIGHT * 2) as i32),
                    settled: 0,
                })
                .collect();
            (empty_armin_grid(), ArminEffectState::Rain { drops })
        }
        ArminEffect::Fade => (
            empty_armin_grid(),
            ArminEffectState::Fade {
                positions: shuffled_positions(rng),
                idx: 0,
            },
        ),
        ArminEffect::Crt => (empty_armin_grid(), ArminEffectState::Crt { expansion: 0 }),
        ArminEffect::Glitch => (
            empty_armin_grid(),
            ArminEffectState::Glitch {
                phase: 0,
                glitch_frames: 8,
            },
        ),
        ArminEffect::Dissolve => {
            let chars = [' ', '░', '▒', '▓', '█', '▀', '▄'];
            let noise = (0..ARMIN_DISPLAY_HEIGHT)
                .map(|_| {
                    (0..ARMIN_WIDTH)
                        .map(|_| chars[rng.index(chars.len())])
                        .collect::<Vec<_>>()
                })
                .collect();
            (
                noise,
                ArminEffectState::Dissolve {
                    positions: shuffled_positions(rng),
                    idx: 0,
                },
            )
        }
    }
}

struct ArminRuntime {
    effect: ArminEffect,
    final_grid: Vec<Vec<char>>,
    current_grid: Vec<Vec<char>>,
    effect_state: ArminEffectState,
    rng: VisualRng,
    started: Instant,
    frame_count: u64,
    grid_version: u64,
    running: bool,
    cached_lines: Vec<String>,
    cached_width: Option<usize>,
    cached_version: u64,
    #[cfg(test)]
    elapsed_override: Option<Duration>,
}

impl ArminRuntime {
    fn new(effect: ArminEffect, seed: u64) -> Self {
        let mut rng = VisualRng::new(seed);
        let (current_grid, effect_state) = initial_armin_state(effect, &mut rng);
        Self {
            effect,
            final_grid: armin_grid(),
            current_grid,
            effect_state,
            rng,
            started: Instant::now(),
            frame_count: 0,
            grid_version: 0,
            running: true,
            cached_lines: Vec::new(),
            cached_width: None,
            cached_version: u64::MAX,
            #[cfg(test)]
            elapsed_override: None,
        }
    }

    #[cfg(test)]
    fn test(effect: ArminEffect, seed: u64) -> Self {
        let mut runtime = Self::new(effect, seed);
        runtime.elapsed_override = Some(Duration::ZERO);
        runtime
    }

    fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(elapsed) = self.elapsed_override {
            return elapsed;
        }
        self.started.elapsed()
    }

    fn sync_to_elapsed(&mut self) -> usize {
        if !self.running {
            return 0;
        }
        let interval = self.effect.frame_interval();
        let due = self.elapsed().as_nanos() / interval.as_nanos();
        let mut advanced = 0;
        while self.running && self.frame_count < due as u64 {
            self.tick_once();
            advanced += 1;
        }
        advanced
    }

    fn tick_once(&mut self) -> bool {
        if !self.running {
            return true;
        }
        let done = self.tick_effect();
        self.frame_count += 1;
        self.grid_version = self.grid_version.wrapping_add(1);
        if done {
            self.running = false;
        }
        done
    }

    fn tick_effect(&mut self) -> bool {
        match self.effect {
            ArminEffect::Typewriter => self.tick_typewriter(),
            ArminEffect::Scanline => self.tick_scanline(),
            ArminEffect::Rain => self.tick_rain(),
            ArminEffect::Fade => self.tick_fade(),
            ArminEffect::Crt => self.tick_crt(),
            ArminEffect::Glitch => self.tick_glitch(),
            ArminEffect::Dissolve => self.tick_dissolve(),
        }
    }

    fn tick_typewriter(&mut self) -> bool {
        let ArminEffectState::Typewriter { pos } = &mut self.effect_state else {
            unreachable!("typewriter effect state must match selected effect");
        };
        for _ in 0..3 {
            let row = *pos / ARMIN_WIDTH;
            let x = *pos % ARMIN_WIDTH;
            if row >= ARMIN_DISPLAY_HEIGHT {
                return true;
            }
            self.current_grid[row][x] = self.final_grid[row][x];
            *pos += 1;
        }
        false
    }

    fn tick_scanline(&mut self) -> bool {
        let ArminEffectState::Scanline { row } = &mut self.effect_state else {
            unreachable!("scanline effect state must match selected effect");
        };
        if *row >= ARMIN_DISPLAY_HEIGHT {
            return true;
        }
        self.current_grid[*row] = self.final_grid[*row].clone();
        *row += 1;
        false
    }

    fn tick_rain(&mut self) -> bool {
        let final_grid = &self.final_grid;
        let ArminEffectState::Rain { drops } = &mut self.effect_state else {
            unreachable!("rain effect state must match selected effect");
        };
        let mut all_settled = true;
        self.current_grid = empty_armin_grid();

        for x in 0..ARMIN_WIDTH {
            let drop = &mut drops[x];

            // Draw settled pixels, matching the inclusive reverse loop in the
            // upstream implementation.
            for row in
                (ARMIN_DISPLAY_HEIGHT.saturating_sub(drop.settled)..ARMIN_DISPLAY_HEIGHT).rev()
            {
                self.current_grid[row][x] = final_grid[row][x];
            }

            if drop.settled >= ARMIN_DISPLAY_HEIGHT {
                continue;
            }

            all_settled = false;
            let target_row = (0..=ARMIN_DISPLAY_HEIGHT - 1 - drop.settled)
                .rev()
                .find(|&row| final_grid[row][x] != ' ');

            drop.y += 1;
            match target_row {
                Some(target_row) if drop.y >= 0 && drop.y < ARMIN_DISPLAY_HEIGHT as i32 => {
                    if drop.y as usize >= target_row {
                        drop.settled = ARMIN_DISPLAY_HEIGHT - target_row;
                        drop.y = -(self.rng.index(5) as i32) - 1;
                    } else {
                        self.current_grid[drop.y as usize][x] = '▓';
                    }
                }
                None => {
                    // The pinned upstream loop leaves blank edge columns
                    // running forever because it never assigns `settled` when
                    // targetRow stays -1. Preserve the falling glyph while it
                    // is visible, then settle the empty column once it exits
                    // the grid so Rust has an explicit finite cleanup path.
                    if drop.y >= ARMIN_DISPLAY_HEIGHT as i32 {
                        drop.settled = ARMIN_DISPLAY_HEIGHT;
                    } else if drop.y >= 0 {
                        self.current_grid[drop.y as usize][x] = '▓';
                    }
                }
                Some(_) => {}
            }
        }

        all_settled
    }

    fn tick_fade(&mut self) -> bool {
        let final_grid = &self.final_grid;
        let ArminEffectState::Fade { positions, idx } = &mut self.effect_state else {
            unreachable!("fade effect state must match selected effect");
        };
        for _ in 0..15 {
            if *idx >= positions.len() {
                return true;
            }
            let (row, x) = positions[*idx];
            self.current_grid[row][x] = final_grid[row][x];
            *idx += 1;
        }
        false
    }

    fn tick_crt(&mut self) -> bool {
        let final_grid = &self.final_grid;
        let ArminEffectState::Crt { expansion } = &mut self.effect_state else {
            unreachable!("crt effect state must match selected effect");
        };
        let mid_row = ARMIN_DISPLAY_HEIGHT / 2;
        self.current_grid = empty_armin_grid();
        let top = mid_row as isize - *expansion as isize;
        let bottom = mid_row + *expansion;
        let top = top.max(0) as usize;
        let bottom = bottom.min(ARMIN_DISPLAY_HEIGHT - 1);
        self.current_grid[top..=bottom].clone_from_slice(&final_grid[top..=bottom]);
        *expansion += 1;
        *expansion > ARMIN_DISPLAY_HEIGHT
    }

    fn tick_glitch(&mut self) -> bool {
        let final_grid = &self.final_grid;
        let ArminEffectState::Glitch {
            phase,
            glitch_frames,
        } = &mut self.effect_state
        else {
            unreachable!("glitch effect state must match selected effect");
        };
        if *phase < *glitch_frames {
            self.current_grid = final_grid
                .iter()
                .map(|row| {
                    let offset = self.rng.index(7) as isize - 3;
                    if self.rng.unit() < 0.3 {
                        return rotate_row(row, offset);
                    }
                    if self.rng.unit() < 0.2 {
                        return final_grid[self.rng.index(ARMIN_DISPLAY_HEIGHT)].clone();
                    }
                    row.clone()
                })
                .collect();
            *phase += 1;
            false
        } else {
            self.current_grid = final_grid.clone();
            true
        }
    }

    fn tick_dissolve(&mut self) -> bool {
        let final_grid = &self.final_grid;
        let ArminEffectState::Dissolve { positions, idx } = &mut self.effect_state else {
            unreachable!("dissolve effect state must match selected effect");
        };
        for _ in 0..20 {
            if *idx >= positions.len() {
                return true;
            }
            let (row, x) = positions[*idx];
            self.current_grid[row][x] = final_grid[row][x];
            *idx += 1;
        }
        false
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn invalidate(&mut self) {
        self.cached_width = None;
    }

    fn render_lines(&mut self, width: usize) -> Vec<String> {
        if self.cached_width == Some(width) && self.cached_version == self.grid_version {
            return self.cached_lines.clone();
        }

        let mut lines = self
            .current_grid
            .iter()
            .map(|row| {
                let content = row.iter().collect::<String>();
                fit_line(&format!(" {}", theme::fg("accent", content)), width)
            })
            .collect::<Vec<_>>();
        lines.push(fit_line(
            &format!(" {}", theme::fg("accent", "ARMIN SAYS HI")),
            width,
        ));

        self.cached_lines = lines;
        self.cached_width = Some(width);
        self.cached_version = self.grid_version;
        self.cached_lines.clone()
    }
}

/// The hidden Armin component.
pub struct ArminComponent {
    runtime: Mutex<ArminRuntime>,
    redraw: Option<RedrawCallback>,
}

impl ArminComponent {
    pub fn new() -> Self {
        Self::with_seed(production_seed())
    }

    fn with_seed(seed: u64) -> Self {
        let mut rng = VisualRng::new(seed);
        let effect = random_armin_effect(&mut rng);
        Self {
            runtime: Mutex::new(ArminRuntime::new(effect, seed ^ 0xa5a5_5a5a_1234_5678)),
            redraw: None,
        }
    }

    /// Construct the component with a redraw observer. Interactive mode can
    /// keep using `new`; this seam is useful for a TUI scheduler or tests that
    /// need to observe frame invalidation without a background timer.
    pub fn with_redraw_callback(callback: RedrawCallback) -> Self {
        let mut component = Self::new();
        component.redraw = Some(callback);
        component
    }

    fn notify_redraw(&self, frames: usize) {
        if let Some(callback) = &self.redraw {
            for _ in 0..frames {
                callback();
            }
        }
    }

    /// Stop future frame consumption. No worker exists, but this explicit
    /// lifecycle hook mirrors upstream `dispose()` and makes replacement
    /// cleanup observable to callers.
    pub fn dispose(&self) {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stop();
    }

    pub fn is_running(&self) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .running
    }
}

#[cfg(test)]
impl ArminComponent {
    fn for_test(effect: ArminEffect, seed: u64) -> Self {
        Self {
            runtime: Mutex::new(ArminRuntime::test(effect, seed)),
            redraw: None,
        }
    }

    fn for_test_with_callback(effect: ArminEffect, seed: u64, callback: RedrawCallback) -> Self {
        Self {
            runtime: Mutex::new(ArminRuntime::test(effect, seed)),
            redraw: Some(callback),
        }
    }

    fn advance_for_test(&self, elapsed: Duration) -> usize {
        let frames = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.elapsed_override = Some(elapsed);
            runtime.sync_to_elapsed()
        };
        self.notify_redraw(frames);
        frames
    }

    fn tick_for_test(&self) -> bool {
        let (done, advanced) = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if runtime.running {
                (runtime.tick_once(), 1)
            } else {
                (true, 0)
            }
        };
        self.notify_redraw(advanced);
        done
    }

    fn frame_count_for_test(&self) -> u64 {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .frame_count
    }

    fn effect_progress_for_test(&self) -> usize {
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &runtime.effect_state {
            ArminEffectState::Typewriter { pos } => *pos,
            ArminEffectState::Scanline { row } => *row,
            ArminEffectState::Rain { drops } => drops.iter().map(|drop| drop.settled).sum(),
            ArminEffectState::Fade { idx, .. } | ArminEffectState::Dissolve { idx, .. } => *idx,
            ArminEffectState::Crt { expansion } => *expansion,
            ArminEffectState::Glitch { phase, .. } => *phase,
        }
    }

    fn current_grid_for_test(&self) -> Vec<Vec<char>> {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_grid
            .clone()
    }

    fn final_grid_for_test(&self) -> Vec<Vec<char>> {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .final_grid
            .clone()
    }

    fn cache_is_valid_for_test(&self) -> bool {
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        runtime.cached_width.is_some() && runtime.cached_version == runtime.grid_version
    }
}

impl Default for ArminComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ArminComponent {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn drop(&mut self) {
        self.runtime.get_mut().unwrap().stop();
    }
}

impl Component for ArminComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let frames = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.sync_to_elapsed()
        };
        self.notify_redraw(frames);
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render_lines(width)
    }

    fn invalidate(&mut self) {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
    }
}

const DAX_WIDTH: usize = 32;
const DAX_HEIGHT: usize = 32;

// Verbatim DAX_HEX from the pinned v0.84.2 upstream
// `interactive/components/daxnuts.ts` at pinned commit
// `5cd93f688aaab89dbb6dfa4aca535f21796ae185` (32x32 RGB, six hex characters
// per pixel). Keep the payload in source so this remains independent of an
// external asset path; the renderer below still has a graceful fallback if a
// future terminal cannot display truecolor sequences.
const DAX_HEX: &str = "bbbab8b9b9b6b9b8b5bcbbb8b8b7b4b7b5b2b6b5b2b8b7b4b7b6b3b6b4b1bdbcb8bab8b6bbb8b5b8b5b1bbb8b4c2bebbc1bebac0bdbabfbcb9c1bebabfbebbc0bfbcc0bdbabbb8b5c1bfbcbfbcb8bbb9b6bfbcb8c2bfbcc1bfbcbfbbb8bdb9b6b8b7b5b9b8b5b8b8b5b5b5b2b6b5b2b8b7b4b9b8b5b9b8b5b6b5b3bab8b5bcbab7bbb9b6bbb8b5bfb9b5bdb2abbcb0a8beb2aabeb5afbfbab6bebab7c0bfbcbebdbabebbb8c0bdbabfbebbc2bebbbdbab7c3c0bdc3c0bdc1bebbc2bebabfbcb8bab9b6b7b6b3b2b1aeb6b5b2b5b4b1b5b4b2b6b5b2b7b6b4b9b8b6b7b6b3bbbab7b2afaba5988fb49e90b09481b79a88b39683b09583b7a395bfb6b0c0bdbabdbbb8bebcb9c1bfbcc0bebbbdbab7bebbb8c2bfbcc0bdbac0bcb9bdb9b6c0bcb8b5b4b2b4b3b0bab9b6b9b9b6b5b4b1b5b4b1b6b5b3b9b8b5b9b8b6b9b8b6b2aeaa968174a6836eaa856eab846eaf8973ac8973b08f79b18f7ab39786b7a89dbbb3aebfbab6c2c0bdbebcb9bfbdbac3c1bdc2bebbc0bcb9bdb9b6c1bdbabfbbb8b4b3b0b9b8b5b8b7b5b4b3b1b5b4b1b8b7b4b8b7b5bab9b6bbbab7b1afad8c7a719d735ca47860a87d65a98069ae8972ae8c75af8d77aa826ba98067aa8974b39e90b6a79dbbb2adc0bdbac1bfbdbfbbb8c1bdb9bebab6c0bdb9bfbbb8c1bdbab4b2b0b7b6b4b7b6b3b4b2b0bab9b7b6b5b2b6b5b2bab9b6bab9b6958c87977663aa836bac8772b08f7aad8c77b2917db0917db0907cac8971a77d64a87f67ac8972b29887b8a89dbfbab5bfbdbac1bebac0bcb9c0bcb9c0bcb9c1bebabebab7b8b7b4b7b6b4b5b4b1b5b4b2b7b6b3b5b4b2bab9b7bab9b6b4b1ada88f7fad8973ae8d78b19684b19685b29786b69a89b29582b1917daa856ea87e66a97e66ad866ea9826baf9280b8ada6bdbbb8bebab7bfbbb8c1bdbabfbbb8bcb8b4bcb8b5b6b4b2b7b5b3b6b5b2b8b7b4b3b2afb8b7b4b6b5b2b3b2b0b3a59aab856fad8d78b0917eb19886b49b8bb49a89b39785b0917eaf8f7cab866fa77d65a77a61a87d64a9816ab08f79b5a296c1bcb8c3bfbcc2bebbbebab7bfbbb7bdbab6c2bebab8b7b4b7b6b4b6b5b3b7b6b3b6b5b2b9b8b6b4b3b1b6b1acac8f7ca9826bae8f7aaf9583b49c8cb49c8bb79d8cb59987b19380ad8e79ae8c77af8e78ac8771a3775faa826bae8972b39888bbb6b2bebbb8bfbbb8bfbbb8c0bdb9bebbb7c0bdb9b6b5b2b9b8b5b4b3b1b8b7b5b4b3b0b7b6b4b6b5b3b1a7a0aa8772a77d65a88570b49887b19b8d9c887c907a6d987f71aa907faf917daf8e7aad8c78ac8b77a8836ca9836cac8770b49b8abdb6b2c0bcb9c0bdb9bfbbb8bebab7bfbcb9bebab7b9b8b6b5b4b2b9b8b5b8b7b5b8b7b4b7b6b4b5b4b2b3a9a2ad8973a1755da9856fb398858c776a65544b776358725d526e594d9c7f6eb1907ba68672ad8e7aab8771ac856db18f79b3a092beb9b5c1bdbabdb9b5bebab7bfbbb7bebab7bcb9b6b7b6b4b6b6b3b8b7b4b5b4b2b8b6b4b7b6b3b4b3b0b4aba4a6826ba3775fb08e79b19584a88e7daa8e7db29481ad8f7c997e6da38674ac8d79ac8e7aae917f9a7c6a896a599a7c6ab3a398c1bdbabdb9b6bcb8b5bebab6bebab7bdb9b5bdb9b6b5b4b1b7b5b3b5b4b2b7b6b3b7b6b4b3b3b0b3b2b0b4aca5a7846fa97f68ae8f7bae9383b59c8bb2937fae8e79ac8b76af927eaf927eb29683b39885b2988891786a72594c6e594d978d86bdbab7bab7b3c0bcb9c0bcb9bebab7bebbb7bdb9b6b3b2b0b4b3b0b5b4b2b4b4b1b4b3b1b4b3b1b4b3b0b6ada5aa8670a57a62ad8e7ab29b8cb69d8dab856fa9826aa88069ab8771af907db49987b19684b29886b59987b39480b09787b5a9a1bcb8b5bebab7bdb9b5bebab7bfbbb8bfbbb7bbb7b4b3b2afb8b7b5b8b7b5b3b2b0b5b4b2b6b5b3b6b4b1afa299a98975a9826baf907cb39988b49a89af8e7aac8973aa856eaf8c74b1917dae907dac907db39988b29785b49785b7a090b9aca3bfbab7bcb8b5bdb9b6bcb8b4bcb8b5bdb9b5bcb8b4b5b4b2b6b5b3b4b3b0b4b3b0b9b8b5b8b6b4908b88887467aa8f7ea78976ad8973b08b74b59885b69e8eb29888b1917cb1917db1937fae907cb19686b39a8ab29886b59b8ab8a192b6aaa3b7b2afbcb8b4bcb8b5bbb7b4c0bcb9bebab7c0bcb9b6b5b2b6b5b3b4b3b0bab9b7b7b6b4b1b0ae7b716ba083709b806f716158967764b08870b29481b69b8ab69f8fb39a89b69f90b49d8db39a89b29988b49c8cb6a090b8a496baa49593867f8f8986bfbbb7bdb9b5bcb7b4bab6b3b9b5b2bab6b2b4b3b1b3b3b0b6b5b3b8b7b5b4b2b0a7a5a38f837dae917ea084725a504c63544da28370b39784b59e8db2a093a698909b918b998e8790857e95877dad998bb39c8cb5a091b9a2938d827c95908dbebab6bbb7b3bdbab7bbb7b4bdb9b6bbb7b4b4b3b0b5b4b1b8b7b5b6b5b3b8b8b5b4b2af968f8ab29a8bab9485544b483a323073655d96887f70655f61595547403e453e3c453f3d57504f655e5b90847db39c8db7a090b6a09189807aaba6a3bdb9b6c0bcb9bebab7bcb7b4bebab7bbb7b4b3b2b0b6b5b3b2b1afb7b6b4b8b7b4b5b4b1aeaba8b5a89fac998d4d44412d25244d46444e4744322b293a3230423937433a37352d2a59504c534b48524a48988a81b59f8fb19c8d827974b2afacbdb9b5bcb8b4bdb9b5bcb8b5bdb9b6bab6b2b8b7b5b5b4b2b6b6b3b9b8b5b7b6b3b6b5b2b8b6b3b9b4b1b2a9a26c64612d25242d2625312a28352d2c453d3a78675c8d7a6ea09792aea6a0615854332b29524a479f8e82b09d90a49b96c1bdb9bebab7bfbbb8bbb8b4b9b5b1b8b4b0b9b4b0b7b6b4b8b7b5b8b7b4b6b5b3b8b6b3bab9b6b9b8b5b4b3b0b7b5b2a5a29f453d3b261e1d261f1e2e2625413936857268977865b19482b5a69caca5a07c7572453d3b746963a0948cc5bfbbc0bbb8beb9b6bbb7b3bbb6b3b7b3afb8b4b0b9b5b1b7b6b3b6b5b3b5b4b2b5b4b2b7b6b3b7b6b3b8b6b3b4b2afb7b6b3b3b1ae6d6765251f1e1e18172a22212d2523443b3971625ab19888b09482a89182877e792c25243e3634766d6abeb9b5bfbbb7bebab6bcb7b3bbb6b3b9b5b1b7b3afb8b4b0b4b3b0b5b4b1b5b4b1b4b3b1b5b4b2b8b6b4b5b3b0b9b6b4b5b4b1b6b4b27f79762a2322221c1b2d2524221b1a443e3c47413f6f676281766f867971675e5a3e37352a222166605dbab7b3bdb9b5beb9b5bcb7b3bcb7b3b9b4b0bab6b2bab6b2b5b3b0b6b4b2b3b2afb7b6b3b4b4b1b4b3b0b6b4b1b5b4b1b4b3b0b9b6b29a8c8252474230292828201f181212322c2c231e1d1c16162c26252923222d26252d2523332b2a8e8885bcb8b5bcb7b3bbb6b2bcb7b3b9b4b1b9b5b1b7b2afb7b2ae7a838e9b9b9caeadacb3b2b0b3b2afb7b7b4b6b5b3b6b6b3b7b6b3b9ada4a991808e7b6f50453f2b24231a14142923221f19181d17161f18182620201d17162a22215d5654b7b3b0bbb7b3bbb6b2b8b4b0bab5b1bbb6b2bab5b1b8b4b0bab6b22c496b4c5d735f68766e727a828285929090adaba8b7b2aeb6a59ab39682a28470a387748e76674e403a1a14141d1716181211221c1c1f1918221c1b2f2827342d2c8d8884bab6b3b9b5b2bab5b1bab5b1b9b4b0bab6b2b8b4b0b9b4b0b7b2ae325e8b365f8a3a5d833f5b7a545f70646469706b6aa08f84b08e78b18e769f7e689e7f6b9e816d907766584940362d2a1c1615201b1a1a1413201a1a251e1d393331a39e9bbab5b1bcb7b3bab6b2b8b3afb8b4b0b9b4b0b9b4b1bab5b2b5b0ac3d6c9843729d44719c426e98415f805a64716f6a699d8677b1927eb3947faa89749d7a649f7f6ba487749e837186716454463f2c25231e181837302e3a33317a7471beb9b6bcb8b4bbb6b2b6b2aebab5b1b9b5b1b8b3afbab6b2b6b1adb5aeaa4877a14c7aa44e7ba345719a3a5d80586b7f767475927b6eb1927faf8e79b08e78a78169a07861a17f6aa58570a688749b83738270666f66618a8480a49e99b7b2aebab6b2bcb8b4b9b5b1b7b2aebab5b1b9b4b0b6b1aeb6b1adb2aca8b2aca84876a04a78a2517fa74771973a5d80405c7a6161677c695fac8a75b08d77b4917aaf8971ad876fa5816aa6846ea78670a98a76ac9484ab9f96b2aca8bdb8b4bcb7b3bcb8b4bcb8b4b8b3afb7b2aeb9b4b0b8b3afb8b2aeb6afabb3aeaab2aeaa4878a14b7aa34c7ba44a759b3d63873b5f825b67766f5f569c7e6caf8c77b18f79b28f78b5927caf8e78a98872aa8a76a98a76ac917fada199b7b0acb9b3afbfb9b5c1bab6bdb6b2b8b3afbab5b1b9b4b0b6afabb7b1adb3ada9b3aeaab0aba8";

fn dax_image() -> Vec<String> {
    if DAX_HEX.len() == DAX_WIDTH * DAX_HEIGHT * 6 {
        let mut pixels = vec![[0_u8; 3]; DAX_WIDTH * DAX_HEIGHT];
        for (index, pixel) in pixels.iter_mut().enumerate() {
            let offset = index * 6;
            *pixel = [
                u8::from_str_radix(&DAX_HEX[offset..offset + 2], 16).unwrap_or_default(),
                u8::from_str_radix(&DAX_HEX[offset + 2..offset + 4], 16).unwrap_or_default(),
                u8::from_str_radix(&DAX_HEX[offset + 4..offset + 6], 16).unwrap_or_default(),
            ];
        }
        (0..DAX_HEIGHT)
            .step_by(2)
            .map(|row| {
                let mut line = String::new();
                for x in 0..DAX_WIDTH {
                    let top = pixels[row * DAX_WIDTH + x];
                    let bottom = pixels[(row + 1) * DAX_WIDTH + x];
                    line.push_str(&format!(
                        "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▄",
                        bottom[0], bottom[1], bottom[2], top[0], top[1], top[2]
                    ));
                }
                line.push_str("\x1b[0m");
                line
            })
            .collect()
    } else {
        Vec::new()
    }
}

fn dax_scanline() -> String {
    format!("\x1b[38;2;100;200;255m{}\x1b[0m", "▓".repeat(DAX_WIDTH))
}

/// The OpenCode + Kimi K2.5 Daxnuts component.
pub struct DaxnutsComponent {
    runtime: Mutex<DaxnutsRuntime>,
    redraw: Option<RedrawCallback>,
}

impl DaxnutsComponent {
    pub fn new() -> Self {
        Self {
            runtime: Mutex::new(DaxnutsRuntime::new()),
            redraw: None,
        }
    }

    /// Construct the component with a redraw observer. The normal interactive
    /// path keeps using `new`; this mirrors upstream's `requestRender` seam
    /// without introducing a timer thread into the Rust component.
    pub fn with_redraw_callback(callback: RedrawCallback) -> Self {
        Self {
            runtime: Mutex::new(DaxnutsRuntime::new()),
            redraw: Some(callback),
        }
    }

    fn notify_redraw(&self, frames: usize) {
        if let Some(callback) = &self.redraw {
            for _ in 0..frames {
                callback();
            }
        }
    }

    /// Stop future frame consumption, matching upstream `dispose()`.
    pub fn dispose(&self) {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stop();
    }

    pub fn is_running(&self) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .running
    }
}

#[cfg(test)]
impl DaxnutsComponent {
    fn completed() -> Self {
        Self {
            runtime: Mutex::new(DaxnutsRuntime::completed()),
            redraw: None,
        }
    }

    fn for_test() -> Self {
        Self {
            runtime: Mutex::new(DaxnutsRuntime::test()),
            redraw: None,
        }
    }

    fn for_test_with_callback(callback: RedrawCallback) -> Self {
        Self {
            runtime: Mutex::new(DaxnutsRuntime::test()),
            redraw: Some(callback),
        }
    }

    fn advance_for_test(&self, elapsed: Duration) -> usize {
        let frames = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.elapsed_override = Some(elapsed);
            runtime.sync_to_elapsed()
        };
        self.notify_redraw(frames);
        frames
    }

    fn tick_for_test(&self) -> bool {
        let (done, advanced) = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if runtime.running {
                (runtime.tick_once(), 1)
            } else {
                (true, 0)
            }
        };
        self.notify_redraw(advanced);
        done
    }

    fn tick_for_test_value(&self) -> u64 {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tick
    }

    fn cache_is_valid_for_test(&self) -> bool {
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        runtime.cached_width.is_some() && runtime.cached_tick == Some(runtime.tick)
    }
}

impl Default for DaxnutsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DaxnutsComponent {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn drop(&mut self) {
        self.runtime.get_mut().unwrap().stop();
    }
}

impl Component for DaxnutsComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let frames = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            runtime.sync_to_elapsed()
        };
        self.notify_redraw(frames);
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render_lines(width)
    }

    fn invalidate(&mut self) {
        self.runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
    }
}

const DAX_TICK_INTERVAL: Duration = Duration::from_millis(80);
const DAX_MAX_TICKS: u64 = 25;

/// The Daxnuts component's exact upstream animation lifetime: one 80 ms
/// interval for each of its 25 scheduled ticks.
pub const fn daxnuts_animation_duration() -> Duration {
    Duration::from_millis(DAX_MAX_TICKS * 80)
}

struct DaxnutsRuntime {
    started: Instant,
    tick: u64,
    running: bool,
    image: Vec<String>,
    cached_lines: Vec<String>,
    cached_width: Option<usize>,
    cached_tick: Option<u64>,
    #[cfg(test)]
    elapsed_override: Option<Duration>,
}

impl DaxnutsRuntime {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            tick: 0,
            running: true,
            image: dax_image(),
            cached_lines: Vec::new(),
            cached_width: None,
            cached_tick: None,
            #[cfg(test)]
            elapsed_override: None,
        }
    }

    #[cfg(test)]
    fn completed() -> Self {
        let mut runtime = Self::new();
        runtime.elapsed_override = Some(DAX_TICK_INTERVAL * DAX_MAX_TICKS as u32);
        runtime.sync_to_elapsed();
        runtime
    }

    #[cfg(test)]
    fn test() -> Self {
        let mut runtime = Self::new();
        runtime.elapsed_override = Some(Duration::ZERO);
        runtime
    }

    fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(elapsed) = self.elapsed_override {
            return elapsed;
        }
        self.started.elapsed()
    }

    fn sync_to_elapsed(&mut self) -> usize {
        if !self.running {
            return 0;
        }
        let due = (self.elapsed().as_nanos() / DAX_TICK_INTERVAL.as_nanos())
            .min(DAX_MAX_TICKS as u128) as u64;
        let mut advanced = 0;
        while self.running && self.tick < due {
            self.tick_once();
            advanced += 1;
        }
        advanced
    }

    fn tick_once(&mut self) -> bool {
        if !self.running {
            return true;
        }
        self.tick += 1;
        self.cached_width = None;
        if self.tick >= DAX_MAX_TICKS {
            self.running = false;
            true
        } else {
            false
        }
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn invalidate(&mut self) {
        self.cached_width = None;
    }

    fn render_lines(&mut self, width: usize) -> Vec<String> {
        if self.cached_width == Some(width) && self.cached_tick == Some(self.tick) {
            return self.cached_lines.clone();
        }

        let revealed_rows = ((self.tick * (self.image.len() as u64 + 3)) / DAX_MAX_TICKS)
            .min(self.image.len() as u64) as usize;
        let mut lines = vec![String::new()];

        for index in 0..self.image.len() {
            let image_line = if index < revealed_rows {
                self.image[index].clone()
            } else if index == revealed_rows && revealed_rows < self.image.len() {
                dax_scanline()
            } else {
                " ".repeat(DAX_WIDTH)
            };
            lines.push(center_line(&image_line, width));
        }

        lines.push(String::new());
        // Upstream computes textPhase = max(0, tick - 15). The first block
        // therefore appears at tick 16, not at the continuous 60% point.
        if self.tick > 15 {
            lines.push(center_line(
                &theme::fg("accent", "Free Kimi K2.5 via OpenCode Zen"),
                width,
            ));
            lines.push(center_line(
                &theme::fg("success", "\"Powered by daxnuts\""),
                width,
            ));
            lines.push(center_line(&theme::fg("muted", "— @thdxr"), width));
        } else {
            lines.extend([String::new(), String::new(), String::new()]);
        }

        lines.push(String::new());
        // textPhase > 2 starts the second block at tick 18. The explicit
        // completion condition is retained for parity if maxTicks changes.
        if self.tick >= 18 || self.tick >= DAX_MAX_TICKS {
            lines.push(center_line(&theme::fg("dim", "Try OpenCode"), width));
            lines.push(center_line(
                &theme::fg("mdLink", "https://mistral.ai/news/mistral-vibe-2-0"),
                width,
            ));
        } else {
            lines.extend([String::new(), String::new()]);
        }
        lines.push(String::new());

        self.cached_lines = lines;
        self.cached_width = Some(width);
        self.cached_tick = Some(self.tick);
        self.cached_lines.clone()
    }
}

const EAR_ENDIL_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";

/// The Earendil announcement. The upstream image is optional and is absent
/// from source checkouts without bundled interactive assets; the textual
/// fallback has the same graceful behavior at every width.
pub struct EarendilAnnouncementComponent;

impl EarendilAnnouncementComponent {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EarendilAnnouncementComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for EarendilAnnouncementComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let border = theme::fg("accent", "─".repeat(width));
        vec![
            fit_line(&border, width),
            fit_line(
                &theme::bold(theme::fg("accent", "pi has joined Earendil")),
                width,
            ),
            String::new(),
            fit_line(&theme::fg("muted", "Read the blog post:"), width),
            fit_line(&theme::fg("mdLink", EAR_ENDIL_URL), width),
            String::new(),
            fit_line(&border, width),
        ]
    }
}

/// Return a hidden component ready to append to the interactive scene.
pub fn armin_component() -> SharedComponent {
    Arc::new(Mutex::new(ArminComponent::new()))
}

/// Return a Daxnuts component ready to append to the interactive scene.
pub fn daxnuts_component() -> SharedComponent {
    Arc::new(Mutex::new(DaxnutsComponent::new()))
}

/// Return an Earendil announcement ready to append to the interactive scene.
pub fn earendil_component() -> SharedComponent {
    Arc::new(Mutex::new(EarendilAnnouncementComponent::new()))
}

/// Match the upstream model-triggered Easter egg exactly.
pub fn is_daxnuts_model(provider: &str, model_id: &str) -> bool {
    provider == "opencode" && model_id.to_ascii_lowercase().contains("kimi-k2.5")
}

/// The maximum period during which either hidden component needs animation
/// redraws. The finite rain Armin effect is longest for this exact grid: 212
/// 30-FPS callbacks, including its explicit cleanup and completion check.
pub const fn animation_duration() -> Duration {
    ARMIN_MAX_ANIMATION
}

fn fit_line(line: &str, width: usize) -> String {
    let clipped = truncate_to_width(line, width, "");
    format!(
        "{clipped}{}",
        " ".repeat(width.saturating_sub(visible_width(&clipped)))
    )
}

fn center_line(line: &str, width: usize) -> String {
    let clipped = truncate_to_width(line, width, "");
    let visible = visible_width(&clipped).min(width);
    let left = (width - visible) / 2;
    let right = width - visible - left;
    format!("{}{}{}", " ".repeat(left), clipped, " ".repeat(right))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn plain(lines: &[String]) -> String {
        pi_tui::utils::strip_terminal_sequences(&lines.join("\n"))
    }

    fn assert_width(lines: &[String], width: usize) {
        for line in lines {
            assert!(visible_width(line) <= width, "width {width}: {line:?}");
        }
    }

    #[test]
    fn hidden_components_are_safe_at_narrow_widths() {
        let components: [Box<dyn Component>; 3] = [
            Box::new(ArminComponent::new()),
            Box::new(DaxnutsComponent::new()),
            Box::new(EarendilAnnouncementComponent::new()),
        ];
        for width in 1..=64 {
            for component in &components {
                for line in component.render(width) {
                    assert!(visible_width(&line) <= width, "{width}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn armin_grid_matches_the_upstream_xbm_dimensions() {
        assert_eq!(ARMIN_BITS.len(), ARMIN_HEIGHT * ARMIN_BYTES_PER_ROW);
        let grid = armin_grid();
        assert_eq!(grid.len(), ARMIN_DISPLAY_HEIGHT);
        assert!(grid.iter().all(|row| row.len() == ARMIN_WIDTH));
        assert!(grid.iter().flatten().any(|cell| *cell != ' '));
        assert!(grid
            .iter()
            .flatten()
            .all(|cell| { matches!(*cell, ' ' | '▀' | '▄' | '█') }));
    }

    #[test]
    fn armin_effects_use_upstream_work_units_and_finish_cleanly() {
        let cases = [
            (ArminEffect::Typewriter, 187, 558),
            (ArminEffect::Scanline, 19, 18),
            (ArminEffect::Fade, 38, 558),
            (ArminEffect::Crt, 19, 19),
            (ArminEffect::Glitch, 9, 8),
            (ArminEffect::Dissolve, 28, 558),
        ];

        for (effect, expected_frames, expected_progress) in cases {
            let component = ArminComponent::for_test(effect, 0xfeed_face);
            assert!(component.is_running(), "{effect:?} did not start");
            let mut safety = 0;
            while component.is_running() {
                let done = component.tick_for_test();
                safety += 1;
                assert!(safety <= 200, "{effect:?} did not finish");
                assert_eq!(done, !component.is_running());
            }
            assert_eq!(
                component.frame_count_for_test(),
                expected_frames,
                "{effect:?}"
            );
            assert_eq!(
                component.effect_progress_for_test(),
                expected_progress,
                "{effect:?}"
            );
            assert_eq!(
                component.current_grid_for_test(),
                component.final_grid_for_test()
            );
            assert!(component.tick_for_test());
            assert_eq!(component.frame_count_for_test(), expected_frames);
        }
    }

    #[test]
    fn armin_each_effect_has_the_upstream_first_frame_shape() {
        let typewriter = ArminComponent::for_test(ArminEffect::Typewriter, 1);
        assert!(!typewriter.tick_for_test());
        assert_eq!(typewriter.effect_progress_for_test(), 3);
        let typewriter_grid = typewriter.current_grid_for_test();
        let final_grid = typewriter.final_grid_for_test();
        assert_eq!(&typewriter_grid[0][..3], &final_grid[0][..3]);

        let scanline = ArminComponent::for_test(ArminEffect::Scanline, 1);
        assert!(!scanline.tick_for_test());
        let scanline_grid = scanline.current_grid_for_test();
        let final_grid = scanline.final_grid_for_test();
        assert_eq!(scanline_grid[0], final_grid[0]);
        assert!(scanline_grid[1..].iter().flatten().all(|cell| *cell == ' '));

        let fade = ArminComponent::for_test(ArminEffect::Fade, 1);
        assert!(!fade.tick_for_test());
        assert_eq!(fade.effect_progress_for_test(), 15);

        let crt = ArminComponent::for_test(ArminEffect::Crt, 1);
        assert!(!crt.tick_for_test());
        let crt_grid = crt.current_grid_for_test();
        let final_grid = crt.final_grid_for_test();
        assert_eq!(
            crt_grid[ARMIN_DISPLAY_HEIGHT / 2],
            final_grid[ARMIN_DISPLAY_HEIGHT / 2]
        );
        assert!(crt_grid
            .iter()
            .enumerate()
            .filter(|(row, _)| *row != ARMIN_DISPLAY_HEIGHT / 2)
            .flat_map(|(_, row)| row.iter())
            .all(|cell| *cell == ' '));

        let glitch = ArminComponent::for_test(ArminEffect::Glitch, 1);
        assert!(!glitch.tick_for_test());
        assert_eq!(glitch.effect_progress_for_test(), 1);
        assert!(glitch
            .current_grid_for_test()
            .iter()
            .all(|row| row.len() == ARMIN_WIDTH));

        let dissolve = ArminComponent::for_test(ArminEffect::Dissolve, 1);
        let noise_chars = [' ', '░', '▒', '▓', '█', '▀', '▄'];
        assert!(dissolve
            .current_grid_for_test()
            .iter()
            .flatten()
            .all(|cell| noise_chars.contains(cell)));
        assert!(!dissolve.tick_for_test());
        assert_eq!(dissolve.effect_progress_for_test(), 20);

        let rain = ArminComponent::for_test(ArminEffect::Rain, 1);
        assert_eq!(rain.effect_progress_for_test(), 0);
        assert!(!rain.tick_for_test());
        assert!(rain.effect_progress_for_test() <= ARMIN_WIDTH * ARMIN_DISPLAY_HEIGHT);
    }

    #[test]
    fn armin_rain_settles_every_column_and_finishes_with_the_final_grid() {
        let component = ArminComponent::for_test(ArminEffect::Rain, 0x1234_5678);
        let mut safety = 0;
        while component.is_running() {
            let done = component.tick_for_test();
            safety += 1;
            assert!(safety <= 256, "rain effect did not finish");
            assert_eq!(done, !component.is_running());
        }
        assert!(component.frame_count_for_test() > 0);
        assert_eq!(
            component.current_grid_for_test(),
            component.final_grid_for_test()
        );
        assert_eq!(
            component.effect_progress_for_test(),
            ARMIN_WIDTH * ARMIN_DISPLAY_HEIGHT
        );
    }

    #[test]
    fn armin_tick_clock_has_30fps_and_glitch_60fps_semantics() {
        assert_eq!(
            ArminEffect::Typewriter.frame_interval(),
            ARMIN_FRAME_INTERVAL
        );
        assert_eq!(
            ArminEffect::Glitch.frame_interval(),
            Duration::from_nanos(16_666_666)
        );

        let component = ArminComponent::for_test(ArminEffect::Scanline, 7);
        assert_eq!(
            component.advance_for_test(ARMIN_FRAME_INTERVAL - Duration::from_nanos(1)),
            0
        );
        assert_eq!(component.frame_count_for_test(), 0);
        assert_eq!(component.advance_for_test(ARMIN_FRAME_INTERVAL), 1);
        assert_eq!(component.frame_count_for_test(), 1);
        assert_eq!(component.advance_for_test(ARMIN_FRAME_INTERVAL * 3), 2);
        assert_eq!(component.frame_count_for_test(), 3);
    }

    #[test]
    fn armin_redraw_seam_and_dispose_are_deterministic_and_leak_free() {
        let redraws = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&redraws);
        let component = ArminComponent::for_test_with_callback(
            ArminEffect::Scanline,
            11,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert_eq!(component.advance_for_test(ARMIN_FRAME_INTERVAL * 2), 2);
        assert_eq!(redraws.load(Ordering::SeqCst), 2);
        component.dispose();
        assert!(!component.is_running());
        assert_eq!(component.advance_for_test(Duration::from_secs(10)), 0);
        assert_eq!(redraws.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn armin_seed_injection_reproduces_random_effect_frames() {
        for effect in ARMIN_EFFECTS {
            let first = ArminComponent::for_test(effect, 0xabc0_1234);
            let second = ArminComponent::for_test(effect, 0xabc0_1234);
            for _ in 0..3 {
                assert_eq!(first.tick_for_test(), second.tick_for_test());
                assert_eq!(
                    first.current_grid_for_test(),
                    second.current_grid_for_test()
                );
            }
        }
    }

    #[test]
    fn armin_cache_invalidation_and_width_clipping_match_component_contract() {
        let mut component = ArminComponent::for_test(ArminEffect::Typewriter, 2);
        for width in 0..=64 {
            let lines = component.render(width);
            assert_eq!(lines.len(), ARMIN_DISPLAY_HEIGHT + 1);
            assert_width(&lines, width);
            assert!(component.cache_is_valid_for_test());
        }
        component.invalidate();
        assert!(!component.cache_is_valid_for_test());
        let lines = component.render(31);
        assert_width(&lines, 31);
        assert!(plain(&lines).contains("ARMIN SAYS HI"));
    }

    #[test]
    fn hidden_components_have_the_upstream_completion_text() {
        let armin = ArminComponent::new().render(80).join("\n");
        assert!(armin.contains("ARMIN SAYS HI"));
        let daxnuts = DaxnutsComponent::completed().render(80).join("\n");
        assert!(daxnuts.contains("Free Kimi K2.5 via OpenCode Zen"));
        assert!(daxnuts.contains("Powered by daxnuts"));
        let announcement = EarendilAnnouncementComponent::new().render(80).join("\n");
        assert!(announcement.contains("pi has joined Earendil"));
        assert!(announcement.contains(EAR_ENDIL_URL));
    }

    #[test]
    fn model_trigger_is_provider_and_id_specific() {
        assert!(is_daxnuts_model("opencode", "kimi-k2.5"));
        assert!(is_daxnuts_model("opencode", "KIMI-K2.5-free"));
        assert!(!is_daxnuts_model("openai", "kimi-k2.5"));
        assert!(!is_daxnuts_model("opencode", "kimi-k2.6"));
    }

    #[test]
    fn animation_duration_covers_the_longest_upstream_effect() {
        assert_eq!(animation_duration(), ARMIN_MAX_ANIMATION);
        assert_eq!(ARMIN_MAX_FRAMES, worst_case_rain_frames(&armin_grid()));
        assert_eq!(animation_duration(), Duration::from_nanos(212 * 33_333_333));
        assert!(animation_duration() >= DAX_TICK_INTERVAL * DAX_MAX_TICKS as u32);
    }

    #[allow(clippy::needless_range_loop)]
    fn worst_case_rain_frames(grid: &[Vec<char>]) -> u64 {
        let initial_offset = (ARMIN_DISPLAY_HEIGHT * 2 - 1) as u64;
        let reset_offset = 5_u64;
        let mut maximum = 0;

        for x in 0..ARMIN_WIDTH {
            let rows = (0..ARMIN_DISPLAY_HEIGHT)
                .rev()
                .filter(|&row| grid[row][x] != ' ')
                .collect::<Vec<_>>();
            let frames = if let Some((&first, rest)) = rows.split_first() {
                initial_offset
                    + first as u64
                    + rest
                        .iter()
                        .map(|&row| reset_offset + row as u64)
                        .sum::<u64>()
                    + ARMIN_DISPLAY_HEIGHT as u64
                    + reset_offset
                    + 1
            } else {
                initial_offset + ARMIN_DISPLAY_HEIGHT as u64 + 1
            };
            maximum = maximum.max(frames);
        }

        maximum
    }

    #[test]
    fn daxnuts_keeps_a_non_empty_upstream_image() {
        assert_eq!(DAX_HEX.len(), DAX_WIDTH * DAX_HEIGHT * 6);
        assert_eq!(dax_image().len(), DAX_HEIGHT / 2);
        assert!(dax_image().iter().any(|line| visible_width(line) > 0));
    }

    #[test]
    fn dax_scanline_uses_real_terminal_escapes() {
        let scanline = dax_scanline();
        assert!(scanline.contains('\x1b'));
        assert!(!scanline.contains(r"\x1b"));
    }

    #[test]
    fn daxnuts_has_exact_80ms_25_tick_lifecycle_and_text_phases() {
        let manual = DaxnutsComponent::for_test();
        assert!(!manual.tick_for_test());
        assert_eq!(manual.tick_for_test_value(), 1);

        let redraws = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&redraws);
        let component = DaxnutsComponent::for_test_with_callback(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));

        assert_eq!(component.tick_for_test_value(), 0);
        assert!(component.is_running());
        assert_eq!(
            component.advance_for_test(DAX_TICK_INTERVAL - Duration::from_nanos(1)),
            0
        );
        assert_eq!(component.tick_for_test_value(), 0);
        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL), 1);
        assert_eq!(component.tick_for_test_value(), 1);

        let tick15 = DAX_TICK_INTERVAL * 15;
        assert_eq!(component.advance_for_test(tick15), 14);
        let before_text = plain(&component.render(80));
        assert!(!before_text.contains("Free Kimi K2.5 via OpenCode Zen"));
        assert!(!before_text.contains("Try OpenCode"));

        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL * 16), 1);
        let first_text = plain(&component.render(80));
        assert!(first_text.contains("Free Kimi K2.5 via OpenCode Zen"));
        assert!(!first_text.contains("Try OpenCode"));

        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL * 18), 2);
        let second_text = plain(&component.render(80));
        assert!(second_text.contains("Try OpenCode"));
        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL * 25), 7);
        assert_eq!(component.tick_for_test_value(), DAX_MAX_TICKS);
        assert!(!component.is_running());
        assert_eq!(redraws.load(Ordering::SeqCst), 25);
        assert_eq!(component.advance_for_test(Duration::from_secs(10)), 0);
        assert_eq!(redraws.load(Ordering::SeqCst), 25);
    }

    #[test]
    fn daxnuts_cache_dimensions_and_completion_are_exact() {
        let mut component = DaxnutsComponent::for_test();
        let initial = component.render(80);
        assert_eq!(initial.len(), 25);
        assert!(component.cache_is_valid_for_test());
        component.invalidate();
        assert!(!component.cache_is_valid_for_test());

        for width in 0..=64 {
            let lines = component.render(width);
            assert_eq!(lines.len(), 25);
            assert_width(&lines, width);
        }

        let completed = DaxnutsComponent::completed().render(80);
        let completed_plain = plain(&completed);
        assert!(completed_plain.contains("Free Kimi K2.5 via OpenCode Zen"));
        assert!(completed_plain.contains("\"Powered by daxnuts\""));
        assert!(completed_plain.contains("Try OpenCode"));
        assert!(completed_plain.contains("https://mistral.ai/news/mistral-vibe-2-0"));
    }

    #[test]
    fn daxnuts_reveals_image_rows_with_the_upstream_scanline_schedule() {
        let component = DaxnutsComponent::for_test();
        let initial = component.render(80);
        assert_eq!(initial[1], center_line(&dax_scanline(), 80));

        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL), 1);
        assert_eq!(component.render(80)[1], center_line(&dax_scanline(), 80));

        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL * 2), 1);
        assert_eq!(component.render(80)[1], center_line(&dax_image()[0], 80));

        assert_eq!(component.advance_for_test(DAX_TICK_INTERVAL * 22), 20);
        let complete_image = component.render(80);
        assert!(!complete_image
            .iter()
            .any(|line| pi_tui::utils::strip_terminal_sequences(line).contains('▓')));
    }
}
