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

## Next slice: close S-011 Vertex ADC file and provider parity

- [x] G30: Vertex ADC service-account and authorized-user files use their
      configured token URI, scopes, and refresh credentials
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex --quiet && printf 'S011_VERTEX_ADC_TESTS_PASS\n'
  EXPECT: S011_VERTEX_ADC_TESTS_PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 316 filtered out; finished in 0.01s | S011_VERTEX_ADC_TESTS_PASS

- [x] G31: Vertex provider auth honors stored credential environment,
      ambient API-key precedence, and explicit ADC path selection
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex_provider --quiet && printf 'S011_VERTEX_PROVIDER_TESTS_PASS\n'
  EXPECT: S011_VERTEX_PROVIDER_TESTS_PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 330 filtered out; finished in 0.01s | S011_VERTEX_PROVIDER_TESTS_PASS

- [x] G32: the Vertex implementation compiles, formats, and has no whitespace
      errors
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-ai --offline && RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check && git diff --check && printf 'S011_STATIC_CHECKS_PASS\n'
  EXPECT: S011_STATIC_CHECKS_PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=S011_STATIC_CHECKS_PASS | Finished `dev` profile [optimized + debuginfo] target(s) in 0.07s

- [x] G33: the conversion ledger and synchronized docs report the measured
      S-011 progress
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: Conversion progress:
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=Conversion progress: 65.06% (108/166; 58 open)

## Next slice: close S-012 Cloudflare gateway binding and precedence parity

- [x] G34: the Cloudflare gateway-binding transport validates its configured
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 325 filtered out; finished in 0.01s | S012_CLOUDFLARE_BINDING_TESTS_PASS
      prefix, translates JSON POST requests, preserves query/headers, and
      rejects unexpressible traffic
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare --quiet && printf 'S012_CLOUDFLARE_BINDING_TESTS_PASS\n'
  EXPECT: S012_CLOUDFLARE_BINDING_TESTS_PASS

- [x] G35: Cloudflare Workers AI and AI Gateway auth preserve stored-field
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 338 filtered out; finished in 0.01s | S012_CLOUDFLARE_PROVIDER_TESTS_PASS
      precedence, scoped account/gateway env, and header/base-url behavior
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare_provider --quiet && printf 'S012_CLOUDFLARE_PROVIDER_TESTS_PASS\n'
  EXPECT: S012_CLOUDFLARE_PROVIDER_TESTS_PASS

- [x] G36: the Cloudflare implementation compiles, formats, and has no
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=S012_STATIC_CHECKS_PASS | Finished `dev` profile [optimized + debuginfo] target(s) in 0.07s
      whitespace errors
  CHECK: RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-ai --offline && RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check && git diff --check && printf 'S012_STATIC_CHECKS_PASS\n'
  EXPECT: S012_STATIC_CHECKS_PASS

- [x] G37: the conversion ledger and synchronized docs report the measured
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=fbe3799a2a56/21 entries; output=Conversion progress: 65.66% (109/166; 57 open)
      S-012 progress
  CHECK: node scripts/conversion-progress.mjs
  EXPECT: Conversion progress:

## Current slice: complete EXT-011 native tool-definition parity

- [x] G38: the native registered-tool model stores every upstream ToolDefinition
      metadata field in an open Rust representation, including label, prompt
      hints, constrained sampling, render-shell policy, preparation, execution
      mode, and render callbacks
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::types::tests::registered_tool_definition_metadata_is_preserved --quiet
  EXPECT: test result: ok
  EVIDENCE: 1 passed; 0 failed

- [x] G39: prepareArguments is applied before native execute, live onUpdate
      values reach the host callback, and renderCall/renderResult receive and
      return open JSON request/response values
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests::native_registered_tool_contract_is_live --quiet
  EXPECT: test result: ok
  EVIDENCE: 1 passed; 0 failed

- [x] G40: the external parity target covers registration, metadata, prepared
      execution, updates, and the normal AgentTool adapter path
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
  EXPECT: test result: ok
  EVIDENCE: 9 passed; 0 failed

- [x] G41: the coding-agent package compiles and the focused source/test scope
      has no formatting or whitespace defects
  CHECK: /bin/sh -c 'RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-coding-agent --tests --offline && /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt --edition 2021 --check crates/pi-coding-agent/src/core/extensions/types.rs crates/pi-coding-agent/src/core/extensions/integration.rs crates/pi-coding-agent/src/core/extensions/runner.rs crates/pi-coding-agent/src/core/extensions/loader.rs crates/pi-coding-agent/src/core/extensions/wrapper.rs crates/pi-coding-agent/src/modes/rpc.rs crates/pi-coding-agent/tests/extensions_parity.rs && git diff --check && printf "EXT011_STATIC_CHECKS_PASS\\n"'
  EXPECT: EXT011_STATIC_CHECKS_PASS
  EVIDENCE: exit=0; package test-target check, focused format check, and diff check passed
  EVIDENCE: exit=0; package check, workspace format check, and diff check passed

- [x] G42: clippy is warning-clean for the package once the pre-existing
      changelog invalid-regex diagnostic is explicitly isolated
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo clippy -p pi-coding-agent --lib --offline -- -D warnings -A clippy::invalid_regex
  EXPECT: Finished `dev` profile
  EVIDENCE: exit=0; finished successfully

## Current slice: complete EXT-009/010 live context and UI broker parity

- [x] G43: native handlers can call the full ExtensionContext host surface,
      including getters, state mutations, queued lifecycle/model actions,
      messaging, compaction, abort, shutdown, signal, and tool updates
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests::native_handler_can_call_the_bound_extension_host_context --quiet
  EXPECT: test result: ok
  EVIDENCE: 1 passed; 0 failed; included in the 57-test core::extensions suite

- [x] G44: the UI broker covers dialog success, cancellation, timeout, late
      and malformed responses, concurrent ids, fire-and-forget actions,
      terminal listener dispatch/cleanup, custom overlays, factories, themes,
      editor state, and tool expansion
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib core::extensions -- --nocapture --test-threads=1
  EXPECT: test result: ok
  EVIDENCE: 57 passed; 0 failed

- [x] G45: RPC routes live select/confirm/input/editor responses and terminal
      input through the host broker, including diagnostic output
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib 'modes::rpc::tests::rpc_' -- --nocapture --test-threads=1
  EXPECT: test result: ok
  EVIDENCE: 16 passed; 0 failed

- [x] G46: the external extension parity target remains green after the
      context/UI/tool contract changes
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --test extensions_parity --offline -- --nocapture --test-threads=1
  EXPECT: test result: ok
  EVIDENCE: 9 passed; 0 failed

- [x] G47: package tests compile and focused extension/RPC files are formatted
      and whitespace-clean
  CHECK: /bin/sh -c 'RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-coding-agent --tests --offline && /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt --edition 2021 --check crates/pi-coding-agent/src/core/extensions/types.rs crates/pi-coding-agent/src/core/extensions/integration.rs crates/pi-coding-agent/src/core/extensions/runner.rs crates/pi-coding-agent/src/core/extensions/loader.rs crates/pi-coding-agent/src/core/extensions/wrapper.rs crates/pi-coding-agent/src/modes/rpc.rs crates/pi-coding-agent/tests/extensions_parity.rs && git diff --check'
  EXPECT: exit 0
  EVIDENCE: exit=0; check finished successfully; rustfmt and diff check passed

- [x] G48: the Rust-native conversion audit remains authoritative and reports
      no ledger/source/JS blockers after the extension checkpoint
  CHECK: /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  EXPECT: Conversion progress: 100.00% (166/166; 0 open); audit blockers: 0; workspace JS/TS source files: 0
  EVIDENCE: exit=0

## Interactive hidden-command evidence — 2026-08-26

These scoped evidence rows are separate from the extension contract gates above;
they do not claim unrelated workspace clippy or formatting debt.

- [x] Interactive hidden components and Daxnuts payload are unit-tested.
  CHECK: `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::easter_eggs -- --test-threads=1`
  EVIDENCE: 6 passed, 0 failed; includes width 1–64 safety, completion text,
  exact provider/model predicate, non-empty 6,144-character image, and real
  ESC-byte scanline.

- [x] Hidden parsing and upstream ISO debug timestamp are unit-tested.
  CHECK: the focused `interactive::interactive_tests::parse_submit_executes_hidden_commands_without_publishing_them` and `modes::interactive::tests::debug_timestamp_matches_upstream_iso_shape` Cargo tests.
  EVIDENCE: 1 passed for each test, 0 failed.

- [x] All registered slash commands and hidden component lifecycle paths have
  real tmux PTY coverage.
  CHECK: `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_complete_pty -- --test-threads=1`
  EVIDENCE: 4 passed, 0 failed, including success, repeat, narrow/resize,
  cancellation, command errors, quit, and terminal restoration.

- [x] The broader interactive PTY matrix remains green.
  CHECK: `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_full_matrix -- --test-threads=1`
  EVIDENCE: 7 passed, 0 failed.

- [x] The coding-agent package compiles and the scoped source is formatted.
  CHECK: `cargo check -p pi-coding-agent --offline`, direct rustfmt over the
  five scoped interactive files, and `git diff --check`.
  EVIDENCE: all three exit 0. Unmodified workspace `cargo fmt --all --
  --check` and strict package clippy remain blocked only by unrelated dirty
  files/diagnostics documented in HANDOFF.md.
