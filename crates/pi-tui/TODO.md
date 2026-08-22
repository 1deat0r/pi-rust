# pi-tui — port status

Core library landed (P7 core, Session 10): crossterm terminal backend with
raw/alt-screen + pi key-string events, Component trait + Scene +
differential line renderer, flex constraint solver, and the core components
(Text/Spacer/VStack/HStack/Box/Loader/SelectList/ScrollView/TruncatedText/
Input). 20 tests. Interactive mode (pi-coding-agent modes/interactive.rs)
uses this subset and is tmux-verified.

## Not yet ported (upstream mapping)
- Editor component (multi-line editing with selection/IME), Markdown
  renderer, Image (sixel/iTerm/kitty), SettingsList, CancellableLoader,
  alt-screen overlays/flash, terminal-image capability probing + cell
  dimensions, terminal-colors (OSC 11), latex subset, autocomplete, fuzzy,
  kill-ring, undo-stack, word-navigation, stdin-buffer, native-modifiers,
  alt-screen-search, editor-component.
- The interactive-mode component library (armin/daxnuts/earendil/footer/
  model/theme selectors etc.) and the full interactive feature set.
