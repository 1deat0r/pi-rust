# Gates: leaf-E1 pi-tui terminal/TUI parity

OWNS: crates/pi-tui/**

Scope: close the requested upstream terminal and TUI parity contracts with deterministic pi-tui fixtures while preserving intentional external-terminal nonports.

- [ ] E1-G1: complete pi-tui parity and regression tests pass
  CHECK: cargo test -p pi-tui --offline --quiet && node -e "process.stdout.write('PI_TUI_FULL_TESTS_PASS\\n')"
  EXPECT: PI_TUI_FULL_TESTS_PASS
  EVIDENCE: pending

- [ ] E1-G2: pi-tui compiles and strict clippy is clean
  CHECK: cargo check -p pi-tui --offline && cargo clippy -p pi-tui --offline --all-targets -- -D warnings && node -e "process.stdout.write('PI_TUI_CHECK_CLIPPY_PASS\\n')"
  EXPECT: PI_TUI_CHECK_CLIPPY_PASS
  EVIDENCE: pending

- [ ] E1-G3: pi-tui formatting and repository diff whitespace checks pass
  CHECK: cargo fmt --manifest-path crates/pi-tui/Cargo.toml -- --check && git diff --check && node -e "process.stdout.write('PI_TUI_FORMAT_DIFF_PASS\\n')"
  EXPECT: PI_TUI_FORMAT_DIFF_PASS
  EVIDENCE: pending

- [ ] E1-G4: terminal state and concurrency review finds no unresolved in-scope leak
  EVIDENCE: pending

- [ ] E1-G5: all changes remain within the requested pi-tui ownership scope
  EVIDENCE: pending
