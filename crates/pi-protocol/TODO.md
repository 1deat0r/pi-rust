# pi-protocol — port status

Complete and tested (46 tests): CBOR codec, framing, codec, schemas, protocol
validation. Mirrors `packages/protocol` v0.84.2 (commit 5cd93f6).

## Done
- cbor/: `Value` model (indexmap-ordered maps, `Undefined` for JS undefined),
  encoder + decoder with strict RFC 8949 definite-length subset semantics:
  i53 ints, f64 for non-integers, -0 handling, UTF-8 validation, duplicate key
  rejection, cycle/deep guards, byte/container/depth limits, undefined map value
  skipping, undefined array element rejection, all upstream error messages.
- framing/: 4-byte BE length prefix, incremental FrameDecoder, limits.
- codec/: ClientMessage/ServerMessage validate (deny unknown fields) + encode
  frame + incremental decoding with failed-state semantics.
- schemas/: typed protocol types (ContentBlocks, Usage, TranscriptItems,
  Snapshots, Commands, Results, Client/Server messages, ModelMetadata etc.)
  with TypeBox-style strict validation.

## Not ported (not applicable / out of scope)
- Lone-surrogate / symbol / function / Date / Map encoder inputs cannot occur
  in Rust values; Rust strings are always valid Unicode scalars.
- TypeBox schema objects themselves (schema-as-data not needed by runtime).

## Future work if server package advances
- If `packages/server` adds protocol messages (hello_error details, new
  commands), extend `schemas::Command`/`ServerEvent` and re-run the
  conformance suite in `packages/protocol/test/protocol.test.ts` (all
  currently covered behaviors are ported; new upstream tests should be mirrored
  here).
