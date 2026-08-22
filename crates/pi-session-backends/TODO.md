# pi-session-backends — port status (P9)

## Done (Session 11)
- SqliteSessionRepository + storage layer over rusqlite (bundled): sessions/entries/lanes/records/
  facts/branch-entries/branch-tips/session-sequences/session-stats/writer-leases; migrations
  001_initial.sql byte-identical; sql.ts/types.ts/branch-cache.ts ports.
- Upstream conformance suite 30/30 against the SQLite backend; migrations/sql/facts/writer-leases/
  repository/search/branch-query/log-query/branch-cache suites ported. 85 tests.
- Divergences: rusqlite mock-injection tests adapted to observable equivalents; metadata/lanes return
  Result; usage optional fields normalize to ""/0.
