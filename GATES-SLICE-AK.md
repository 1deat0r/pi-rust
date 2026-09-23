# GATES — empty-prompt creates no session file (2026-09-23, slice AK)

Oracle: print mode skips empty initial message; sessions materialize
lazily on first assistant-bearing append. Rust today writes header
eagerly on create.

## G1 RED: empty print prompt leaves zero session files

- CHECK: cargo test -p pi-coding-agent --offline --test clean_home_cli_process empty_print_prompt -- --nocapture
- EXPECT: test exists and FAILS before fix (no session file written) /
  after fix PASSES; both `pi -p ""` and `pi --session <missing> -p ""`
  exit 0 with zero `*.jsonl` under session root and explicit path absent

## G2 RED witness: oracle citations present at pin

- CHECK: rg -n "if \(initialMessage\)" upstream_pi/packages/coding-agent/src/modes/print-mode.ts &&
  rg -n "parts.length > 0" upstream_pi/packages/coding-agent/src/cli/initial-message.ts &&
  rg -n "_persist|flushed" upstream_pi/packages/coding-agent/src/core/session-manager.ts | head -20
- EXPECT: empty-message skip + lazy persist references found at pinned oracle d7296c0

## G3 GREEN: existing session/create pins stay green

- CHECK: cargo test -p pi-coding-agent --offline --test clean_home_cli_process -- --nocapture
- EXPECT: clean_home_cli_process all pass (no clean-home regression)

## G4 GREEN: session discovery/continue family stays green

- CHECK: cargo test -p pi-coding-agent --offline --test cli_flag_matrix -- --nocapture &&
  cargo test -p pi-coding-agent --offline --test cli_session_restart_parity -- --nocapture
- EXPECT: flag matrix and restart parity pass

## G5 GREEN: session file invalid + import parity stay green

- CHECK: cargo test -p pi-coding-agent --offline --test session_file_invalid -- --nocapture &&
  cargo test -p pi-coding-agent --offline --test session_import_parity -- --nocapture
- EXPECT: session_file_invalid and session_import_parity all pass

## G6 GREEN: lib suite (918) stays green

- CHECK: cargo test -p pi-coding-agent --offline --lib -- --nocapture
- EXPECT: all lib tests pass (918 baseline)

## G7 quality gates still green

- CHECK: bash scripts/size-gate.sh && bash scripts/docs-lint.sh &&
  cargo fmt --all -- --check && cargo clippy -p pi-coding-agent --all-targets -- -D warnings &&
  cargo run -p pi-coding-agent --offline --quiet --bin conversion_audit -- progress
- EXPECT: size-gate OK; docs-lint 0; FMT_OK; clippy 0 warnings;
  Conversion progress: 100.00% (166/166; 0 open)

## G8 register note (non-TUI) records edge

- CHECK: rg -n "header-only|empty.prompt|no session file" docs/NON-TUI-PARITY-STATUS.md
- EXPECT: edge note present; metrics unchanged (111/107/59/58/58 unless a row promotes)

## Results 2026-09-23 (post-fix)

- G1 GREEN: empty_print_prompt_writes_no_session_file + empty_prompt_with_explicit_missing_session_path_writes_no_file PASS (RED before fix: header written eagerly)
- G2 GREEN: oracle citations present (print-mode.ts:131, initial-message.ts:40, session-manager.ts flushed/_persist)
- G3 GREEN: clean_home_cli_process 14/14
- G4 GREEN: cli_flag_matrix 7/7, cli_session_restart_parity 13/13
- G5 GREEN: session_file_invalid 12/12, session_import_parity 6/6
- G6 GREEN: lib 918
- G7 GREEN: size-gate OK, docs-lint 0, fmt OK, clippy -D warnings 0, conversion 100.00% (166/166; 0 open)
- G8 GREEN: Empty-prompt materialization note in CLI-014; metrics unchanged (111/107/59/58/58)
- Extra: jsonl_storage 17/17, jsonl_repo 18/18, cli_resources missing_session 1/1
