//! Terminal UI library — port of `packages/tui`.
//!
//! Differential line renderer, component tree, layout stacks, and the core
//! components (Text, Box, Input, SelectList, Loader, ScrollView). The input
//! model follows the upstream key-string surface ("enter", "ctrl+c", ...);
//! the terminal backend is crossterm.

pub mod fuzzy;
pub mod keys;
pub mod kill_ring;
pub mod layout;
pub mod native_modifiers;
pub mod terminal;
pub mod terminal_colors;
pub mod tui;
pub mod undo_stack;
pub mod utils;
pub mod word_navigation;

pub mod components {
    pub mod box_;
    pub mod input;
    pub mod loader;
    pub mod scroll_view;
    pub mod select_list;
    pub mod spacer;
    pub mod stack;
    pub mod text;
    pub mod truncated_text;
    pub use box_::Box as BoxComponent;
    pub use input::Input;
    pub use loader::Loader;
    pub use scroll_view::ScrollView;
    pub use select_list::SelectList;
    pub use spacer::Spacer;
    pub use stack::{HStack, VStack};
    pub use text::Text;
    pub use truncated_text::TruncatedText;
}

pub use keys::{match_key, parse_key, TuiKey};
pub use layout::{HStackLayout, LayoutConstraint, StackLayout, VStackLayout};
pub use terminal::{TerminalBackend, TerminalEvent};
pub use tui::{Component, Scene, SharedComponent, Tree};
pub use utils::{slice_with_width, visible_width, wrap_text_with_ansi};

#[cfg(test)]
mod tests {
    #[test]
    fn tui_lib_loads() {}
}
