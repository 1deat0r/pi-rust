# pi-evals — port status (P9)

## Done (Session 11)
- Eval harness (pi-harness), vitest-evals equivalents (harness-table/summary/reporter/artifacts),
  smoke + extensions scenarios, `cargo run -p pi-evals -- --faux --binary ./target/release/pi` runner.
  20 tests.
- Divergences: eval tasks run the real `pi` binary as a subprocess (usage tokens 0); extension
  scenario reports unscorable diagnostics under faux.
