# Gates: interactive slash-command fixture checkpoint

OWNS: crates/pi-tui/src/terminal_image.rs, crates/pi-coding-agent/src/interactive/**, crates/pi-coding-agent/src/modes/interactive.rs, crates/pi-coding-agent/tests/interactive_slash_pty.rs, crates/pi-coding-agent/tests/fixtures/interactive/**, CONVERSION-LEDGER.md, PLAN.md, HANDOFF.md, README.md, .github/repository-description.txt

Scope: add deterministic interactive slash-command fixture coverage and close any verified dispatch gaps without claiming the broader terminal matrix is complete.

- [x] G1: the interactive slash-command fixture exercises the registered command surface and records expected terminal outcomes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.16s

- [x] G2: interactive unit regressions and the shared compaction path remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..................................... | test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 427 filtered out; finished in 0.04s

- [x] G3: the first uncached terminal capability detection cannot deadlock on cache publication
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline terminal_image::tests::uncached_capability_detection_releases_read_lock_before_write --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 186 filtered out; finished in 0.00s

- [x] G4: the coding-agent crate still type-checks after the dispatch changes
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline && printf "coding-agent-check-passed\\n"'
  EXPECT: coding-agent-check-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=coding-agent-check-passed | Finished `dev` profile [optimized + debuginfo] target(s) in 0.09s

- [x] G5: formatting and whitespace validation are clean
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && printf "format-and-diff-check-passed\\n"'
  EXPECT: format-and-diff-check-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=format-and-diff-check-passed

- [x] G6: the conversion ledger and synchronized docs report the measured progress
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: /Conversion progress: [0-9]+\.[0-9]+% \([0-9]+\/[0-9]+; [0-9]+ open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 59.64% (99/166; 67 open)
