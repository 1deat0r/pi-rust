//! Rust-native equivalents of the upstream interactive hidden components.
//!
//! The TypeScript versions use timers to request renders. The Rust TUI owns
//! the render loop, so these components use a monotonic start time instead.
//! They never spawn a task and therefore have no worker to leak or join when a
//! scene is replaced or the terminal exits.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::tui::{Component, SharedComponent};
use pi_tui::utils::{truncate_to_width, visible_width};

use super::tui_theme as theme;

const ARMIN_WIDTH: usize = 31;
const ARMIN_HEIGHT: usize = 36;
const ARMIN_DISPLAY_HEIGHT: usize = ARMIN_HEIGHT.div_ceil(2);
const ARMIN_ANIMATION: Duration = Duration::from_millis(900);

// Upstream XBM data: 1 is background, 0 is foreground, and bits are LSB
// first. The deterministic scanline reveal below is the bounded Rust
// equivalent of the upstream effect variants.
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

fn armin_pixel(x: usize, y: usize) -> bool {
    if x >= ARMIN_WIDTH || y >= ARMIN_HEIGHT {
        return false;
    }
    let byte_index = y * ARMIN_WIDTH.div_ceil(8) + x / 8;
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

/// The hidden Armin component.
pub struct ArminComponent {
    started: Instant,
    grid: Vec<Vec<char>>,
}

impl ArminComponent {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            grid: armin_grid(),
        }
    }
}

impl Default for ArminComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ArminComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let progress =
            (self.started.elapsed().as_secs_f64() / ARMIN_ANIMATION.as_secs_f64()).min(1.0);
        let revealed = ((progress * (ARMIN_DISPLAY_HEIGHT + 1) as f64).floor() as usize)
            .min(ARMIN_DISPLAY_HEIGHT);
        let mut lines = Vec::with_capacity(ARMIN_DISPLAY_HEIGHT + 1);

        for row in 0..ARMIN_DISPLAY_HEIGHT {
            let content = if row < revealed {
                self.grid[row].iter().collect::<String>()
            } else if row == revealed && revealed < ARMIN_DISPLAY_HEIGHT {
                "─".repeat(ARMIN_WIDTH)
            } else {
                " ".repeat(ARMIN_WIDTH)
            };
            lines.push(fit_line(
                &format!(" {}", theme::fg("accent", content)),
                width,
            ));
        }
        lines.push(fit_line(
            &format!(" {}", theme::fg("accent", "ARMIN SAYS HI")),
            width,
        ));
        lines
    }
}

const DAX_WIDTH: usize = 32;
const DAX_HEIGHT: usize = 32;
const DAX_ANIMATION: Duration = Duration::from_millis(2_000);

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
    started: Instant,
    image: Vec<String>,
}

impl DaxnutsComponent {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            image: dax_image(),
        }
    }
}

#[cfg(test)]
impl DaxnutsComponent {
    fn completed() -> Self {
        Self {
            started: Instant::now() - DAX_ANIMATION,
            image: dax_image(),
        }
    }
}

impl Default for DaxnutsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for DaxnutsComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let progress =
            (self.started.elapsed().as_secs_f64() / DAX_ANIMATION.as_secs_f64()).min(1.0);
        let revealed = if self.image.is_empty() {
            0
        } else {
            ((progress * (self.image.len() + 3) as f64).floor() as usize).min(self.image.len())
        };
        let mut lines = vec![String::new()];

        for index in 0..self.image.len() {
            let image_line = if index < revealed {
                self.image[index].clone()
            } else if index == revealed && revealed < self.image.len() {
                dax_scanline()
            } else {
                " ".repeat(DAX_WIDTH)
            };
            lines.push(center_line(&image_line, width));
        }

        lines.push(String::new());
        if progress >= 0.6 {
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
        if progress >= 0.6 {
            lines.push(center_line(&theme::fg("dim", "Try OpenCode"), width));
            lines.push(center_line(
                &theme::fg("mdLink", "https://mistral.ai/news/mistral-vibe-2-0"),
                width,
            ));
        } else {
            lines.extend([String::new(), String::new()]);
        }
        lines.push(String::new());
        lines
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

/// The maximum period during which a hidden component needs animation redraws.
pub const fn animation_duration() -> Duration {
    DAX_ANIMATION
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
mod tests {
    use super::*;

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
    fn animation_duration_is_bounded() {
        assert!(animation_duration() <= Duration::from_secs(2));
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
}
