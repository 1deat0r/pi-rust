#!/usr/bin/env bash
# docs-lint: machine-checked hygiene for living workflow docs.
# Lints README.md + AGENTS.md per .markdownlint-cli2.jsonc (MD013 off:
# parity tables are machine-read single-line rows; historical registers
# and appendices are grandfathered out of the globs).
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
npx --yes markdownlint-cli2 2>&1 | tail -5
