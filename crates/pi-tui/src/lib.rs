//! Terminal UI library — port of `packages/tui`.
//!
//! Differential line renderer, component tree, layout stacks, and the core
//! components (Text, Box, Input, SelectList, Loader, ScrollView). The input
//! model follows the upstream key-string surface ("enter", "ctrl+c", ...);
//! the terminal backend is crossterm.

pub mod autocomplete;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod latex;
pub mod layout;
pub mod mouse;
pub mod native_modifiers;
pub mod stdin_buffer;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod tui;
pub mod undo_stack;
pub mod utils;
pub mod word_navigation;

pub mod components {
    pub mod alt_screen;
    pub mod box_;
    pub mod cancellable_loader;
    pub mod editor;
    pub mod image;
    pub mod input;
    pub mod loader;
    pub mod markdown;
    pub mod scroll_view;
    pub mod select_list;
    pub mod settings_list;
    pub mod spacer;
    pub mod stack;
    pub mod text;
    pub mod truncated_text;
    pub use alt_screen::{
        extract_selection, find_alt_screen_search_matches, snap_selection_column,
        AltScreenFlashContainer, AltScreenSearchComponent, SearchMatch, SearchSegment,
        SelectionPoint, SelectionRange,
    };
    pub use box_::Box as BoxComponent;
    pub use cancellable_loader::CancellableLoader;
    pub use editor::{Editor, EditorOptions, EditorTheme};
    pub use image::Image;
    pub use input::Input;
    pub use loader::Loader;
    pub use markdown::Markdown;
    pub use scroll_view::ScrollView;
    pub use select_list::SelectList;
    pub use settings_list::{SettingItem, SettingsList};
    pub use spacer::Spacer;
    pub use stack::{HStack, VStack};
    pub use text::Text;
    pub use truncated_text::TruncatedText;
}
pub mod controller;

pub use autocomplete::{
    AutocompleteCommand, AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions,
    CombinedAutocompleteProvider, CompletionResult, SlashCommand,
};
pub use controller::{
    InputListenerHandle, TuiAltScreen, TuiMainScreen, TuiMainScreenRenderState, TuiMode,
    TuiStopOptions,
};
pub use keybindings::{
    get_keybindings, set_keybindings, KeybindingDefinition, KeybindingsConfig, KeybindingsManager,
    TUI_KEYBINDINGS,
};
pub use keys::{match_key, parse_key, TuiKey};
pub use latex::render_latex;
pub use layout::{
    allocate_stack_sizes, clamp_layout_line, get_scroll_view_box, get_scroll_views_at,
    get_scrollbar_geometry, render_layout_frame, visible_stack_entries, HStackLayout, LayoutAlign,
    LayoutBasis, LayoutBox, LayoutConstraint, LayoutDirection, LayoutFrame, LayoutNode, LayoutRect,
    LayoutViewport, ScrollLayoutNode, ScrollLayoutState, ScrollOverscroll, ScrollbarGeometry,
    ScrollbarMode, StackLayout, StackLayoutEntry, StackLayoutNode, VStackLayout,
};
pub use mouse::{
    decode_mouse_event, is_mouse_sequence, MouseButton, MouseDecodeError, MouseEvent,
    MouseEventKind, MouseModifiers,
};
pub use stdin_buffer::{SequenceStatus, StdinBuffer};
pub use terminal::{TerminalBackend, TerminalEvent};
pub use tui::{
    composite_tui_line, resolve_overlay_layout, Component, Container, OverlayAnchor, OverlayHandle,
    OverlayManager, OverlayMargin, OverlayOptions, OverlayRect, Scene, SharedComponent, SizeValue,
    Tree, CURSOR_MARKER,
};
pub use utils::{
    extract_ansi_code, extract_segments, get_grapheme_cell_range, get_osc8_link_at_column,
    grapheme_boundaries, next_grapheme_boundary, normalize_terminal_output,
    previous_grapheme_boundary, slice_by_column, slice_by_column_strict, slice_with_width,
    slice_with_width_info, strip_ansi_codes, strip_terminal_sequences, truncate_to_width,
    truncate_to_width_padded, visible_width, wrap_text_with_ansi, AnsiCode, ExtractedSegments,
    GraphemeCellRange, WidthSlice,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    #[test]
    fn tui_lib_loads() {}
}
