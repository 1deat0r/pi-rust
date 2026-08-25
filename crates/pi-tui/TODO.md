# pi-tui — current conversion notes

The terminal backend, alternate-screen lifecycle, key parsing, kitty/ITerm
image paths, cell dimensions/capability probes, colors, editor, autocomplete,
markdown, settings/select lists, loaders, overlays, fuzzy matching, kill-ring,
undo, word navigation, stdin buffering, native modifiers, and interactive
integration are covered by checked rows #59–70 and S-050–S-057.

Evidence:

- cargo test -p pi-tui --offline --quiet
- cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
- cargo test -p pi-coding-agent --offline --test interactive_full_matrix --quiet
- cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet

Terminal feature probes and PTY checks remain evidence-tiered live/local
fixtures in the ledger; they are not restated as an open “not yet ported”
surface here.
