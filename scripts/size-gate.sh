#!/usr/bin/env bash
# size-gate: reject tracked-source bloat and accidental binary/target commits.
# Enforced by .githooks/pre-commit. Thresholds are workspace-wide ceilings,
# not style suggestions: current maxima sit well below the byte cap
# (interactive.rs ~572KB, CHANGELOG.md ~555KB).
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

MAX_BYTES="${PI_MAX_FILE_BYTES:-600000}"
MAX_RS_LINES="${PI_MAX_RS_LINES:-13000}"
fail=0

# 1. Never stage build artifacts or local binaries.
while IFS= read -r path; do
  case "$path" in
    target/*|rust_out|*/target/*|*.o|*.rlib|*.a|*.so|*.dylib|*.dll|*.exe)
      echo "size-gate: staged build artifact or binary: $path" >&2
      fail=1
      ;;
  esac
done <<< "$(git diff --cached --name-only --diff-filter=ACMRTUXB || true)"

# 2. Tracked-file byte ceiling (all paths).
while IFS= read -r -d '' path; do
  [[ -f "$path" ]] || continue
  size="$(wc -c < "$path")"
  if [[ "$size" -gt "$MAX_BYTES" ]]; then
    echo "size-gate: $path is $size bytes (max $MAX_BYTES)" >&2
    fail=1
  fi
  # 3. Source line ceiling for .rs (guards monolith growth like interactive.rs).
  if [[ "$path" == *.rs ]]; then
    lines="$(wc -l < "$path")"
    if [[ "$lines" -gt "$MAX_RS_LINES" ]]; then
      echo "size-gate: $path is $lines lines (max $MAX_RS_LINES)" >&2
      fail=1
    fi
  fi
  # 4. Tracked ELF/PE/Mach-O payloads are never legitimate here.
  if command -v file >/dev/null 2>&1; then
    if file -b --mime-type "$path" 2>/dev/null | grep -Eq 'application/(x-elf|x-executable|x-msdos-program|x-mach-binary|octet-stream)'; then
      # octet-stream alone is not enough (some fixtures); confirm ELF/PE magic.
      magic="$(head -c 4 "$path" 2>/dev/null | od -An -tx1 | tr -d ' \n')"
      case "$magic" in
        7f454c46|4d5a|cffaedfe|cefaedfe|feedface|feedfacf|cafebabe)
          # ELF / MZ / mach-o — allow only if path looks like a deliberate fixture name.
          case "$path" in
            *fixture*|*golden*|*testdata*) ;;
            *)
              echo "size-gate: tracked binary payload: $path" >&2
              fail=1
              ;;
          esac
          ;;
      esac
    fi
  fi
done < <(git ls-files -z)

if [[ "$fail" -ne 0 ]]; then
  echo "size-gate: FAILED (override ceilings only with PI_MAX_FILE_BYTES / PI_MAX_RS_LINES)" >&2
  exit 1
fi
echo "size-gate: OK (max ${MAX_BYTES}B, rs lines ${MAX_RS_LINES})"
