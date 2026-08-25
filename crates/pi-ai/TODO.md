# pi-ai — current conversion notes

The historical session notes are superseded by the evidence rows in
CONVERSION-LEDGER.md. This file deliberately does not claim parity from
module names alone.

## Evidence-backed surface

Core types and event streams, auth/credentials, OAuth flows, model catalogs and
stores, all bundled provider registrations, provider adaptors, constrained
sampling, deferred-response capability wiring, images, retries, proxy/Codex
transport, and catalog refresh behavior are covered by ledger rows
S-005–S-017, S-063, and the original #12–21/#38–39 rows.

Focused reproducible checks include:

- cargo test -p pi-ai --offline --lib --quiet
- cargo test -p pi-ai --offline --tests --quiet
- cargo clippy -p pi-ai --offline --all-targets -- -D warnings

Network-dependent provider smoke cases remain explicitly classified as
not-run in S-063; that is an evidence-tier boundary, not a stale TODO.
