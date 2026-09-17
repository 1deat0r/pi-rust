# GATES archive — 2026-08-27 launch/live revalidation (append-only, frozen)

> Archived 2026-09-17 by the doc-hygiene pass. Frozen history: do not edit
> except to fix corruption. The open G54 item (318-ID live evidence) is
> tracked by the parity audits, not by this file. Preserved verbatim below.

## Current launch and live-provider revalidation — 2026-08-27

- [x] G51: the installed `pi-rust` command resolves to the optimized Rust
  binary while the official `pi` command remains independently runnable.
  CHECK: `target/release/parity_audit installed`
  EXPECT: `PARITY_INSTALLED_RUST_OK command=pi-rust release_version=pi 0.85.1 official_pi_version=0.84.3`
  EVIDENCE: exact expected output passed; `pi-rust` resolves to
  `/run/media/mustbearnold/Projects/AI Agents/pi-rust/target/release/pi`,
  while `pi` resolves to the independent official Pi 0.84.3 installation.

- [x] G52: real OpenAI Codex OAuth works through the installed print command
  for sequential turns without exposing credentials.
  CHECK: release `pi-rust --print --provider openai-codex --model gpt-5.5
  --no-tools`, followed by `--continue`.
  EXPECT: exact redacted test responses and persisted user/assistant pairs.
  EVIDENCE: two sequential live turns passed on 2026-08-26; no credential
  value was printed or persisted in the repository.

- [x] G53: the release interactive TUI reaches the real login selector and
  browser OAuth URL, cancels safely, and completes two live turns.
  CHECK: release binary in a real tmux PTY with `/login`, `/login
  openai-codex`, Escape, and two prompts.
  EXPECT: selector, `auth.openai.com` URL, `Login cancelled`, exact live replies.
  EVIDENCE: all observed across the real auth PTY suite; the current release
  rerun is 5 passed, 0 failed, including Qwen Token Plan bracketed API-key
  paste, persistence, masking, and logout.

- [ ] G54: all 318 exhaustive inventory IDs and every credentialed provider's
  live refresh/restart/error-recovery paths have current evidence.
  CHECK: `.unlazy/parity-20260827/GATES.md` R1–R8.
  EXPECT: no pending root evidence.
  EVIDENCE: intentionally open; current evidence is partial and is not being
  represented as 1:1 or flawless parity.

## Current serialized verification — 2026-08-27

- The selector/full-matrix/release-multiturn PTY run passed 20/20. Its Kitty
  CSI-u regression confirms that a release event is ignored and one Up/Down
  press changes the selected row exactly once.
- The release binary was rebuilt from the current worktree without warnings;
  `target/release/pi --version` reports `pi 0.85.1`, and the international
  Qwen Token Plan catalog is listed by `--offline --list-models
  qwen-token-plan`.
- The release authentication PTY suite passed 5/5. Its Qwen case proves the
  real `/login qwen-token-plan` path accepts bracketed paste, masks the secret
  in the terminal, persists the API-key credential, and removes it with
  `/logout qwen-token-plan`.
- Strict clippy is green for `pi-coding-agent` and `pi-ai` with all targets;
  the prior workspace package-set clippy gate is also green. Formatting and
  whitespace checks are green. The complete workspace release test suite
  exited 0, including 680 `pi-coding-agent` library tests and the interactive
  PTY matrices. R8 is closed by the independent official-Pi visual/interaction
  review recorded in `.unlazy/parity-20260827/GATES.md`; per-capability visual
  review remains tracked separately in `docs/TUI-PARITY-STATUS.md`.
