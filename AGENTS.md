# Codex Operating Protocol — pi-rust

Every Codex session working in this repository follows this file. The goal is
that another session can resume immediately from the repository documents,
without relying on pane history or memory.

## Required startup

Before changing code, read:

- `CONVERSION-LEDGER.md` — authoritative conversion checklist.
- `PLAN.md` — phase status, parity matrix, evidence, and next work.
- `HANDOFF.md` — latest checkpoint, tests, blockers, and resume point.

Run the Rust-native progress/audit checker before relying on a percentage:

```bash
cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
```

## Required end-of-task documentation gate

A Codex task is not complete until this gate passes, even when the task ends in
a test failure, a blocker, or a decision not to change code.

1. Update `CONVERSION-LEDGER.md` for every task-status change. Mark a task
   complete only with its evidence tier (`unit`, `mock`, or `live`) and the
   exact command or fixture that proves it. Never check off work merely because
   similarly named code exists.
2. Run `cargo run -p pi-coding-agent --offline --bin conversion_audit -- all`
   and use its exact `Conversion progress:` output as the current source-ledger
   percentage. If the ledger header disagrees, fix the stale summary before
   ending the task.
3. Update `PLAN.md` with the current progress, phase/criterion evidence,
   issues found, and the next dependency-safe action.
4. Update `HANDOFF.md` with the current branch/worktree state, the exact tests
   and checks run, blockers, completed milestone(s), remaining work, and the
   same progress-checker output. Keep `PLAN.md`, `HANDOFF.md`, and the ledger
   synchronized.
5. If no ledger item changed, explicitly record that fact and the current
   checker output in `HANDOFF.md`; verify the other two documents still agree.
   Do not fabricate a checkbox solely to create a diff.
6. Run `git diff --check` and the narrowest relevant tests. Do not report a
   task as complete while documentation or validation is stale.

The final Codex response must name the documentation files updated or verified,
quote the progress result, list the exact validation commands, and identify the
next task or blocker. A session that stops before this gate must leave a clear
partial-work checkpoint in `HANDOFF.md`.

## Local and remote commit gate

For every completed logical task or checkpoint:

1. Update the required project documents and run the relevant validation.
2. Create one focused local commit; do not batch unrelated tasks.
3. Immediately push that commit to the configured upstream branch.
4. Verify synchronization with both `git rev-parse HEAD` and
   `git ls-remote origin refs/heads/<branch>`. Do not declare the checkpoint
   complete until the hashes match.
5. If authentication, network, permissions, or CI prevents the push, keep the
   local commit intact, record the exact blocker in `HANDOFF.md`, and stop
   claiming local/remote parity. Do not rewrite the remote URL with a secret,
   skip the commit, or silently continue accumulating unpublished commits.

A remote push is part of completion, not an optional cleanup step. This rule
applies even when the task is documentation-only. Use the existing `origin`
remote and current branch unless the user explicitly changes the target.

## Engineering rules

- Preserve existing user changes; never use broad reset or revert commands.
- Use TDD and upstream tests as the parity oracle.
- Keep evidence tiers explicit and record intentional divergences in `PLAN.md`.
- Build/test before committing; never commit a red build.
- Follow the existing one-logical-unit-per-commit and push-after-checkpoint
  directives recorded in the handoff.
