//! Pi server — port of `packages/server` (`PiServer` over a Unix-domain
//! socket with the CBOR/4-byte-length protocol from pi-protocol).
//!
//! `PiServer` accepts authorized byte connections, runs the hello handshake
//! (protocol-version check → `ServerHello` with a snapshot → ready), executes
//! `Command`s against a `PiServerService`, and emits `ServerEvent`s (snapshot
//! broadcasts, transcript progress). The Unix listener owns the socket
//! lifecycle (stale-socket removal, mode, graceful close).

pub mod connection;
pub mod errors;
pub mod listener;
pub mod live_session;
pub mod server;
pub mod service;
pub mod snapshots;
pub mod types;

pub use connection::{ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler, ConnectionState, is_terminal_connection};
pub use errors::{internal_server_error, not_implemented_error, PiServerError, INTERNAL_SERVER_ERROR_MESSAGE, NOT_IMPLEMENTED_MESSAGE};
pub use listener::{validate_unix_socket_path, PiServerListener, UnixListener};
pub use server::PiServer;
pub use service::{InMemoryService, PiServerService, PiSessionRuntime, PiSessionRuntimeEvent};
pub use types::CreateSessionOptions;
pub use snapshots::ServerSnapshotPublisher;
pub use types::{PiServerOptions, PromptInput, SteerInput};
