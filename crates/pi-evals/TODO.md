# pi-evals — current conversion notes (P9)

The subprocess harness, session-usage/cost extraction, smoke scenario,
extension scenario, artifacts, harness table, reporter, and summary are
covered by checked ledger rows #91–92 and S-058–S-059.

Evidence:

- cargo test -p pi-evals --offline --quiet
- cargo test -p pi-evals --offline --test session_usage --quiet
- cargo test -p pi-evals --offline --test extensions --quiet

The faux run intentionally reports deterministic fixture usage and a
schema-1 extension-authoring diagnostic; it is not a stale usage-zero or
unscorable claim.
