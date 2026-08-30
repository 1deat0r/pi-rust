//! Unix-domain socket listener — port of
//! `packages/server/src/transports/unix/listener.ts`.

use std::future::Future;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener as TokioUnixListener;

use crate::connection::{
    ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler, UnixByteConnection,
};

const DEFAULT_SOCKET_MODE: u32 = 0o600;
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const AUTH_PREFIX: &str = "PI-AUTH ";
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_PENDING_BYTES: usize = pi_protocol::DEFAULT_MAX_FRAME_LENGTH * 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

pub fn validate_unix_socket_path(path: &str, description: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{description} must not be empty"));
    }
    if path.len() > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(format!(
            "{description} is too long; maximum is {MAX_UNIX_SOCKET_PATH_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

pub trait PiServerListener: Send + Sync {
    fn address(&self) -> Option<String>;
    fn start(
        &mut self,
        accept: ByteConnectionAcceptor,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    fn close(&mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

async fn remove_stale_socket(path: &Path) -> Result<(), String> {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect Unix listener path: {error}")),
    };
    if !meta.file_type().is_socket() {
        return Err(format!(
            "Refusing to remove non-socket Unix listener path: {}",
            path.display()
        ));
    }
    if socket_is_live(path).await? {
        return Err(format!(
            "Unix listener is already running: {}",
            path.display()
        ));
    }
    let original = file_identity(&meta);
    let preserved = path.with_file_name(format!(".s-{}", uuid::Uuid::new_v4()));
    match tokio::fs::rename(path, &preserved).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("preserve stale socket: {error}")),
    }
    let moved = tokio::fs::symlink_metadata(&preserved)
        .await
        .map_err(|error| format!("inspect preserved stale socket: {error}"))?;
    if !moved.file_type().is_socket() || file_identity(&moved) != original {
        if tokio::fs::symlink_metadata(path).await.is_err() {
            let _ = tokio::fs::rename(&preserved, path).await;
        }
        return Err(format!(
            "Unix listener path changed while checking for a stale socket: {}",
            path.display()
        ));
    }
    tokio::fs::remove_file(&preserved)
        .await
        .map_err(|e| format!("remove stale socket: {e}"))?;
    Ok(())
}

async fn socket_is_live(path: &Path) -> Result<bool, String> {
    match tokio::time::timeout(SOCKET_PROBE_TIMEOUT, tokio::net::UnixStream::connect(path)).await {
        Ok(Ok(_)) => Ok(true),
        Err(_) => Ok(true),
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(false)
        }
        Ok(Err(error)) => Err(format!("probe Unix listener: {error}")),
    }
}

async fn remove_owned_path(path: &Path, expected: FileIdentity) -> Result<(), String> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect owned Unix listener path: {error}")),
    };
    if file_identity(&metadata) != expected {
        // A caller may have replaced the public path while the listener was
        // shutting down. Never remove a replacement inode.
        return Ok(());
    }
    tokio::fs::remove_file(path)
        .await
        .map_err(|error| format!("remove owned Unix listener path: {error}"))
}

/// Unix-domain socket listener with a private owned-bind path and stale
/// socket removal.
pub struct UnixListener {
    path: PathBuf,
    mode: u32,
    listener_keepalive: bool,
    bound_path: Option<String>,
    accept: Option<ByteConnectionAcceptor>,
    close_flag: Option<tokio_util::sync::CancellationToken>,
    join: Option<tokio::task::JoinHandle<()>>,
    connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
    auth_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    bound_identity: Option<FileIdentity>,
    closing: bool,
    auth_token: Option<Arc<str>>,
    max_pending_bytes: usize,
}

impl UnixListener {
    pub fn new(path: impl Into<String>) -> Result<Self, String> {
        let path = path.into();
        validate_unix_socket_path(&path, "PiServer Unix socket path")?;
        Ok(Self {
            path: PathBuf::from(&path),
            mode: DEFAULT_SOCKET_MODE,
            listener_keepalive: false,
            bound_path: None,
            accept: None,
            close_flag: None,
            join: None,
            connections: Arc::new(Mutex::new(Vec::new())),
            auth_tasks: Arc::new(Mutex::new(Vec::new())),
            bound_identity: None,
            closing: false,
            auth_token: None,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
        })
    }

    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    /// Require a connection preface before exposing bytes to the protocol
    /// server. The preface is `PI-AUTH <token>\n`; connections without the
    /// exact token are closed before a protocol handshake is possible.
    /// Leaving the listener unconfigured preserves the existing wire format.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Result<Self, String> {
        let token = token.into();
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err(
                "Unix listener auth token must be non-empty and contain no newlines".into(),
            );
        }
        self.auth_token = Some(Arc::from(token));
        Ok(self)
    }

    /// Bound queued outbound bytes per Unix connection, matching the
    /// upstream listener's slow-peer protection. The default is four maximum
    /// protocol frames.
    pub fn with_max_pending_bytes(mut self, max_pending_bytes: usize) -> Result<Self, String> {
        if max_pending_bytes == 0 {
            return Err("Unix listener max_pending_bytes must be positive".into());
        }
        self.max_pending_bytes = max_pending_bytes;
        Ok(self)
    }

    fn private_bind_path(&self) -> PathBuf {
        let suffix = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(self.path.to_string_lossy().as_bytes());
            let digest = hasher.finalize();
            format!(
                "{:08x}",
                u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
            )
        };
        let dir = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        dir.join(format!(".p-{suffix}"))
    }
}

impl PiServerListener for UnixListener {
    fn address(&self) -> Option<String> {
        self.bound_path.clone()
    }

    fn start(
        &mut self,
        accept: ByteConnectionAcceptor,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(start_inner(self, accept))
    }

    fn close(&mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(close_inner(self))
    }
}

impl UnixListener {
    async fn start_inner(&mut self, accept: ByteConnectionAcceptor) -> Result<(), String> {
        if self.listener_keepalive {
            return Err("Unix listener is already started".to_string());
        }
        if self.closing {
            return Err("Unix listener is closing or closed".to_string());
        }
        self.accept = Some(accept.clone());

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("mkdir: {e}"))?;
            }
        }
        remove_stale_socket(&self.path).await?;
        let bind_path = self.private_bind_path();
        validate_unix_socket_path(
            &bind_path.to_string_lossy(),
            "PiServer private Unix bind path",
        )?;
        remove_stale_socket(&bind_path).await?;
        let listener = TokioUnixListener::bind(&bind_path).map_err(|e| format!("bind: {e}"))?;
        let identity = file_identity(
            &std::fs::metadata(&bind_path).map_err(|e| format!("stat bound socket: {e}"))?,
        );
        // Expose the public path as a hard link. Unix sockets support hard
        // links, and this preserves inode identity for race-safe cleanup.
        if let Err(error) = std::fs::hard_link(&bind_path, &self.path) {
            drop(listener);
            let _ = tokio::fs::remove_file(&bind_path).await;
            return Err(format!("link: {error}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
        }

        let flag = tokio_util::sync::CancellationToken::new();
        let join = tokio::spawn(Self::accept_loop(
            listener,
            accept,
            flag.clone(),
            self.auth_token.clone(),
            self.max_pending_bytes,
            self.connections.clone(),
            self.auth_tasks.clone(),
        ));
        self.listener_keepalive = true;
        self.close_flag = Some(flag);
        self.join = Some(join);
        self.bound_path = Some(self.path.to_string_lossy().into_owned());
        self.bound_identity = Some(identity);
        Ok(())
    }

    async fn close_inner(&mut self) -> Result<(), String> {
        self.closing = true;
        if let Some(flag) = &self.close_flag {
            flag.cancel();
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
        let auth_tasks = std::mem::take(
            &mut *self
                .auth_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for task in auth_tasks {
            let _ = task.await;
        }
        let connections = std::mem::take(
            &mut *self
                .connections
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let close_tasks = connections
            .into_iter()
            .map(|connection| {
                tokio::spawn(async move {
                    let _ = connection.close(None).await;
                })
            })
            .collect::<Vec<_>>();
        for task in close_tasks {
            let _ = task.await;
        }
        self.listener_keepalive = false;
        self.close_flag = None;
        self.accept = None;
        let mut cleanup_error = None;
        if let Some(identity) = self.bound_identity.take() {
            if let Err(error) = remove_owned_path(&self.path, identity).await {
                cleanup_error = Some(error);
            }
            if let Err(error) = remove_owned_path(&self.private_bind_path(), identity).await {
                cleanup_error.get_or_insert(error);
            }
        }
        self.bound_path = None;
        cleanup_error.map_or(Ok(()), Err)
    }

    async fn accept_loop(
        listener: TokioUnixListener,
        accept: ByteConnectionAcceptor,
        flag: tokio_util::sync::CancellationToken,
        auth_token: Option<Arc<str>>,
        max_pending_bytes: usize,
        connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
        auth_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    ) {
        loop {
            tokio::select! {
                _ = flag.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    if let Some(token) = auth_token.clone() {
                        let task = tokio::spawn(authenticate_and_accept(
                            stream,
                            token,
                            flag.clone(),
                            accept.clone(),
                            connections.clone(),
                            max_pending_bytes,
                        ));
                        let mut tasks = auth_tasks.lock().unwrap_or_else(|error| error.into_inner());
                        tasks.retain(|task| !task.is_finished());
                        tasks.push(task);
                    } else {
                        accept_stream(
                            stream,
                            flag.clone(),
                            accept.clone(),
                            connections.clone(),
                            max_pending_bytes,
                        ).await;
                    }
                }
            }
        }
    }
}

async fn authenticate_and_accept(
    mut stream: tokio::net::UnixStream,
    token: Arc<str>,
    flag: tokio_util::sync::CancellationToken,
    accept: ByteConnectionAcceptor,
    connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
    max_pending_bytes: usize,
) {
    let mut expected = AUTH_PREFIX.as_bytes().to_vec();
    expected.extend_from_slice(token.as_bytes());
    expected.push(b'\n');
    let mut received = vec![0u8; expected.len()];
    let authenticated = tokio::select! {
        _ = flag.cancelled() => false,
        result = tokio::time::timeout(AUTH_TIMEOUT, stream.read_exact(&mut received)) => {
            result.is_ok_and(|result| result.is_ok()) && received == expected
        }
    };
    if !authenticated {
        let _ = stream.shutdown().await;
        return;
    }
    accept_stream(stream, flag, accept, connections, max_pending_bytes).await;
}

async fn accept_stream(
    mut stream: tokio::net::UnixStream,
    flag: tokio_util::sync::CancellationToken,
    accept: ByteConnectionAcceptor,
    connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
    max_pending_bytes: usize,
) {
    if flag.is_cancelled() {
        let _ = stream.shutdown().await;
        return;
    }
    let (read_half, write_half) = stream.into_split();
    let connection = UnixByteConnection::from_parts_with_max_pending_bytes(
        read_half,
        write_half,
        max_pending_bytes,
    );
    let connection_as_trait: Arc<dyn ByteConnection> = connection.clone();
    {
        let mut tracked = connections
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        tracked.retain(|candidate| !candidate.closed());
        tracked.push(connection_as_trait.clone());
    }
    if flag.is_cancelled() {
        let _ = connection.close(None).await;
        return;
    }
    let handler: Arc<Mutex<dyn ByteConnectionHandler>> = accept(connection.clone());
    let Some(reader) = connection.take_reader() else {
        let _ = connection.close(None).await;
        return;
    };
    tokio::spawn(read_loop(
        reader,
        handler,
        connection_as_trait,
        connections,
        connection.read_cancel_token(),
    ));
}

/// Boxed start future for the trait (private access wrapper).
fn start_inner<'a>(
    listener: &'a mut UnixListener,
    accept: ByteConnectionAcceptor,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(listener.start_inner(accept))
}

/// Boxed close future for the trait (private access wrapper).
fn close_inner<'a>(
    listener: &'a mut UnixListener,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(listener.close_inner())
}

async fn read_loop(
    mut reader: crate::connection::UnixReadHalf,
    handler: Arc<Mutex<dyn ByteConnectionHandler>>,
    connection: Arc<dyn ByteConnection>,
    connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match tokio::select! {
            _ = cancel.cancelled() => None,
            result = reader.read(&mut buf) => Some(result),
        } {
            None => break,
            Some(Ok(0)) => break,
            Some(Ok(n)) => n,
            Some(Err(error)) => {
                handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .on_error(error.to_string());
                break;
            }
        };
        handler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on_data(&buf[..n], &handler);
    }
    let _ = connection.close(None).await;
    connections
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|candidate| !Arc::ptr_eq(candidate, &connection));
    handler
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .on_close();
}
