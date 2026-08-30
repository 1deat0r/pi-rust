# Gates: leaf-E1 pi-tui terminal/TUI parity

This leaf is part of the active Rust-only exhaustive behavioral audit. The
older conversion percentage and Node-based commands are historical and do not
certify TUI parity. The complete acceptance list is in
`../../docs/EXHAUSTIVE-PARITY-INVENTORY.md` (`TUI-001` through `TUI-039`).

OWNS: crates/pi-tui/**

Scope: close the requested upstream terminal and TUI parity contracts with deterministic pi-tui fixtures while preserving intentional external-terminal nonports.

Latest parent verification (2026-08-29): the package all-target matrix passes
386 library tests plus every integration target, strict clippy passes, and the
full stable formatting/diff checks are clean. The 52-row register remains the
acceptance authority; automated package health does not establish visual
parity.

- [x] E1-G1: complete pi-tui parity and regression tests pass
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --all-targets -- --test-threads=1
  EXPECT: every test target exits 0
  EVIDENCE: exit=0; `running 386 tests` => 386 passed; all integration targets exit 0.

- [x] E1-G2: pi-tui compiles and strict clippy is clean
  CHECK: /home/mustbearnold/.cargo/bin/cargo check -p pi-tui --offline --all-targets && /home/mustbearnold/.cargo/bin/cargo clippy -p pi-tui --offline --all-targets -- -D warnings
  EXPECT: check and clippy exit 0 with no warnings
  EVIDENCE: exit=0 for check and strict all-target clippy.

- [x] E1-G3: pi-tui formatting and repository diff whitespace checks pass
  CHECK: /home/mustbearnold/.cargo/bin/cargo fmt --manifest-path crates/pi-tui/Cargo.toml -- --check && git diff --check
  EXPECT: no formatting or diff-check output
  EVIDENCE: stable cargo fmt --check, rustfmt changed-path check, and git diff --check exit 0.

- [ ] E1-G4: terminal state and concurrency review finds no unresolved in-scope leak
  EVIDENCE: pending

- [ ] E1-G5: all changes remain within the requested pi-tui ownership scope
  EVIDENCE: pending
