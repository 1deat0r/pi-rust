# Gates: parity-20260827 leaf-agent-harness

OWNS: crates/pi-agent/**, crates/pi-evals/**

Scope: audit the Rust agent, harness, and eval runtime against pinned upstream_pi; fix concrete parity defects and leave permanent deterministic regression evidence for lifecycle, queues, tools, compaction, recovery, telemetry, and eval usage.

- [ ] H1: the complete pi-agent unit and integration regression matrix passes offline
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --lib --tests --quiet -- --test-threads=1 && printf "H1_PI_AGENT_TESTS_PASS\\n"'
  EXPECT: H1_PI_AGENT_TESTS_PASS
  EVIDENCE: pending

- [ ] H2: the complete pi-evals binary and test targets pass offline
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --all-targets --quiet -- --test-threads=1 && printf "H2_PI_EVALS_TESTS_PASS\\n"'
  EXPECT: H2_PI_EVALS_TESTS_PASS
  EVIDENCE: pending

- [ ] H3: owned crates type-check, pass strict clippy, and have no scoped whitespace errors
  CHECK: /bin/sh -c '/home/mustbearnold/.cargo/bin/cargo check -p pi-agent -p pi-evals --offline && /home/mustbearnold/.cargo/bin/cargo clippy -p pi-agent -p pi-evals --offline --all-targets -- -D warnings && git diff --check -- crates/pi-agent crates/pi-evals && printf "H3_OWNED_STATIC_PASS\\n"'
  EXPECT: H3_OWNED_STATIC_PASS
  EVIDENCE: pending
