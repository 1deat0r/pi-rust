# Codex Operating Protocol — pi-rust

Every agent session working in this repository follows this file. The human
is the prompter; the agent owns implementation, gates, docs, and push
hygiene. Goal: another session resumes from the repository alone.

## Required startup (thin)

1. Read `HANDOFF.md` **top section only** (latest checkpoint + next task).
2. Skim `PLAN.md` top slice if HANDOFF points at open work.
3. Treat `CONVERSION-LEDGER.md` as append-only evidence (do not rewrite
   history; insert new slices at the top under the active day goal).
4. Run the progress checker before trusting any percentage:

```bash
cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
```

Do not re-read multi-thousand-line archives at startup
(`docs/*-ARCHIVE-*.md` are frozen).

## Documentation map

Living docs (edit these; `README.md` + this file are markdownlint-clean
via `bash scripts/docs-lint.sh`; tracked-file size via
`bash scripts/size-gate.sh` / pre-commit):

- `README.md` — public status; numbers come from audits, never memory.
- `AGENTS.md` (this file) — session protocol.
- `PLAN.md` — active planning window only (old checkpoints →
  `docs/PLAN-ARCHIVE-2026-08.md`).
- `HANDOFF.md` — resume context only (old → `docs/HANDOFF-ARCHIVE-2026-08.md`).
- `CONVERSION-LEDGER.md` — per-slice evidence log (append-only).
- `GATES.md` / `GATES-*.md` — frozen or per-slice gate ledgers; new work
  goes to the ledger, not a rewritten frozen file.

Machine-read registers (edit rows only):

- `docs/NON-TUI-PARITY-STATUS.md`, `docs/TUI-PARITY-STATUS.md`,
  `docs/EXHAUSTIVE-PARITY-INVENTORY.md`, `docs/PARITY-DASHBOARD.md`.

Frozen archives: `docs/*-ARCHIVE-*.md`, retired narrative in
`docs/PARITY-DASHBOARD.md`.

## End-of-task documentation (use the helper)

Write the slice body **once**. Prefer:

```bash
bash scripts/checkpoint.sh \
  --title "Session slice AJ: short name" \
  --body-file - <<'EOF'
One-paragraph what/why, oracle refs, RED/GREEN evidence, intentional
divergences, exact gate commands+counts, next dependency-safe action.
EOF
```

`checkpoint.sh` prepends the same body to `CONVERSION-LEDGER.md`,
`PLAN.md`, and `HANDOFF.md`, runs size-gate + docs-lint, stages the trio,
commits, and (via post-commit) pushes + verifies hashes. Pass
`--register-note` only when a non-TUI register note is required
(product-source slices). Pass `--dry-run` to preview. Manual path: edit
the three docs in the same shape, then still commit through the hooks.

A task is complete only when:

1. Ledger (or checkpoint body) records status with evidence tier
   (`unit` / `mock` / `live`) and the exact command that proves it.
2. `conversion_audit -- progress` matches what the staged docs claim.
3. PLAN next action and HANDOFF resume point are current.
4. `git diff --check` clean; narrowest relevant tests green.
5. Final reply names docs touched, quotes `Conversion progress:`,
   lists validation commands, and names next task or blocker.

If no ledger item changed, say so explicitly in the HANDOFF top section
with the current checker output — do not fabricate a checkbox.

## Local ↔ remote sync (automatic)

1. `.githooks/post-commit` pushes every normal commit immediately and
   verifies `git rev-parse HEAD` == `git ls-remote origin refs/heads/<branch>`.
2. On push/auth/network failure the local commit stays; the reason is
   written to `.git/PUSH_BLOCKED`. **Never claim sync while that file
   exists.** Remove it only after a verified push.
3. `checkpoint.sh` fails if `PUSH_BLOCKED` remains or hashes diverge.
4. `PI_SKIP_PUSH=1` is an emergency local-only escape (record it in
   HANDOFF if used). Do not leave work unpushed across sessions.
5. One focused commit per logical unit; never batch unrelated tasks.
   Enable hooks for a clone with `git config core.hooksPath .githooks`.

## Engineering rules

- Preserve user changes; no broad reset/revert.
- TDD + pinned upstream tests as the parity oracle.
- Explicit evidence tiers; intentional divergences recorded in PLAN.
- Never commit a red build.
- Size-gate ceilings are load-bearing (600000 B / 13000 `.rs` lines).
