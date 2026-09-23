# GATES — docs system + always-push (2026-09-23, slice AJ)

Observable outcomes for optimizing agent-driven docs + guaranteeing
git↔GitHub sync.

## G1 post-commit always-push hook

- CHECK: test -x .githooks/post-commit && rg -n "git push|ls-remote|PUSH_BLOCKED" .githooks/post-commit
- EXPECT: push + hash-verify + PUSH_BLOCKED present; hook executable

## G2 commit auto-pushs and lands on origin/main

- CHECK: after a normal commit (checkpoint.sh or git commit):
  `git rev-parse HEAD` == `git ls-remote origin refs/heads/main`
  and `.git/PUSH_BLOCKED` absent; `git log -1` subject matches
- EXPECT: hashes match; no PUSH_BLOCKED; post-commit verifies remote tip

## G3 checkpoint.sh writes one body into ledger + plan + handoff

- CHECK: printf 'probe body ZZZ\n' | bash scripts/checkpoint.sh
  --title "Session slice AJ: probe" --body-file -
  --progress "100.00% (166/166; 0 open)" --dry-run
- EXPECT: exit 0; lists insert/skip for all three docs; no file mutation

## G4 product source still requires the docs bundle

- CHECK: rg -n "needs_register|crates/\*|required_docs" .githooks/pre-commit
- EXPECT: crates/* sets requires_docs=1 and needs_register=1;
  Cargo.toml forces trio only; register only when needs_register
  or register staged

## G5 existing quality gates still green

- CHECK: bash scripts/size-gate.sh && bash scripts/docs-lint.sh &&
  cargo fmt --all -- --check &&
  cargo run -p pi-coding-agent --offline --quiet --bin conversion_audit -- progress
- EXPECT: size-gate OK; docs-lint 0 issues; FMT_OK;
  Conversion progress: 100.00% (166/166; 0 open)

## G6 AGENTS.md documents thin protocol + auto-push

- CHECK: rg -n "checkpoint.sh|post-commit|PUSH_BLOCKED|top section" AGENTS.md
- EXPECT: HANDOFF top-only startup; checkpoint.sh end-of-task;
  auto-push + PUSH_BLOCKED failure signal documented

## G7 push failure does not claim sync

- CHECK: rg -n "PUSH_BLOCKED|ls-remote" .githooks/post-commit scripts/checkpoint.sh
- EXPECT: failure path writes .git/PUSH_BLOCKED and does not print
  verified success; success path compares ls-remote to HEAD
