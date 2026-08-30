//! Ordered raw-output writes for machine modes and terminal takeovers.
//!
//! The TypeScript implementation temporarily redirects ordinary stdout while
//! the TUI owns the terminal and serializes raw writes so a slow pipe cannot
//! interleave JSONL/event records. Rust cannot replace `std::io::Stdout::write`
//! process-wide without unsafe global hooks, so this module makes the same
//! boundary explicit: callers use `write_raw_stdout` for bytes that must reach
//! stdout and hold an `OutputTakeover` while a terminal owner is active.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};

const RETRY_DELAY: Duration = Duration::from_millis(10);

fn write_lock() -> &'static Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
}

fn takeover_state() -> &'static Mutex<usize> {
    static STATE: OnceLock<Mutex<usize>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(0))
}

fn transient_write_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) || matches!(error.raw_os_error(), Some(11 | 55 | 105 | 10_055))
}

/// Write all bytes, retrying only transient backpressure errors.
pub async fn write_all_with_backpressure<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        match writer.write(&bytes[offset..]).await {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "output writer accepted zero bytes",
                ));
            }
            Ok(written) => offset += written,
            Err(error) if transient_write_error(&error) => tokio::time::sleep(RETRY_DELAY).await,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Serialize one raw stdout write with all other raw writes in this process.
pub async fn write_raw_stdout(text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let _guard = write_lock().lock().await;
    let mut stdout = tokio::io::stdout();
    write_all_with_backpressure(&mut stdout, text.as_bytes()).await?;
    stdout.flush().await
}

/// Wait until all raw stdout writes issued before this call have completed.
pub async fn wait_for_raw_stdout_backpressure() {
    let _guard = write_lock().lock().await;
}

/// Flush stdout after all ordered raw writes have settled.
pub async fn flush_raw_stdout() -> io::Result<()> {
    let _guard = write_lock().lock().await;
    tokio::io::stdout().flush().await
}

/// A scoped marker that a terminal/TUI owner has taken over stdout.
///
/// The marker is intentionally observable by tests and embedding callers; it
/// does not silently redirect or drop output. Dropping it always restores the
/// previous state, including during panic unwinding.
#[derive(Debug)]
pub struct OutputTakeover {
    active: bool,
}

impl OutputTakeover {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn acquire() -> Self {
        let mut state = takeover_state().lock().expect("output takeover lock");
        *state = state.saturating_add(1);
        Self { active: true }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn is_active() -> bool {
        *takeover_state().lock().expect("output takeover lock") > 0
    }
}

impl Drop for OutputTakeover {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = takeover_state().lock() {
            *state = state.saturating_sub(1);
        }
        self.active = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct TransientWriter {
        attempts: usize,
        output: Vec<u8>,
    }

    struct PartialWriter {
        max_bytes: usize,
        output: Vec<u8>,
    }

    struct ZeroWriter;

    struct FatalWriter;

    impl AsyncWrite for TransientWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.attempts < 2 {
                self.attempts += 1;
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "pipe is temporarily full",
                )));
            }
            self.output.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PartialWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let written = bytes.len().min(self.max_bytes);
            self.output.extend_from_slice(&bytes[..written]);
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for ZeroWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FatalWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stdout closed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn retries_transient_backpressure_and_preserves_bytes() {
        let mut writer = TransientWriter {
            attempts: 0,
            output: Vec::new(),
        };
        write_all_with_backpressure(&mut writer, b"one\ntwo\n")
            .await
            .unwrap();
        assert_eq!(writer.output, b"one\ntwo\n");
        assert_eq!(writer.attempts, 2);
    }

    #[tokio::test]
    async fn handles_partial_writes_without_dropping_the_tail() {
        let mut writer = PartialWriter {
            max_bytes: 2,
            output: Vec::new(),
        };
        write_all_with_backpressure(&mut writer, b"abcdef")
            .await
            .unwrap();
        assert_eq!(writer.output, b"abcdef");
    }

    #[tokio::test]
    async fn reports_write_zero_and_fatal_errors() {
        let mut zero = ZeroWriter;
        let error = write_all_with_backpressure(&mut zero, b"x")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);

        let mut fatal = FatalWriter;
        let error = write_all_with_backpressure(&mut fatal, b"x")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn retries_platform_buffer_errors() {
        for code in [11, 55, 105, 10_055] {
            assert!(transient_write_error(&io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn takeover_is_scoped_and_restored() {
        assert!(!OutputTakeover::is_active());
        let outer = OutputTakeover::acquire();
        assert!(OutputTakeover::is_active());
        let inner = OutputTakeover::acquire();
        assert!(OutputTakeover::is_active());
        // The marker is a depth count, so an out-of-order drop cannot expose
        // a false inactive state to a concurrent terminal owner.
        drop(outer);
        assert!(OutputTakeover::is_active());
        drop(inner);
        assert!(!OutputTakeover::is_active());
    }
}
