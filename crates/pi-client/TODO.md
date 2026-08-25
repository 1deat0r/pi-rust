# pi-client — current conversion notes

The Unix client, reconnect state/listeners, lease and exclusive-attach
reconciliation, request timeouts, late-response suppression, disposal, and
transport-factory seams are implemented and owned by ledger rows S-045–S-049.

Evidence:

- cargo test -p pi-client --offline --test auxiliary_parity --quiet
- cargo test -p pi-client --offline --quiet
- cargo clippy -p pi-client --offline --all-targets -- -D warnings

This file contains no open “not yet ported” claim. Any newly discovered
client/protocol behavior must be assigned by the final S-001/S-066 audit.
