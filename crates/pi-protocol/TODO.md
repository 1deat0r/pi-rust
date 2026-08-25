# pi-protocol — current conversion notes

CBOR, framing, message codec, schemas, strict validation, and protocol
conformance for the pinned upstream revision are implemented. The protocol
surface is an owned distributed wire contract for S-043–S-049 and is also
included in the release matrix; it is not an unowned foundation hole.

Evidence:

- cargo test -p pi-protocol --offline --quiet
- cargo clippy -p pi-protocol --offline --all-targets -- -D warnings
- cargo test -p pi-server --offline --quiet
- cargo test -p pi-client --offline --test auxiliary_parity --quiet

Rust cannot represent JavaScript-only values such as symbols, functions,
dates, maps, or lone-surrogate strings. The strict codec boundary records
those intentional type-system divergences; no current upstream protocol test
requires them as Rust runtime inputs.
