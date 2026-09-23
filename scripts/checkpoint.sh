#!/usr/bin/env bash
# checkpoint.sh — write one slice body into CONVERSION-LEDGER + PLAN +
# HANDOFF (and optionally a register note), then commit. post-commit
# auto-pushes and verifies the remote hash.
#
# Usage:
#   scripts/checkpoint.sh --title "Session slice AJ: ..." --body-file - \
#     [--message "..."] [--progress "100.00% (166/166; 0 open)"] \
#     [--register-note] [--dry-run] [--no-commit]
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

TITLE=""
BODY=""
BODY_FILE=""
MESSAGE=""
PROGRESS=""
REGISTER_NOTE=0
DRY_RUN=0
NO_COMMIT=0

usage() {
  cat <<'EOF'
checkpoint.sh — write one slice body into CONVERSION-LEDGER + PLAN + HANDOFF,
stage them, commit; post-commit auto-pushes and verifies remote hash.

  --title "Session slice AJ: short"   required
  --body-file - | --body STRING       required (stdin for -)
  --message "git subject"             defaults to title
  --progress "100.00% (166/166; 0 open)"  pin progress (else conversion_audit)
  --register-note                     also append non-TUI register note
  --dry-run                           print plan; touch nothing
  --no-commit                         stage docs only
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --title) TITLE="${2:-}"; shift 2 ;;
    --body) BODY="${2:-}"; shift 2 ;;
    --body-file)
      BODY_FILE="${2:-}"
      if [[ "$BODY_FILE" == "-" ]]; then
        BODY="$(cat)"
        shift 2
      else
        BODY="$(cat "$BODY_FILE")"
        shift 2
      fi
      ;;
    --message) MESSAGE="${2:-}"; shift 2 ;;
    --progress) PROGRESS="${2:-}"; shift 2 ;;
    --register-note) REGISTER_NOTE=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --no-commit) NO_COMMIT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "checkpoint: unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$TITLE" ]] || { echo "checkpoint: --title required" >&2; exit 2; }
[[ -n "$BODY" ]] || { echo "checkpoint: --body or --body-file - required" >&2; exit 2; }
[[ -n "$MESSAGE" ]] || MESSAGE="$TITLE"

if [[ -z "$PROGRESS" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    PROGRESS="$(cargo run -p pi-coding-agent --offline --quiet --bin conversion_audit -- progress 2>/dev/null \
      | sed -n 's/^Conversion progress: //p' | head -1 || true)"
  fi
  if [[ -z "$PROGRESS" ]]; then
    PROGRESS="$(grep -oE '[0-9]+\.[0-9]+% \([0-9]+/[0-9]+; [0-9]+ open\)' CONVERSION-LEDGER.md | head -1 || echo 'unknown')"
  fi
fi

today="$(date -u +%Y-%m-%d)"
# Ledger heading keeps colon form used historically; plan/handoff use em dash.
# Title example: "Session slice AJ: short name" → plan/handoff: "Session slice AJ — short name"
if [[ "$TITLE" == *"Session slice "* ]]; then
  plan_title="${TITLE/Session slice /Session slice }"
  # replace first ": " after the slice id with " — "
  plan_title="$(printf '%s' "$plan_title" | sed -E 's/^(Session slice [^:]+): /\1 — /')"
else
  plan_title="$TITLE"
fi

export CKPT_TITLE="$TITLE"
export CKPT_PROGRESS="$PROGRESS"
export CKPT_BODY="$BODY"
export CKPT_LEDGER_H="### ${TITLE} (last updated ${today})"
export CKPT_PLAN_H="### ${plan_title}"
export CKPT_HANDOFF_H="### ${plan_title} (checkpoint ${today})"
export CKPT_TODAY="$today"
export CKPT_DRY="$DRY_RUN"
export CKPT_REG="$REGISTER_NOTE"

python3 <<'PY'
import os, re, sys, pathlib

dry = os.environ.get("CKPT_DRY") == "1"
reg = os.environ.get("CKPT_REG") == "1"
title = os.environ["CKPT_TITLE"]
progress = os.environ["CKPT_PROGRESS"]
body = os.environ["CKPT_BODY"]
ledger_h = os.environ["CKPT_LEDGER_H"]  # includes leading ###
plan_h = os.environ["CKPT_PLAN_H"]
handoff_h = os.environ["CKPT_HANDOFF_H"]
today = os.environ["CKPT_TODAY"]

# ledger_h may be built as "### ..." already or without — normalize
def as_heading(h: str) -> str:
    h = h.strip()
    if not h.startswith("###"):
        h = "### " + h
    return h

ledger_h = as_heading(ledger_h)
plan_h = as_heading(plan_h)
handoff_h = as_heading(handoff_h)

section_body = body.rstrip() + f"\n\nConversion progress: {progress}.\n"

def read(p):
    return pathlib.Path(p).read_text(encoding="utf-8")

def write(p, t):
    pathlib.Path(p).write_text(t, encoding="utf-8")

def insert_after_day_goal(text: str, heading: str, section: str):
    # Prefer first "## Day goal" line; insert block before the next "### " if any,
    # else at end of that section's first paragraph area.
    m = re.search(r"^## Day goal .*$", text, flags=re.M)
    if not m:
        raise SystemExit("checkpoint: no '## Day goal' anchor")
    # If this exact heading already exists, skip
    if re.search(re.escape(heading) + r"\b", text):
        return text, False
    # Insert immediately after the day-goal line (and its trailing blank line)
    insert_at = m.end()
    # consume one following newline
    if insert_at < len(text) and text[insert_at] == "\n":
        insert_at += 1
    block = f"\n{heading}\n\n{section}\n"
    return text[:insert_at] + block + text[insert_at:], True

ledger = read("CONVERSION-LEDGER.md")
plan = read("PLAN.md")
handoff = read("HANDOFF.md")

ledger_sec = section_body
# PLAN/HANDOFF historically end with next-action; keep same body.
plan_sec = section_body
hand_sec = section_body

ledger2, l_ok = insert_after_day_goal(ledger, ledger_h, ledger_sec)
plan2, p_ok = insert_after_day_goal(plan, plan_h, plan_sec)
hand2, h_ok = insert_after_day_goal(handoff, handoff_h, hand_sec)

if dry:
    print("checkpoint dry-run:")
    print(f"  title: {title}")
    print(f"  progress: {progress}")
    print(f"  CONVERSION-LEDGER.md: {'insert ' + ledger_h if l_ok else 'skip (exists)'}")
    print(f"  PLAN.md: {'insert ' + plan_h if p_ok else 'skip (exists)'}")
    print(f"  HANDOFF.md: {'insert ' + handoff_h if h_ok else 'skip (exists)'}")
    print(f"  docs/NON-TUI-PARITY-STATUS.md: {'append Checkpoint sync' if reg else 'unchanged'}")
    sys.exit(0)

if l_ok:
    write("CONVERSION-LEDGER.md", ledger2)
if p_ok:
    write("PLAN.md", plan2)
if h_ok:
    write("HANDOFF.md", hand2)

if reg:
    rp = pathlib.Path("docs/NON-TUI-PARITY-STATUS.md")
    rt = rp.read_text(encoding="utf-8")
    marker = f"## Checkpoint sync ({today})"
    # Replace any prior Checkpoint sync sections, then insert a fresh one
    # before "## Normalized status contract" when present.
    if "## Checkpoint sync (" in rt:
        rt = re.sub(
            r"\n## Checkpoint sync \([^)]*\)\n(?:(?!\n## ).)*",
            "",
            rt,
            flags=re.S,
        )
    note = (
        f"\n{marker}\n\n"
        f"Progress: `{progress}`. Infrastructure/process checkpoint; "
        "parity rows change only when a slice body explicitly promotes them.\n\n"
    )
    anchor = re.search(r"^## Normalized status contract", rt, flags=re.M)
    if anchor:
        rt = rt[: anchor.start()] + note + rt[anchor.start() :]
    else:
        rt = rt.rstrip() + "\n" + note
    write(str(rp), rt)

def yn(b):
    return "insert" if b else "skip (exists)"

print(f"checkpoint: ledger={yn(l_ok)} plan={yn(p_ok)} handoff={yn(h_ok)} register={'insert' if reg else 'n/a'}")
print(f"checkpoint: progress {progress}")
PY

if [[ "$DRY_RUN" -eq 1 ]]; then
  exit 0
fi

# Quality gates before staging.
bash scripts/size-gate.sh
if grep -Fxq "README.md" <(git status --porcelain=v1 --untracked-files=no | awk '{print $2}') 2>/dev/null \
  || true; then
  :
fi
bash scripts/docs-lint.sh >/dev/null

docs=(CONVERSION-LEDGER.md PLAN.md HANDOFF.md)
if [[ "$REGISTER_NOTE" -eq 1 ]]; then
  docs+=(docs/NON-TUI-PARITY-STATUS.md)
fi
git add -- "${docs[@]}"
git diff --cached --check

if [[ "$NO_COMMIT" -eq 1 ]]; then
  echo "checkpoint: docs staged (--no-commit); commit manually with message: $MESSAGE"
  exit 0
fi

git commit -m "$MESSAGE"
# post-commit auto-pushes; surface blocker if any.
git_dir="$(git rev-parse --git-dir)"
if [[ -f "$git_dir/PUSH_BLOCKED" ]]; then
  echo "checkpoint: commit ok but PUSH BLOCKED:" >&2
  cat "$git_dir/PUSH_BLOCKED" >&2
  exit 1
fi
branch="$(git rev-parse --abbrev-ref HEAD)"
local_head="$(git rev-parse HEAD)"
remote_head="$(git ls-remote origin "refs/heads/$branch" | awk '{print $1; exit}')"
if [[ "$local_head" != "$remote_head" ]]; then
  echo "checkpoint: local/remote mismatch local=$local_head remote=$remote_head" >&2
  exit 1
fi
echo "checkpoint: committed+pushed $local_head (origin/$branch match)"
