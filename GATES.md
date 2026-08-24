# Gates: interactive slash-command fixture checkpoint

OWNS: crates/pi-coding-agent/src/main.rs, crates/pi-coding-agent/src/run.rs, crates/pi-coding-agent/src/commands/**, crates/pi-coding-agent/src/core/project_trust.rs, crates/pi-coding-agent/src/core/settings.rs, crates/pi-coding-agent/tests/cli_trust.rs, CONVERSION-LEDGER.md, PLAN.md, HANDOFF.md, README.md, .github/repository-description.txt

Scope: preserve the completed interactive slash-command gates while closing the project-trust safety matrix across mode startup, resource loaders, saved/default decisions, CLI overrides, prompts, and lock-safe trust persistence.

- [x] G1: the interactive slash-command fixture exercises the registered command surface and records expected terminal outcomes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.62s

- [x] G2: interactive unit regressions and the shared compaction path remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..................................... | test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 429 filtered out; finished in 0.04s

- [x] G3: the first uncached terminal capability detection cannot deadlock on cache publication
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline terminal_image::tests::uncached_capability_detection_releases_read_lock_before_write --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 186 filtered out; finished in 0.00s

- [x] G4: the coding-agent crate still type-checks after the dispatch changes
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline && printf "coding-agent-check-passed\\n"'
  EXPECT: coding-agent-check-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=coding-agent-check-passed | Finished `dev` profile [optimized + debuginfo] target(s) in 0.08s

- [x] G5: formatting and whitespace validation are clean
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && printf "format-and-diff-check-passed\\n"'
  EXPECT: format-and-diff-check-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=format-and-diff-check-passed

- [x] G6: the conversion ledger and synchronized docs report the measured progress
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: /Conversion progress: [0-9]+\.[0-9]+% \([0-9]+\/[0-9]+; [0-9]+ open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 60.84% (101/166; 65 open)

## Next slice: close the interactive `/resume` behavior audit

- [x] G7: the real PTY fixture seeds a second session, opens `/resume`, selects it, and rehydrates its transcript
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.61s

- [x] G8: the session-picker helpers and interactive dispatch regressions remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..................................... | test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 429 filtered out; finished in 0.04s

- [x] G9: the coding-agent crate type-checks and the touched tree is formatted with no whitespace errors
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline && /home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && printf "resume-slice-checks-passed\\n"'
  EXPECT: resume-slice-checks-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=resume-slice-checks-passed | Finished `dev` profile [optimized + debuginfo] target(s) in 0.08s

- [x] G10: the progress checker remains valid after the completed S-033 slice
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: /Conversion progress: [0-9]+\.[0-9]+% \([0-9]+\/[0-9]+; [0-9]+ open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 60.84% (101/166; 65 open)

## Next slice: close S-036 project-trust safety parity

- [x] G11: project trust resolves saved decisions, global defaults, and CLI overrides across the run modes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_trust --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=....... | test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s

- [x] G12: trust-resource detection, nearest-ancestor lookup, option shape, and lock-safe persistence remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::project_trust --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=....... | test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 459 filtered out; finished in 0.01s

- [x] G13: all coding-agent callers use the resolved project-trust setting and type-check cleanly
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline && printf "project-trust-check-passed\\n"'
  EXPECT: project-trust-check-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=project-trust-check-passed | Finished `dev` profile [optimized + debuginfo] target(s) in 0.08s

- [x] G14: formatting and whitespace validation are clean after the trust-resolution changes
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && printf "project-trust-format-passed\\n"'
  EXPECT: project-trust-format-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=project-trust-format-passed

- [x] G15: closing S-036 updates the exhaustive progress checker consistently
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: /Conversion progress: 60\.84% \(101\/166; 65 open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 60.84% (101/166; 65 open)
