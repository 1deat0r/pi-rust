# pi-server — port status

P6 landed (Session 10): UnixSocketListener (stale-socket probe, private bind
symlink, mode), PiServer (hello handshake + version check + hello_error,
Command dispatch to PiServerService, ServerMessage response envelopes,
snapshot publisher with revision/broadcast, protocol error mapping), plus
the InMemoryService test service. E2E over a real unix socket
(client_server_roundtrip, bad_protocol_version_gets_hello_error) +
codec framing probe.

## Not yet ported (upstream mapping)
- LiveSessionManager acquire/release exclusivity and attach/detach
  validation (the service layer is shared-safe for the in-memory service).
- Session lock/terminal-close semantics, command queuing, subscription
  segment control for prompt/steer concurrency.
- testing/service.ts parity harness + conformance suite (package tests).
