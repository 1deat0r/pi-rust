//! Loader component — animated spinner line.

use std::time::Instant;

use crate::tui::Component;

const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

pub struct Loader {
    message: String,
    started: Instant,
}

impl Loader {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), started: Instant::now() }
    }
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let _ = width;
        let frame = FRAMES[((self.started.elapsed().as_millis() / 120) as usize) % FRAMES.len()];
        vec![format!(" {frame} {}", self.message)]
    }
}
