# Gates: leaf-D2 auxiliary client parity

OWNS: crates/pi-client/**

Scope: close S-045–S-049 lease, reconnect, transport, timeout, disposal, and churn parity in pi-client with deterministic fixtures.

- [x] G1: pi-client parity and regression fixtures pass
  CHECK: /home/mustbearnold/.cargo/bin/cargo test -p pi-client --offline --all-targets --quiet && echo PI_CLIENT_TESTS_PASSED
  EXPECT: PI_CLIENT_TESTS_PASSED
  CWD: ../..
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=0e9bd6bf5a04/31 entries; output=test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s | PI_CLIENT_TESTS_PASSED

- [x] G2: pi-client formatting is clean
  CHECK: /home/mustbearnold/.cargo/bin/cargo fmt -p pi-client -- --check && echo PI_CLIENT_FMT_PASSED
  EXPECT: PI_CLIENT_FMT_PASSED
  CWD: ../..
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=0e9bd6bf5a04/31 entries; output=PI_CLIENT_FMT_PASSED

- [x] G3: pi-client strict clippy is clean
  CHECK: /home/mustbearnold/.cargo/bin/cargo clippy -p pi-client --offline --all-targets -- -D warnings && echo PI_CLIENT_CLIPPY_PASSED
  EXPECT: PI_CLIENT_CLIPPY_PASSED
  CWD: ../..
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=0e9bd6bf5a04/31 entries; output=PI_CLIENT_CLIPPY_PASSED | Finished `dev` profile [optimized + debuginfo] target(s) in 0.06s

- [x] G4: scoped changes contain no whitespace errors
  CHECK: git diff --check -- crates/pi-client && echo PI_CLIENT_DIFF_CHECK_PASSED
  EXPECT: PI_CLIENT_DIFF_CHECK_PASSED
  CWD: ../..
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/run/media/mustbearnold/Projects/AI Agents/pi-rust; path=0e9bd6bf5a04/31 entries; output=PI_CLIENT_DIFF_CHECK_PASSED
