# Gates: leaf-D1 pi-server auxiliary parity

OWNS: crates/pi-server/**

Scope: close original 52/53 and supplemental S-043/S-044 pi-server parity for the testing harness, deferred helpers, malformed frames, handshake errors, snapshots, lifecycle events, and declared conformance breadth.

- [x] D1-G1: pi-server unit and integration parity tests pass
  CHECK: cargo test -p pi-server --offline --quiet && echo PI_SERVER_TESTS_PASSED
  EXPECT: PI_SERVER_TESTS_PASSED
  EVIDENCE: `/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline --quiet` → 21 tests passed.

- [x] D1-G2: pi-server compiles with strict clippy
  CHECK: cargo clippy -p pi-server --offline --all-targets -- -D warnings && echo PI_SERVER_CLIPPY_PASSED
  EXPECT: PI_SERVER_CLIPPY_PASSED
  EVIDENCE: `/home/mustbearnold/.cargo/bin/cargo clippy -p pi-server --offline --all-targets -- -D warnings` → clean.

- [x] D1-G3: pi-server formatting and diff whitespace checks pass
  CHECK: cargo fmt --package pi-server -- --check && git diff --check -- crates/pi-server && echo PI_SERVER_FORMAT_DIFF_PASSED
  EXPECT: PI_SERVER_FORMAT_DIFF_PASSED
  EVIDENCE: `cargo fmt --package pi-server -- --check` and `git diff --check -- crates/pi-server` → passed.

- [x] D1-G4: final review confirms concurrency, shutdown, malformed-input, and cleanup behavior is covered by deterministic tests or intentionally documented in-scope
  EVIDENCE: The 21-test offline suite covers malformed frames, handshake errors, snapshots, lifecycle cleanup, command detach/dispose, and Unix socket E2E; the remaining auxiliary-service boundary is recorded in the scoped leaf gate.
