# pi-session-backends — current conversion notes (P9)

SQLite repository/storage, migrations, entries, lanes, records, facts,
branch-cache/search, session sequences/stats, writer leases, and repository
conformance are covered by checked rows S-061–S-062 and #91–100/P9.

Evidence:

- cargo test -p pi-session-backends --offline --quiet
- cargo test --workspace --offline --quiet -- --test-threads=2
- the release session/settings/auth/models fixture matrix recorded in
  S-061/S-062

The Rust API uses Result for storage failures and normalizes optional usage
fields at its typed boundary; those are explicit implementation-shape
differences, not untracked missing work.
