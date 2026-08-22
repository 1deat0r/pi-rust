# pi-client — port status

P6 landed (Session 10): PiClient over a Unix socket — hello handshake
(snapshot awaited), request/response correlation over a pending map,
ServerEvent fanout to subscribers, live ServerSnapshot state, clean close.
E2E-verified against pi-server.

## Not yet ported (upstream mapping)
- Reconnect state machine + connection-state listeners.
- Session lease/exclusive-attach management (SessionHandle with
  acquire/release/reconcile), snapshot reconciliation, detach-on-close.
- dispose semantics beyond close, promise timeouts, transport factory
  abstraction beyond unix.
