# pi-rust TUI parity status

This is the measured status register for every TUI capability in
`docs/EXHAUSTIVE-PARITY-INVENTORY.md` (TUI-001 through TUI-052). It is
separate from the 166-row source-conversion ledger.

`functional` means the capability is implemented to the inventory's required
behavioral contract. `evidence` means the required success, failure,
cancellation, malformed-input, repeat, and restart checks have current
recorded evidence. `visual/interaction` means the terminal presentation and
interaction have been manually compared against the pinned official Pi TUI at
the named terminal sizes and emulator; automated tests do not satisfy this
column. `overall` is calculated by `parity_audit tui` and is PASS only when all
three dimensions are PASS.

Status values are deliberately conservative:

- `PASS` — the dimension has complete current evidence for this row.
- `PARTIAL` — implementation or evidence exists, but at least one required
  boundary remains unverified.
- `OPEN` — no complete current evidence has been accepted.

The current checkpoint is intentionally not a 100% claim. The audit command
and the pre-commit hook recalculate the four percentages from this table and
require the generated values in the README, PLAN, HANDOFF, and this file to
stay synchronized.

| ID | Capability | Functional | Test/evidence | Visual/interaction | Current evidence or remaining boundary |
|---|---|---|---|---|---|
| TUI-001 | regular mode | PARTIAL | PARTIAL | OPEN | Regular PTY lifecycle passes; normalized official-vs-Rust startup captures matched at 100x30 and 80x24; full scrollback and manual comparison remain. |
| TUI-002 | fullscreen alt-screen | PARTIAL | PARTIAL | OPEN | Alt-screen PTY lifecycle passes; normalized official-vs-Rust startup captures matched at 100x30 and 80x24; nested/crash/manual comparison remains. |
| TUI-003 | resize | PARTIAL | PARTIAL | OPEN | Resize matrix and retained-layout tests pass; complete emulator review remains. |
| TUI-004 | differential renderer | PARTIAL | PARTIAL | OPEN | Differential/cursor tests pass; all churn and stale-cell cases remain. |
| TUI-005 | editor insertion | PARTIAL | PARTIAL | OPEN | Unicode/editor unit coverage, immediate cached-scene repaint, and real rapid-burst PTY echo exist; full inventory boundary evidence remains. |
| TUI-006 | editor deletion | PARTIAL | PARTIAL | OPEN | Grapheme/delete tests exist; complete interaction evidence remains. |
| TUI-007 | editor history | PARTIAL | PARTIAL | OPEN | History unit coverage exists; persistence-scope comparison remains. |
| TUI-008 | kill/yank/undo | PARTIAL | PARTIAL | OPEN | Core unit coverage exists; complete parity boundary evidence remains. |
| TUI-009 | word navigation | PARTIAL | PARTIAL | OPEN | Unicode/navigation tests exist; all terminal encodings remain. |
| TUI-010 | bracketed paste | PARTIAL | PARTIAL | OPEN | Real PTY proves sub-second large-paste marker echo and exact Unicode/multiline payload persistence after submit; remaining paste boundaries remain. |
| TUI-011 | autocomplete | PARTIAL | PARTIAL | OPEN | Completion tests exist; every source and cancel boundary remains. |
| TUI-012 | input buffer | PARTIAL | PARTIAL | OPEN | Fragmentation/timeout tests exist; complete overflow/EOF evidence remains. |
| TUI-013 | key decoding | PARTIAL | PARTIAL | OPEN | Key protocol tests and the real Kitty CSI-u press/release regression pass; the full emulator matrix remains. |
| TUI-014 | keybinding config | PARTIAL | PARTIAL | OPEN | Registry/conflict tests exist; reload/extension precedence evidence remains. |
| TUI-015 | slash command menu | PARTIAL | PARTIAL | OPEN | Slash completion is exercised; full menu interaction evidence remains. |
| TUI-016 | slash command execution | PASS | PASS | OPEN | All registered commands pass the real PTY inventory; manual visual comparison remains. |
| TUI-017 | model selector | PARTIAL | PARTIAL | OPEN | Selector paths are covered in local matrices; complete auth/search evidence remains. |
| TUI-018 | thinking selector | PARTIAL | PARTIAL | OPEN | Selector paths and one-step Kitty Up navigation are covered; provider-limit and persistence evidence remains. |
| TUI-019 | settings selector | PARTIAL | PARTIAL | OPEN | Settings tests exist; every nested cancel/reload boundary remains. |
| TUI-020 | theme picker/controller | PARTIAL | PARTIAL | OPEN | Theme tests/reload path exist; complete visual palette review remains. |
| TUI-021 | login dialog | PARTIAL | PARTIAL | OPEN | Complete real auth PTY matrix passes 5/5: browser callback, device code, browser cancellation/guidance, llama key/URL, and Qwen Token Plan bracketed API-key paste with masking/persistence/logout; component tests cover grapheme-safe editing and split-marker cancellation. Provider/manual visuals remain open. |
| TUI-022 | session picker | PARTIAL | PARTIAL | OPEN | Session routing and search-state component tests pass; full picker/delete/rename interaction evidence remains. |
| TUI-023 | tree selector | PARTIAL | PARTIAL | OPEN | Parent-linked selection, filter controls, ancestor visibility, and non-stop assistant rows are tested; complete search/streaming-guard evidence remains. |
| TUI-024 | trust selector | PARTIAL | PARTIAL | OPEN | Trust PTY paths exist; full save/cancel/navigation evidence remains. |
| TUI-025 | modal overlays | PARTIAL | PARTIAL | OPEN | Overlay geometry/focus tests pass; complete stacking visual review remains. |
| TUI-026 | markdown | PARTIAL | PARTIAL | OPEN | Markdown unit/snapshot coverage passes; full source boundary comparison remains. |
| TUI-027 | assistant rendering | PARTIAL | PARTIAL | OPEN | Streaming renderer coverage exists; all stop/error/usage variants remain. |
| TUI-028 | user/tool rendering | PARTIAL | PARTIAL | OPEN | Live lifecycle/tool PTYs and sequential `!`/`!!` Bash projection pass; custom/image/compaction boundary evidence remains. |
| TUI-029 | footer/status | PARTIAL | PARTIAL | OPEN | Footer/status paths are exercised; complete width/usage visual evidence remains. |
| TUI-030 | images/terminal graphics | PARTIAL | PARTIAL | OPEN | Parent-verified pi-tui all-targets now pass 386 library tests plus every integration target, including explicit image-dimension overrides, cell-size cache invalidation, Kitty/iTerm2 sizing/placement, capability fallback, and image protocol metadata; emulator-specific visual evidence remains. |
| TUI-031 | external editor | PARTIAL | PARTIAL | OPEN | External-editor paths exist; every cancel/failure terminal boundary remains. |
| TUI-032 | clipboard | PARTIAL | PARTIAL | OPEN | Clipboard component fixtures cover Wayland/X11 MIME handling and image paths; all backend environments remain. |
| TUI-033 | interrupt/cancel | PARTIAL | PARTIAL | OPEN | Control matrix passes core paths; every operation/modal boundary remains. |
| TUI-034 | quit/shutdown | PARTIAL | PARTIAL | OPEN | Quit/restoration matrices pass; all child/signal boundaries remain. |
| TUI-035 | terminal portability | PARTIAL | PARTIAL | OPEN | Parent-verified terminal protocol tests cover tmux/TERM detection, escape-timeout normalization, keyboard-protocol fallback, resize, raw-mode restoration, and no-op pre-start lifecycle; Linux PTYs pass, while no-display/cross-platform and manual terminal matrix evidence remains. |
| TUI-036 | hidden `/debug` command | PASS | PASS | OPEN | Real PTY and unit evidence pass; manual output comparison remains. |
| TUI-037 | hidden `/arminsayshi` command | PASS | PASS | OPEN | Real PTY, timing, width, and cleanup evidence pass; visual comparison remains. |
| TUI-038 | hidden `/dementedelves` command | PASS | PASS | OPEN | Real PTY, resize, width, and cleanup evidence pass; visual comparison remains. |
| TUI-039 | OpenCode/Kimi Daxnuts | PASS | PASS | OPEN | Payload/trigger/PTY evidence pass; manual animation comparison remains. |
| TUI-040 | application keybinding registry | PARTIAL | PARTIAL | OPEN | Parent-verified controller/action dispatch, conflict, unknown-key, and Kitty-release tests are included in 37 focused controller cases and the 386-test pi-tui all-target gate; complete action/extension precedence review remains. |
| TUI-041 | suspend/resume | PARTIAL | PARTIAL | OPEN | Parent-verified alternate-screen suspend/resume, cursor/protocol restoration, and resize lifecycle cases pass in the controller/all-target gate; active-operation, signal, emulator, and cross-platform evidence remains. |
| TUI-042 | selection/search/scrollback | PARTIAL | PARTIAL | OPEN | Parent-verified selection highlighting, search-release handling, scroll actions, scrollback stop, resize, and fullscreen document restoration cases pass in the 386-test all-target gate; full integrated return-to-editor and visual evidence remains. |
| TUI-043 | native image/clipboard backends | PARTIAL | PARTIAL | OPEN | Fallback tests pass; Wayland/X11/Termux runtime evidence remains. |
| TUI-044 | Mermaid rendering | PARTIAL | PARTIAL | OPEN | Supported/unsupported renderer tests pass; full cancellation visual evidence remains. |
| TUI-045 | loader animation and cancellation | PASS | PASS | OPEN | Exact-frame/interval/callback/cancellation tests pass; manual timing remains. |
| TUI-046 | interactive render scheduler | PARTIAL | PARTIAL | OPEN | Parent-verified owner-render coalescing, deferred dispatch repaint, cached-scene cursor/overlay invalidation, unchanged-frame suppression, and controller scheduler cases pass; real composer latency remains within the existing 20-sample p95/max 3.98 ms evidence, while full cadence, stream/tool, resize, and visual-diff boundaries remain. |
| TUI-047 | transient animated surfaces | PARTIAL | PARTIAL | OPEN | Parent-verified scrollbar activity/expiry tests cover repeated-activity generation rearming, current repaint callback, saturating clocks, mode transitions, and narrow-width visibility; every expiry/cadence boundary and visual timing comparison remains. |
| TUI-048 | terminal progress keepalive | PASS | PASS | OPEN | OSC keepalive/clear tests pass; emulator visual review remains. |
| TUI-049 | hidden animated components | PASS | PASS | OPEN | Armin/Daxnuts/Earendil timing/width/cleanup evidence passes; visual review remains. |
| TUI-050 | loader/status animation integration | PARTIAL | PARTIAL | OPEN | Loader/status lifecycle is wired; the active normal and queued-follow-up label now matches upstream `Working...` with a separate queued count, covered by `working_loader_uses_upstream_label_for_each_turn_kind` and a real Alt+Enter queued-follow-up PTY. Countdown/retry/compaction and visual timing matrices remain. |
| TUI-051 | Pi-style tool execution display | PASS | PASS | OPEN | Live Codex release PTY and renderer lifecycle tests pass; visual comparison remains. |
| TUI-052 | concurrent Pi process coexistence | PASS | PASS | OPEN | Two Rust instances pass real concurrent isolation/migration tests; official Pi 0.84.3 and release pi-rust also ran concurrently in isolated tmux PTYs at 100x30 and exited cleanly. Terminal visual comparison remains open. |

## Current measured percentages

<!-- PARITY_AUDIT_TUI_METRICS:START -->
TUI functional implementation: 19.23% (10/52)
TUI test/evidence parity: 19.23% (10/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
<!-- PARITY_AUDIT_TUI_METRICS:END -->

The latest workspace revalidation passes 381 pi-tui library tests plus every
integration target, strict pi-tui/workspace clippy, and the optimized release
build. The newest serialized package rerun passes 382 pi-tui library tests
plus every integration target and strict pi-tui check/clippy. The JSON/session
wave also has release-binary evidence, but these
package gates do not promote TUI rows: functional/evidence remains 10/52 and
visual/interaction/overall remains 0/52 until each row has integrated behavior
and terminal visual/interaction proof.

These values are generated from the table above. They are not the historical
`Conversion progress: 100.00% (166/166; 0 open)` source-ledger value.

The latest parent package gate passes 378 pi-tui library tests plus every
integration target and strict all-target clippy after the selector, overlay,
and shared key-release fixes. The row dimensions remain unchanged because
complete behavior and manual visual comparison are still outstanding.

The 2026-08-29 optimized workspace all-targets gate, strict clippy, release
binary smoke checks, and real auth/settings/composer/PTy suites pass. This is
stronger runtime evidence for the listed rows, but it does not close their
manual visual/interaction column or the remaining per-row behavioral
boundaries; TUI-052 is now reflected as PASS for functional and test/evidence
dimensions while visual remains open.

The follow-up terminal/image/input gate passes 409 pi-tui all-target tests,
including the 359-library-test suite and every integration target, plus strict
pi-tui clippy, full formatting, and scoped diff checks. TUI-030 and TUI-035
remain PARTIAL because emulator-specific graphics and the complete terminal
portability/manual comparison matrix are not yet evidenced.

The latest controller slice additionally passes 360 pi-tui library tests plus
every integration target and strict clippy, covering deferred/coalesced owner
repaint, hardware-cursor placement/toggles, overlay lifecycle, scrollback
preservation on regular stop, shrink/resize repaint, and fullscreen document
restoration. These automated results do not promote the visual/interaction
column or close the remaining row-level boundaries.

## Synchronized dashboard checkpoint

Source/conversion ledger: 100.00% (166/166; 0 open)
Acceptance inventory census: 100.00% (318/318) (318 IDs indexed)
Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)
Root acceptance gates: 100.00% (8/8) (8 passed; 0 open)
Rust-only distribution boundary: 100.00% (0 JS/TS executable source files; generated Rustdoc excluded)
TUI functional implementation: 19.23% (10/52)
TUI test/evidence parity: 19.23% (10/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
Non-TUI implementation parity: 18.42% (49/266 PASS; 194 PARTIAL; 23 OPEN)
Non-TUI deterministic evidence parity: 13.53% (36/266 PASS; 207 PARTIAL; 23 OPEN)
Non-TUI runtime-boundary parity: 13.91% (37/266 PASS; 154 PARTIAL; 75 OPEN)
Non-TUI overall parity: 11.28% (30/266)
Whole-product behavioral parity: 9.43% (30/318)

The definitions and refresh command are maintained in
[`PARITY-DASHBOARD.md`](PARITY-DASHBOARD.md).

## Latest parent verification — 2026-08-29

The current `pi-tui` tree passes 380 library tests plus every integration
target, strict all-target clippy, full stable rustfmt, scoped diff checks, and
the repository Rust trailing-whitespace scan. This confirms package health,
not completion of the TUI parity rows: functional/evidence remains 10/52 and
visual/interaction/overall remains 0/52 until every row has integrated
behavior and manual or equivalent terminal-render evidence.

The latest serialized pi-tui gate passes 386 library tests plus every
integration target, strict all-target clippy, stable formatting, and scoped
diff checks. This updates package evidence only: functional/evidence remains
10/52 and visual/interaction/overall remains 0/52 until the integrated
behavioral and terminal visual boundaries are individually accepted.

The Rust-idiom campaign checkpoints (pi-evals, pi-server typed errors and
lint gates) do not touch TUI rows: functional/test-evidence remains 10/52
and visual/interaction/overall remains 0/52.

Phase 2.2 (pi-client) likewise does not touch TUI rows.

Phase 2.3a (pi-ai) likewise does not touch TUI rows.

Phase 2.3b (PiAiError) likewise does not touch TUI rows.

Phase 2.4 (pi-agent) likewise does not touch TUI rows.
