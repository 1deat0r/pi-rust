# pi-tui — port status

Core library landed (P7 core, Session 10): crossterm terminal backend with
raw/alt-screen + pi key-string events, Component trait + Scene +
differential line renderer, flex constraint solver, and the core components
(Text/Spacer/VStack/HStack/Box/Loader/SelectList/ScrollView/TruncatedText/
Input). 20 tests. Interactive mode (pi-coding-agent modes/interactive.rs)
uses this subset and is tmux-verified.

## Done (Session 11 — full TUI surface + interactive mode)
- Pure logic: fuzzy, kill-ring, undo-stack, word-navigation, terminal-colors, native-modifiers,
  LaTeX (91 upstream parity cases), autocomplete (fuzzy + fd path + slash).
- Components: full SelectList, multi-line Editor (history, kill/yank, undo coalescing, paste markers,
  autocomplete), Markdown block renderer, Image + terminal-image capability probing, SettingsList,
  CancellableLoader, alt-screen flash/search overlays.
- keybindings registry + KeybindingsManager, stdin-buffer (bracketed paste + kitty CSI-u dedup),
  Esc/escape normalization.
- coding-agent interactive mode: Editor-driven loop with autocomplete, slash-commands registry +
  dispatch, selectors (model/thinking/theme/settings), footer, streaming markdown, tmux-verified.
- pi-tui 176 lib tests; pi-coding-agent 142 (6 interactive).
- ICU-style word segmentation landed (Session 15, T5 #63/64): `word_navigation.rs`
  steps each CJK ideograph as its own word-like segment to match upstream
  `Intl.Segmenter`; the Editor's Ctrl+arrow word nav adopts it. 18 tests.
- Token-total footer reads landed (Session 15, T5 #67/68): `formatTokens` +
  `render_usage_stats` (`↑input ↓output Rcache Wcache CH{rate}% $cost`) driven by
  cumulative transcript usage + per-turn cache-hit-rate. 5 footer tests.
- Remaining: full alt-screen screen-swap, tmux client_termfeatures probe,
  ConfigSelector full TUI component (its data layer + resolve producer are
  landed in `pi-coding-agent`; the interactive render surface is PTY-bound).

## Not yet ported (upstream mapping)
- Editor component (multi-line editing with selection/IME), Markdown
  renderer, Image (sixel/iTerm/kitty), SettingsList, CancellableLoader,
  alt-screen overlays/flash, terminal-image capability probing + cell
  dimensions, terminal-colors (OSC 11), latex subset, autocomplete, fuzzy,
  kill-ring, undo-stack, word-navigation, stdin-buffer, native-modifiers,
  alt-screen-search, editor-component.
- The interactive-mode component library (armin/daxnuts/earendil/footer/
  model/theme selectors etc.) and the full interactive feature set.
