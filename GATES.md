# Gates: interactive slash-command fixture checkpoint

OWNS: crates/pi-telemetry/src/lib.rs, crates/pi-ai/src/models.rs, crates/pi-ai/src/images.rs, crates/pi-ai/src/api/lazy.rs, crates/pi-ai/src/api/openrouter_images.rs, crates/pi-ai/src/providers/faux.rs, crates/pi-coding-agent/src/main.rs, crates/pi-coding-agent/src/run.rs, crates/pi-coding-agent/src/modes/interactive.rs, crates/pi-coding-agent/src/modes/rpc.rs, crates/pi-coding-agent/src/core/model_runtime.rs, crates/pi-coding-agent/src/core/model_registry.rs, crates/pi-coding-agent/src/core/provider_composer.rs, crates/pi-coding-agent/src/core/models_store.rs, crates/pi-coding-agent/tests, CONVERSION-LEDGER.md, PLAN.md, HANDOFF.md, README.md, .github/repository-description.txt

Scope: preserve the completed interactive slash-command, project-trust, deferred-response, and image gates while restoring the strict zero-warning clippy baseline in the telemetry dependency path.

- [x] G1: the interactive slash-command fixture exercises the registered command surface and records expected terminal outcomes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.79s

- [x] G2: interactive unit regressions and the shared compaction path remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..................................... | test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 433 filtered out; finished in 0.03s

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
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open)

## Next slice: close the interactive `/resume` behavior audit

- [x] G7: the real PTY fixture seeds a second session, opens `/resume`, selects it, and rehydrates its transcript
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_pty --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.61s

- [x] G8: the session-picker helpers and interactive dispatch regressions remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..................................... | test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 433 filtered out; finished in 0.03s

- [x] G9: the coding-agent crate type-checks and the touched tree is formatted with no whitespace errors
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline && /home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && printf "resume-slice-checks-passed\\n"'
  EXPECT: resume-slice-checks-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=resume-slice-checks-passed | Finished `dev` profile [optimized + debuginfo] target(s) in 0.08s

- [x] G10: the progress checker remains valid after the completed S-033 slice
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: /Conversion progress: [0-9]+\.[0-9]+% \([0-9]+\/[0-9]+; [0-9]+ open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open)

## Next slice: close S-036 project-trust safety parity

- [x] G11: project trust resolves saved decisions, global defaults, and CLI overrides across the run modes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_trust --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=....... | test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s

- [x] G12: trust-resource detection, nearest-ancestor lookup, option shape, and lock-safe persistence remain green
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::project_trust --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=....... | test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 463 filtered out; finished in 0.01s

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
  EXPECT: /Conversion progress: 62\.65% \(104\/166; 62 open\)/
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open)

## Next slice: close S-005/S-006 deferred-response runtime parity

- [x] G16: the coding-agent deferred runtime can submit, resolve, and cancel a
      faux deferred response through the same auth-applied Models facade used
      by the modes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::model_runtime::tests::deferred_runtime --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 469 filtered out; finished in 0.00s

- [x] G17: provider composition and models.json overlays preserve deferred
      fetch/cancel capabilities while dispatching by the selected model API
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::provider_composer --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=............... | test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 455 filtered out; finished in 0.00s

- [x] G18: interactive, print, JSON, and RPC faux mode registrations expose
      deferred fetch/cancel instead of silently dropping the provider hooks
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib deferred_mode_wiring --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=. | test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 469 filtered out; finished in 0.00s

- [x] G19: lazy capability declarations expose only requested deferred methods
      and return the upstream missing-capability diagnostics
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --lib api::lazy --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=.. | test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 288 filtered out; finished in 0.00s

- [x] G20: formatting, whitespace, and exhaustive conversion progress remain
      clean after the deferred runtime slice
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && node scripts/conversion-progress.mjs && printf "deferred-slice-checks-passed\\n"'
  EXPECT: deferred-slice-checks-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open) | deferred-slice-checks-passed

## Next slice: close S-007 image retry and terminal classification parity

- [x] G21: the OpenRouter image adapter preserves the upstream request,
      response, usage, and retry contract
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --lib api::openrouter_images --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=.......... | test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 280 filtered out; finished in 0.03s

- [x] G22: image retry honors HTTP-date `Retry-After` values and aborts an
      in-flight retry backoff without issuing another request
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --lib api::openrouter_images::retry_tests --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=..... | test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 285 filtered out; finished in 0.03s

- [x] G23: quota/billing failures remain terminal while transient provider,
      transport, and explicit retry guidance remain retryable
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --lib utils::retry --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=................ | test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 274 filtered out; finished in 0.03s

- [x] G24: the image facade preserves provider registration and error-encoded
      output semantics after the retry/cancellation changes
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --lib images --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=................... | test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 271 filtered out; finished in 0.03s

- [x] G25: the image slice is formatted, whitespace-clean, progress-accounted,
      and ready for a synchronized checkpoint
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && node scripts/conversion-progress.mjs && printf "image-slice-checks-passed\\n"'
  EXPECT: image-slice-checks-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open) | image-slice-checks-passed

## Next cleanup: restore the strict clippy baseline

- [x] G26: the telemetry crate's async span path remains behaviorally green
      while avoiding a mutex guard across an await
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-telemetry --offline --quiet
  EXPECT: test result: ok
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; output=6 passed; 0 failed; cargo test also reported the zero-test binary target

- [x] G27: pi-telemetry passes all-target clippy with warnings denied
  CHECK: /home/mustbearnold/.cargo/bin/cargo clippy -p pi-telemetry --offline --all-targets -- -D warnings
  EXPECT: Finished `dev` profile
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Finished `dev` profile [optimized + debuginfo] target(s) in 0.06s

- [x] G28: pi-ai and its telemetry dependency pass the strict clippy gate
  CHECK: /home/mustbearnold/.cargo/bin/cargo clippy -p pi-ai --offline --all-targets -- -D warnings
  EXPECT: Finished `dev` profile
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Finished `dev` profile [optimized + debuginfo] target(s) in 0.07s

- [x] G29: the cleanup checkpoint remains formatted, whitespace-clean, and
      progress-accounted
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check && git diff --check && node scripts/conversion-progress.mjs && printf "clippy-cleanup-checks-passed\\n"'
  EXPECT: clippy-cleanup-checks-passed
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=06dd37959781/23 entries; output=Conversion progress: 62.65% (104/166; 62 open) | clippy-cleanup-checks-passed
