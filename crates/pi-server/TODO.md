# pi-server — current conversion notes

The Unix listener, stale-socket handling, handshake/version errors, command
dispatch, response envelopes, snapshots, live-session lifecycle, deferred
test service, malformed frames, and reconnect/lease fixtures are covered by
checked ledger rows S-043–S-044 and #49–58.

Evidence:

- cargo test -p pi-server --offline --quiet
- cargo test -p pi-server --offline --test reconnect_lease_e2e --quiet
- cargo clippy -p pi-server --offline --all-targets -- -D warnings

No stale “not yet ported” claim is retained. New protocol/service exports must
be assigned by the source/export audit before the denominator is frozen.
