//! Unix-domain socket listener — port of
//! `packages/server/src/transports/unix/listener.ts`.

use std::future::Future;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener as TokioUnixListener;

use crate::connection::{
    ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler, UnixByteConnection,
};

const DEFAULT_SOCKET_MODE: u32 = 0o600;
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;

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
    let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
        return Ok(());
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
    tokio::fs::remove_file(path)
        .await
        .map_err(|e| format!("remove stale socket: {e}"))?;
    Ok(())
}

async fn socket_is_live(path: &Path) -> Result<bool, String> {
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => Ok(true),
        Err(e) => {
            let code = e.raw_os_error().unwrap_or(0);
            Ok(code != 111 && code != 2 && code != 32 && code != 104)
        }
    }
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
    closing: bool,
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
            closing: false,
        })
    }

    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
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
        remove_stale_socket(&bind_path).await?;
        let listener = TokioUnixListener::bind(&bind_path).map_err(|e| format!("bind: {e}"))?;
        // Expose the public path via a symlink to the bind path.
        std::os::unix::fs::symlink(&bind_path, &self.path).map_err(|e| format!("link: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
        }

        let flag = tokio_util::sync::CancellationToken::new();
        let join = tokio::spawn(Self::accept_loop(listener, accept, flag.clone()));
        self.listener_keepalive = true;
        self.close_flag = Some(flag);
        self.join = Some(join);
        self.bound_path = Some(self.path.to_string_lossy().into_owned());
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
        self.listener_keepalive = false;
        let _ = tokio::fs::remove_file(&self.path).await;
        let _ = tokio::fs::remove_file(&self.private_bind_path()).await;
        self.bound_path = None;
        Ok(())
    }

    async fn accept_loop(
        listener: TokioUnixListener,
        accept: ByteConnectionAcceptor,
        flag: tokio_util::sync::CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = flag.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (read_half, write_half) = stream.into_split();
                    let connection = UnixByteConnection::from_parts(read_half, write_half);
                    let handler: Arc<Mutex<dyn ByteConnectionHandler>> = accept(connection.clone());
                    let Some(reader) = connection.take_reader() else {
                        continue;
                    };
                    tokio::spawn(read_loop(reader, handler));
                }
            }
        }
    }
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
) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        handler.lock().unwrap().on_data(&buf[..n], &handler);
    }
    handler.lock().unwrap().on_close();
}
